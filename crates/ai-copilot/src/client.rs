//! HTTP implementation of [`AiCopilotProvider`] supporting Ollama, OpenAI-compatible APIs,
//! Anthropic Claude, and Google Gemini (including streaming SSE/NDJSON summaries).
//!
//! Every prompt is passed through [`crate::redact`] before it leaves the process: sensitive file
//! sections are dropped from diffs and known secret formats are masked, for local and cloud
//! providers alike. Endpoints that are not on this machine additionally require
//! [`AiEndpointConfig::allow_cloud_upload`].

use crate::redact::{is_sensitive_path, mask_secrets, sanitize_diff};
use crate::traits::{
    AiCopilotError, AiCopilotProvider, AiCopilotResult, AiEndpointConfig, AiProviderKind,
    CommitMessageSuggestion, ConflictResolutionAdvice, RangeSummaryRequest, is_loopback_url,
    parse_endpoint_host,
};
use async_trait::async_trait;
use branvio_core::domain::ConflictResolution;
use branvio_core::paths::RepoRelPath;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;

/// Lookup source providing runtime API keys without embedding secrets in repository configs.
pub trait ApiKeyLookup: Send + Sync {
    /// Returns the secret API key for `(kind, base_url)`, if configured.
    fn resolve_api_key(&self, kind: AiProviderKind, base_url: &str) -> Option<String>;
}

/// Thread-safe API key store.
///
/// Resolution order for `(kind, base_url)`:
/// 1. a key registered for that exact endpoint origin via [`Self::set_endpoint_key`];
/// 2. a per-provider key registered via [`Self::set_key`];
/// 3. the standard environment variable (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`,
///    `GEMINI_API_KEY`).
///
/// Steps 2 and 3 apply **only** when `base_url` points at the provider's official HTTPS API host
/// (`api.openai.com`, `api.anthropic.com`, `generativelanguage.googleapis.com`) or at a loopback
/// address. A custom/proxy endpoint never receives a provider-wide key implicitly; it needs an
/// endpoint-specific key.
#[derive(Default)]
pub struct DefaultApiKeyLookup {
    overrides: RwLock<HashMap<AiProviderKind, String>>,
    endpoint_overrides: RwLock<HashMap<(AiProviderKind, String), String>>,
}

impl std::fmt::Debug for DefaultApiKeyLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut kinds: Vec<AiProviderKind> = self
            .overrides
            .read()
            .map(|g| g.keys().copied().collect())
            .unwrap_or_default();
        kinds.sort_by_key(|k| provider_label(*k));
        let endpoint_count = self.endpoint_overrides.read().map_or(0, |g| g.len());
        f.debug_struct("DefaultApiKeyLookup")
            .field("overridden_kinds", &kinds)
            .field("endpoint_key_count", &endpoint_count)
            .finish_non_exhaustive()
    }
}

/// Normalised `scheme://host[:port]` origin used as the endpoint-key map key, or `None` when the
/// URL fails strict parsing.
fn endpoint_origin(base_url: &str) -> Option<String> {
    parse_endpoint_host(base_url)?;
    let parsed = url::Url::parse(base_url.trim()).ok()?;
    Some(parsed.origin().ascii_serialization())
}

/// `true` when `base_url` is the provider's official HTTPS API host or a loopback address, i.e.
/// an endpoint that may receive the provider-wide key / environment variable key.
fn endpoint_may_use_default_key(kind: AiProviderKind, base_url: &str) -> bool {
    if is_loopback_url(base_url) {
        return true;
    }
    let official = match kind {
        AiProviderKind::OllamaLocal => return false,
        AiProviderKind::OpenAiCompatible => "api.openai.com",
        AiProviderKind::AnthropicClaude => "api.anthropic.com",
        AiProviderKind::GoogleGemini => "generativelanguage.googleapis.com",
    };
    match parse_endpoint_host(base_url) {
        Some((scheme, url::Host::Domain(d))) => {
            let d = d.strip_suffix('.').unwrap_or(&d);
            scheme == "https" && d.eq_ignore_ascii_case(official)
        }
        _ => false,
    }
}

impl DefaultApiKeyLookup {
    /// Constructs an empty key store using process environment fallback when needed.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores or clears an in-memory per-provider API key override for `kind`. It is only used
    /// for the provider's official API host or loopback endpoints (see the type docs).
    pub fn set_key(&self, kind: AiProviderKind, key: Option<String>) {
        if let Ok(mut guard) = self.overrides.write() {
            match key {
                Some(k) if !k.trim().is_empty() => {
                    guard.insert(kind, k);
                }
                _ => {
                    guard.remove(&kind);
                }
            }
        }
    }

    /// Stores or clears an API key that is sent only to the endpoint whose origin
    /// (`scheme://host:port`) matches `base_url`. Required for custom / proxy endpoints.
    /// Invalid URLs are ignored.
    pub fn set_endpoint_key(&self, kind: AiProviderKind, base_url: &str, key: Option<String>) {
        let Some(origin) = endpoint_origin(base_url) else {
            return;
        };
        if let Ok(mut guard) = self.endpoint_overrides.write() {
            match key {
                Some(k) if !k.trim().is_empty() => {
                    guard.insert((kind, origin), k);
                }
                _ => {
                    guard.remove(&(kind, origin));
                }
            }
        }
    }
}

impl ApiKeyLookup for DefaultApiKeyLookup {
    fn resolve_api_key(&self, kind: AiProviderKind, base_url: &str) -> Option<String> {
        if let Some(origin) = endpoint_origin(base_url)
            && let Ok(guard) = self.endpoint_overrides.read()
            && let Some(k) = guard.get(&(kind, origin))
        {
            return Some(k.clone());
        }
        if !endpoint_may_use_default_key(kind, base_url) {
            return None;
        }
        if let Ok(guard) = self.overrides.read()
            && let Some(k) = guard.get(&kind)
        {
            return Some(k.clone());
        }
        let env_var = match kind {
            AiProviderKind::OllamaLocal => return None,
            AiProviderKind::OpenAiCompatible => "OPENAI_API_KEY",
            AiProviderKind::AnthropicClaude => "ANTHROPIC_API_KEY",
            AiProviderKind::GoogleGemini => "GEMINI_API_KEY",
        };
        std::env::var(env_var).ok().filter(|v| !v.trim().is_empty())
    }
}

/// Unified HTTP client implementing [`AiCopilotProvider`] for all supported local and cloud LLMs.
pub struct HttpAiCopilotProvider {
    client: Client,
    key_lookup: Arc<dyn ApiKeyLookup>,
}

impl HttpAiCopilotProvider {
    /// Creates an [`HttpAiCopilotProvider`] using `key_lookup` (or [`DefaultApiKeyLookup`] if omitted).
    pub fn new(key_lookup: Option<Arc<dyn ApiKeyLookup>>) -> AiCopilotResult<Self> {
        let client = Client::builder()
            .user_agent("Branvio-Copilot/0.1.0")
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            client,
            key_lookup: key_lookup.unwrap_or_else(|| Arc::new(DefaultApiKeyLookup::new())),
        })
    }

    /// Refuses off-device endpoints unless the caller explicitly granted upload consent.
    fn ensure_upload_allowed(config: &AiEndpointConfig) -> AiCopilotResult<()> {
        if config.sends_off_device() && !config.allow_cloud_upload {
            return Err(AiCopilotError::CloudUploadNotAllowed {
                provider: provider_label(config.kind).to_string(),
            });
        }
        Ok(())
    }

    fn require_key_if_cloud(&self, config: &AiEndpointConfig) -> AiCopilotResult<Option<String>> {
        let key = self
            .key_lookup
            .resolve_api_key(config.kind, &config.base_url);
        let is_local = is_loopback_url(&config.base_url);
        match config.kind {
            AiProviderKind::OllamaLocal => Ok(key),
            AiProviderKind::OpenAiCompatible if is_local => Ok(key),
            AiProviderKind::OpenAiCompatible
            | AiProviderKind::AnthropicClaude
            | AiProviderKind::GoogleGemini => {
                if let Some(k) = key {
                    Ok(Some(k))
                } else {
                    Err(AiCopilotError::NotConfigured(
                        "missing API key for cloud AI provider",
                    ))
                }
            }
        }
    }

    async fn generate_text(
        &self,
        config: &AiEndpointConfig,
        system_prompt: &str,
        user_prompt: &str,
    ) -> AiCopilotResult<String> {
        let base = config.base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return Err(AiCopilotError::NotConfigured("endpoint base_url is empty"));
        }
        Self::ensure_upload_allowed(config)?;
        let api_key = self.require_key_if_cloud(config)?;
        let provider_name = provider_label(config.kind);
        let system_masked = mask_secrets(system_prompt).text;
        let user_masked = mask_secrets(user_prompt).text;
        let system_prompt = system_masked.as_str();
        let user_prompt = user_masked.as_str();

        match config.kind {
            AiProviderKind::OllamaLocal => {
                let url = format!("{base}/api/chat");
                let body = serde_json::json!({
                    "model": config.model_name.as_str(),
                    "stream": false,
                    "messages": [
                        { "role": "system", "content": system_prompt },
                        { "role": "user", "content": user_prompt }
                    ]
                });
                let resp = self.client.post(url).json(&body).send().await?;
                let raw = read_checked_response(provider_name, resp).await?;
                extract_ollama_text(&raw, provider_name)
            }
            AiProviderKind::OpenAiCompatible => {
                let url = format!("{base}/v1/chat/completions");
                let body = serde_json::json!({
                    "model": config.model_name.as_str(),
                    "messages": [
                        { "role": "system", "content": system_prompt },
                        { "role": "user", "content": user_prompt }
                    ]
                });
                let mut req = self.client.post(url).json(&body);
                if let Some(k) = api_key {
                    req = req.bearer_auth(k);
                }
                let resp = req.send().await?;
                let raw = read_checked_response(provider_name, resp).await?;
                extract_openai_text(&raw, provider_name)
            }
            AiProviderKind::AnthropicClaude => {
                let url = format!("{base}/v1/messages");
                let body = serde_json::json!({
                    "model": config.model_name.as_str(),
                    "max_tokens": 1024,
                    "system": system_prompt,
                    "messages": [{ "role": "user", "content": user_prompt }]
                });
                let mut req = self
                    .client
                    .post(url)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body);
                if let Some(k) = api_key {
                    req = req.header("x-api-key", k);
                }
                let resp = req.send().await?;
                let raw = read_checked_response(provider_name, resp).await?;
                extract_anthropic_text(&raw, provider_name)
            }
            AiProviderKind::GoogleGemini => {
                let url = format!(
                    "{base}/v1beta/models/{}:generateContent",
                    config.model_name.as_str()
                );
                let body = serde_json::json!({
                    "systemInstruction": { "parts": [{ "text": system_prompt }] },
                    "contents": [{ "role": "user", "parts": [{ "text": user_prompt }] }]
                });
                let mut req = self.client.post(url).json(&body);
                if let Some(k) = api_key {
                    req = req.header("x-goog-api-key", k);
                }
                let resp = req.send().await?;
                let raw = read_checked_response(provider_name, resp).await?;
                extract_gemini_text(&raw, provider_name)
            }
        }
    }
}

#[async_trait]
impl AiCopilotProvider for HttpAiCopilotProvider {
    async fn generate_commit_message(
        &self,
        config: &AiEndpointConfig,
        staged_diff_excerpt: &str,
        recent_repo_subjects: &[String],
    ) -> AiCopilotResult<CommitMessageSuggestion> {
        let style_hint = config
            .custom_commit_style_hint
            .as_deref()
            .unwrap_or("Use concise Conventional Commits (feat/fix/refactor/docs/chore/test).");
        let system = format!(
            "You generate accurate Git commit messages. {style_hint} \
             Respond strictly with JSON matching {{\"subject\": string, \"body\": string|null}}."
        );
        let recent = recent_repo_subjects.join("\n- ");
        let sanitized_diff = sanitize_diff(staged_diff_excerpt).text;
        let bounded_diff = truncate_chars(&sanitized_diff, 12_000);
        let user = format!(
            "Recent commit subjects:\n- {recent}\n\nStaged diff:\n```diff\n{bounded_diff}\n```"
        );
        let text = self.generate_text(config, &system, &user).await?;
        Ok(parse_commit_suggestion(&text))
    }

    async fn summarize_range_streaming(
        &self,
        config: &AiEndpointConfig,
        request: RangeSummaryRequest,
        token_tx: mpsc::Sender<String>,
    ) -> AiCopilotResult<()> {
        let base = config.base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return Err(AiCopilotError::NotConfigured("endpoint base_url is empty"));
        }
        Self::ensure_upload_allowed(config)?;
        let api_key = self.require_key_if_cloud(config)?;
        let provider_name = provider_label(config.kind);
        let sanitized_diff = sanitize_diff(&request.truncated_diff_context).text;
        let system = "Summarize Git commit ranges cleanly in concise Markdown sections: Highlights, Behavioral Changes, Risks.";
        let user = mask_secrets(&format!(
            "Range {}..{}\nCommits:\n- {}\n\nDiff excerpt:\n```diff\n{}\n```",
            request.base_oid.short_hex(8),
            request.tip_oid.short_hex(8),
            request.commit_subjects.join("\n- "),
            truncate_chars(&sanitized_diff, 14_000),
        ))
        .text;

        let mut resp = match config.kind {
            AiProviderKind::OllamaLocal => {
                let url = format!("{base}/api/chat");
                let body = serde_json::json!({
                    "model": config.model_name.as_str(),
                    "stream": true,
                    "messages": [
                        { "role": "system", "content": system },
                        { "role": "user", "content": user }
                    ]
                });
                self.client.post(url).json(&body).send().await?
            }
            AiProviderKind::OpenAiCompatible => {
                let url = format!("{base}/v1/chat/completions");
                let body = serde_json::json!({
                    "model": config.model_name.as_str(),
                    "stream": true,
                    "messages": [
                        { "role": "system", "content": system },
                        { "role": "user", "content": user }
                    ]
                });
                let mut req = self.client.post(url).json(&body);
                if let Some(k) = api_key {
                    req = req.bearer_auth(k);
                }
                req.send().await?
            }
            AiProviderKind::AnthropicClaude => {
                let url = format!("{base}/v1/messages");
                let body = serde_json::json!({
                    "model": config.model_name.as_str(),
                    "max_tokens": 1024,
                    "stream": true,
                    "system": system,
                    "messages": [{ "role": "user", "content": user }]
                });
                let mut req = self
                    .client
                    .post(url)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body);
                if let Some(k) = api_key {
                    req = req.header("x-api-key", k);
                }
                req.send().await?
            }
            AiProviderKind::GoogleGemini => {
                let url = format!(
                    "{base}/v1beta/models/{}:streamGenerateContent?alt=sse",
                    config.model_name.as_str()
                );
                let body = serde_json::json!({
                    "systemInstruction": { "parts": [{ "text": system }] },
                    "contents": [{ "role": "user", "parts": [{ "text": user }] }]
                });
                let mut req = self.client.post(url).json(&body);
                if let Some(k) = api_key {
                    req = req.header("x-goog-api-key", k);
                }
                req.send().await?
            }
        };

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let err_body = resp.text().await.unwrap_or_default();
            return Err(AiCopilotError::ProviderError {
                provider: provider_name.to_string(),
                status,
                message: truncate_chars(&err_body, 240).to_string(),
            });
        }

        let mut line_buf = String::new();
        while let Some(chunk) = resp.chunk().await? {
            line_buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = line_buf.find('\n') {
                let line = line_buf[..pos].trim().to_string();
                line_buf.drain(..=pos);
                if let Some(token) = parse_stream_line(config.kind, &line)
                    && token_tx.send(token).await.is_err()
                {
                    return Ok(());
                }
            }
        }
        let trailing = line_buf.trim();
        if !trailing.is_empty()
            && let Some(token) = parse_stream_line(config.kind, trailing)
        {
            let _ = token_tx.send(token).await;
        }
        Ok(())
    }

    async fn suggest_conflict_resolution(
        &self,
        config: &AiEndpointConfig,
        file_path: &RepoRelPath,
        base_snippet: Option<&str>,
        ours_snippet: &str,
        theirs_snippet: &str,
    ) -> AiCopilotResult<ConflictResolutionAdvice> {
        if is_sensitive_path(file_path.as_str()) {
            return Err(AiCopilotError::SensitivePathExcluded {
                path: file_path.as_str().to_string(),
            });
        }
        // Snippets are masked centrally in `generate_text`; masking before truncation keeps
        // secrets that straddle the 4k cut-off recognisable.
        let base_masked = base_snippet.map(|b| mask_secrets(b).text);
        let ours_masked = mask_secrets(ours_snippet).text;
        let theirs_masked = mask_secrets(theirs_snippet).text;
        let system = "You resolve 3-way Git merge conflicts cleanly. Reply strictly with JSON: \
                      {\"strategy\": \"ours\"|\"theirs\"|\"both_ours_first\"|\"both_theirs_first\"|\"manual\", \
                       \"proposed_code\": string, \"rationale\": string}.";
        let user = format!(
            "File: {}\n\nBASE:\n```\n{}\n```\n\nOURS:\n```\n{}\n```\n\nTHEIRS:\n```\n{}\n```",
            file_path.as_str(),
            truncate_chars(base_masked.as_deref().unwrap_or("<none>"), 4_000),
            truncate_chars(&ours_masked, 4_000),
            truncate_chars(&theirs_masked, 4_000),
        );
        let text = self.generate_text(config, system, &user).await?;
        Ok(parse_conflict_advice(&text, ours_snippet))
    }
}

fn provider_label(kind: AiProviderKind) -> &'static str {
    match kind {
        AiProviderKind::OllamaLocal => "Ollama",
        AiProviderKind::OpenAiCompatible => "OpenAI",
        AiProviderKind::AnthropicClaude => "Anthropic",
        AiProviderKind::GoogleGemini => "Gemini",
    }
}

fn truncate_chars(s: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

async fn read_checked_response(provider: &str, resp: reqwest::Response) -> AiCopilotResult<String> {
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(AiCopilotError::ProviderError {
            provider: provider.to_string(),
            status: status.as_u16(),
            message: truncate_chars(&text, 240).to_string(),
        });
    }
    Ok(text)
}

#[derive(Deserialize)]
struct OllamaResp {
    message: Option<OllamaMsg>,
    response: Option<String>,
}
#[derive(Deserialize)]
struct OllamaMsg {
    content: String,
}

fn extract_ollama_text(raw: &str, provider: &str) -> AiCopilotResult<String> {
    let parsed: OllamaResp =
        serde_json::from_str(raw).map_err(|e| AiCopilotError::ProviderError {
            provider: provider.to_string(),
            status: 200,
            message: format!("invalid Ollama response: {e}"),
        })?;
    Ok(parsed
        .message
        .map(|m| m.content)
        .or(parsed.response)
        .unwrap_or_default())
}

#[derive(Deserialize)]
struct OpenAiResp {
    choices: Vec<OpenAiChoice>,
}
#[derive(Deserialize)]
struct OpenAiChoice {
    message: Option<OpenAiMessage>,
    delta: Option<OpenAiDelta>,
}
#[derive(Deserialize)]
struct OpenAiMessage {
    content: Option<String>,
}
#[derive(Deserialize)]
struct OpenAiDelta {
    content: Option<String>,
}

fn extract_openai_text(raw: &str, provider: &str) -> AiCopilotResult<String> {
    let parsed: OpenAiResp =
        serde_json::from_str(raw).map_err(|e| AiCopilotError::ProviderError {
            provider: provider.to_string(),
            status: 200,
            message: format!("invalid OpenAI response: {e}"),
        })?;
    Ok(parsed
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.and_then(|m| m.content))
        .unwrap_or_default())
}

#[derive(Deserialize)]
struct AnthropicResp {
    content: Vec<AnthropicBlock>,
}
#[derive(Deserialize)]
struct AnthropicBlock {
    text: Option<String>,
}

fn extract_anthropic_text(raw: &str, provider: &str) -> AiCopilotResult<String> {
    let parsed: AnthropicResp =
        serde_json::from_str(raw).map_err(|e| AiCopilotError::ProviderError {
            provider: provider.to_string(),
            status: 200,
            message: format!("invalid Anthropic response: {e}"),
        })?;
    Ok(parsed
        .content
        .into_iter()
        .filter_map(|b| b.text)
        .collect::<Vec<_>>()
        .join(""))
}

#[derive(Deserialize)]
struct GeminiResp {
    candidates: Option<Vec<GeminiCandidate>>,
}
#[derive(Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiContent>,
}
#[derive(Deserialize)]
struct GeminiContent {
    parts: Option<Vec<GeminiPart>>,
}
#[derive(Deserialize)]
struct GeminiPart {
    text: Option<String>,
}

fn extract_gemini_text(raw: &str, provider: &str) -> AiCopilotResult<String> {
    let parsed: GeminiResp =
        serde_json::from_str(raw).map_err(|e| AiCopilotError::ProviderError {
            provider: provider.to_string(),
            status: 200,
            message: format!("invalid Gemini response: {e}"),
        })?;
    Ok(parsed
        .candidates
        .unwrap_or_default()
        .into_iter()
        .next()
        .and_then(|c| c.content)
        .and_then(|c| c.parts)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| p.text)
        .collect::<Vec<_>>()
        .join(""))
}

#[derive(Deserialize)]
struct AnthropicStreamEvent {
    delta: Option<AnthropicDelta>,
}
#[derive(Deserialize)]
struct AnthropicDelta {
    text: Option<String>,
}

fn parse_stream_line(kind: AiProviderKind, line: &str) -> Option<String> {
    if line.is_empty() || line == "data: [DONE]" {
        return None;
    }
    let payload = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
    if payload == "[DONE]" || payload.is_empty() {
        return None;
    }
    match kind {
        AiProviderKind::OllamaLocal => {
            let obj: OllamaResp = serde_json::from_str(payload).ok()?;
            let text = obj.message.map(|m| m.content).or(obj.response)?;
            (!text.is_empty()).then_some(text)
        }
        AiProviderKind::OpenAiCompatible => {
            let obj: OpenAiResp = serde_json::from_str(payload).ok()?;
            let text = obj
                .choices
                .into_iter()
                .next()
                .and_then(|c| c.delta.and_then(|d| d.content))?;
            (!text.is_empty()).then_some(text)
        }
        AiProviderKind::AnthropicClaude => {
            let obj: AnthropicStreamEvent = serde_json::from_str(payload).ok()?;
            let text = obj.delta.and_then(|d| d.text)?;
            (!text.is_empty()).then_some(text)
        }
        AiProviderKind::GoogleGemini => {
            let obj: GeminiResp = serde_json::from_str(payload).ok()?;
            let text: String = obj
                .candidates
                .unwrap_or_default()
                .into_iter()
                .next()
                .and_then(|c| c.content)
                .and_then(|c| c.parts)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|p| p.text)
                .collect();
            (!text.is_empty()).then_some(text)
        }
    }
}

fn strip_markdown_json_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        let after_lang = rest.split_once('\n').map_or(rest, |(_lang, body)| body);
        after_lang
            .rsplit_once("```")
            .map_or(after_lang, |(json, _tail)| json)
            .trim()
    } else {
        trimmed
    }
}

#[derive(Deserialize)]
struct CommitSuggestionJson {
    subject: String,
    body: Option<String>,
}

fn parse_commit_suggestion(raw: &str) -> CommitMessageSuggestion {
    let json_slice = strip_markdown_json_fence(raw);
    if let Ok(parsed) = serde_json::from_str::<CommitSuggestionJson>(json_slice) {
        return CommitMessageSuggestion {
            subject: parsed.subject.trim().to_string(),
            body: parsed.body.and_then(|b| {
                let t = b.trim().to_string();
                (!t.is_empty()).then_some(t)
            }),
        };
    }
    let mut lines = raw.trim().lines();
    let subject = lines.next().unwrap_or("chore: update").trim().to_string();
    let body_rest: String = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    CommitMessageSuggestion {
        subject,
        body: (!body_rest.is_empty()).then_some(body_rest),
    }
}

#[derive(Deserialize)]
struct ConflictAdviceJson {
    strategy: String,
    proposed_code: Option<String>,
    rationale: Option<String>,
}

fn parse_conflict_advice(raw: &str, fallback_code: &str) -> ConflictResolutionAdvice {
    let json_slice = strip_markdown_json_fence(raw);
    if let Ok(parsed) = serde_json::from_str::<ConflictAdviceJson>(json_slice) {
        let suggested_strategy = match parsed.strategy.trim().to_ascii_lowercase().as_str() {
            "ours" | "takeours" | "take_ours" => ConflictResolution::TakeOurs,
            "theirs" | "taketheirs" | "take_theirs" => ConflictResolution::TakeTheirs,
            "both_ours_first" | "takebothoursfirst" | "both" => {
                ConflictResolution::TakeBothOursFirst
            }
            "both_theirs_first" | "takeboththeirsfirst" => ConflictResolution::TakeBothTheirsFirst,
            _ => ConflictResolution::ManualEdit,
        };
        return ConflictResolutionAdvice {
            suggested_strategy,
            proposed_code: parsed
                .proposed_code
                .unwrap_or_else(|| fallback_code.to_string()),
            rationale: parsed.rationale.unwrap_or_else(|| {
                "Combined resolution preserving compatible semantics.".to_string()
            }),
        };
    }
    ConflictResolutionAdvice {
        suggested_strategy: ConflictResolution::ManualEdit,
        proposed_code: raw.trim().to_string(),
        rationale: "AI returned freeform resolution snippet.".to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_keys() {
        let keys = DefaultApiKeyLookup::new();
        keys.set_key(
            AiProviderKind::AnthropicClaude,
            Some("sk-ant-super-secret".into()),
        );
        keys.set_endpoint_key(
            AiProviderKind::OpenAiCompatible,
            "https://proxy.example.com/v1",
            Some("proxy-secret-key".into()),
        );
        let dbg = format!("{keys:?}");
        assert!(!dbg.contains("super-secret"), "{dbg}");
        assert!(!dbg.contains("proxy-secret-key"), "{dbg}");
        assert!(dbg.contains("AnthropicClaude"), "{dbg}");
    }

    #[test]
    fn provider_wide_key_only_reaches_official_or_loopback_hosts() {
        let keys = DefaultApiKeyLookup::new();
        keys.set_key(AiProviderKind::OpenAiCompatible, Some("openai-key".into()));
        keys.set_key(
            AiProviderKind::AnthropicClaude,
            Some("anthropic-key".into()),
        );
        keys.set_key(AiProviderKind::GoogleGemini, Some("gemini-key".into()));

        let get = |kind, url: &str| keys.resolve_api_key(kind, url);
        assert_eq!(
            get(AiProviderKind::OpenAiCompatible, "https://api.openai.com"),
            Some("openai-key".into())
        );
        assert_eq!(
            get(
                AiProviderKind::AnthropicClaude,
                "https://API.anthropic.com./"
            ),
            Some("anthropic-key".into())
        );
        assert_eq!(
            get(
                AiProviderKind::GoogleGemini,
                "https://generativelanguage.googleapis.com"
            ),
            Some("gemini-key".into())
        );
        assert_eq!(
            get(AiProviderKind::OpenAiCompatible, "http://127.0.0.1:8000"),
            Some("openai-key".into())
        );

        for (kind, url) in [
            (
                AiProviderKind::OpenAiCompatible,
                "https://proxy.example.com",
            ),
            (AiProviderKind::OpenAiCompatible, "http://api.openai.com"),
            (
                AiProviderKind::OpenAiCompatible,
                "https://api.openai.com.evil.io",
            ),
            (
                AiProviderKind::OpenAiCompatible,
                "https://evil.io\\@api.openai.com",
            ),
            (AiProviderKind::AnthropicClaude, "https://api.openai.com"),
            (AiProviderKind::GoogleGemini, "https://gemini.proxy.example"),
        ] {
            assert_eq!(get(kind, url), None, "{kind:?} {url}");
        }
    }

    #[test]
    fn endpoint_specific_key_is_used_for_custom_hosts_only() {
        let keys = DefaultApiKeyLookup::new();
        let kind = AiProviderKind::OpenAiCompatible;
        keys.set_endpoint_key(
            kind,
            "https://proxy.example.com/v1",
            Some("proxy-key".into()),
        );
        assert_eq!(
            keys.resolve_api_key(kind, "https://proxy.example.com"),
            Some("proxy-key".into())
        );
        assert_eq!(
            keys.resolve_api_key(kind, "https://proxy.example.com:8443"),
            None
        );
        assert_eq!(
            keys.resolve_api_key(AiProviderKind::AnthropicClaude, "https://proxy.example.com"),
            None
        );
        keys.set_endpoint_key(kind, "https://proxy.example.com", None);
        assert_eq!(
            keys.resolve_api_key(kind, "https://proxy.example.com"),
            None
        );
    }
}
