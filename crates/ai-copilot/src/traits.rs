//! Provider-agnostic traits for commit message generation, PR summarization, and conflict advice.

use async_trait::async_trait;
use branvio_core::domain::ConflictResolution;
use branvio_core::ids::GitOid;
use branvio_core::paths::RepoRelPath;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

/// Result alias for `ai-copilot`.
pub type AiCopilotResult<T> = Result<T, AiCopilotError>;

/// Errors emitted by local or cloud AI providers.
#[derive(Debug, Error)]
pub enum AiCopilotError {
    /// Network or HTTP error reaching endpoint.
    #[error("AI provider transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// Provider rejected credentials or quota.
    #[error("AI provider '{provider}' error ({status}): {message}")]
    ProviderError {
        /// Provider display label.
        provider: String,
        /// HTTP status code if applicable.
        status: u16,
        /// Error explanation.
        message: String,
    },

    /// Endpoint is disabled or missing API key.
    #[error("AI copilot is not configured: {0}")]
    NotConfigured(&'static str),

    /// The endpoint sends data off this machine and the caller did not grant explicit consent
    /// via [`AiEndpointConfig::allow_cloud_upload`]. No network request was made.
    #[error(
        "refusing to upload repository content to off-device AI provider '{provider}' without explicit consent"
    )]
    CloudUploadNotAllowed {
        /// Provider display label.
        provider: String,
    },

    /// The requested file is sensitive (credentials, keys, `.env`) and is never sent to a provider.
    #[error("refusing to send sensitive file '{path}' to an AI provider")]
    SensitivePathExcluded {
        /// Repository-relative path that was withheld.
        path: String,
    },
}

/// Supported LLM backends (local and Bring-Your-Own-Key).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AiProviderKind {
    /// Local Ollama HTTP API (`http://localhost:11434`).
    OllamaLocal,
    /// OpenAI or any OpenAI-compatible local/enterprise proxy (`vLLM`, `LM Studio`, `LiteLLM`).
    OpenAiCompatible,
    /// Anthropic Claude Messages API.
    AnthropicClaude,
    /// Google Gemini API.
    GoogleGemini,
}

/// Provider-agnostic endpoint configuration. API keys are resolved via `forge-client`'s `TokenVault`
/// or user settings and never stored in repository files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiEndpointConfig {
    /// Selected backend kind.
    pub kind: AiProviderKind,
    /// Endpoint base URL.
    pub base_url: String,
    /// Target model identifier (e.g., `qwen2.5-coder:14b`, `claude-sonnet-5-5`, `gemini-2.5-flash`).
    pub model_name: CompactString,
    /// Optional system prompt customization (e.g., enforce Conventional Commits scope conventions).
    pub custom_commit_style_hint: Option<String>,
    /// Explicit user consent to upload (secret-masked) diff/code excerpts to an endpoint that is
    /// not on this machine (any provider whose `base_url` is not `localhost`/`127.0.0.1`/`::1`).
    /// Defaults to `false`; off-device requests fail with
    /// [`AiCopilotError::CloudUploadNotAllowed`] until the user opts in.
    #[serde(default)]
    pub allow_cloud_upload: bool,
}

impl AiEndpointConfig {
    /// Returns `true` when requests to this endpoint leave the local machine (i.e. `base_url`
    /// does not resolve to a loopback host). Such endpoints require [`Self::allow_cloud_upload`].
    pub fn sends_off_device(&self) -> bool {
        !is_loopback_url(&self.base_url)
    }
}

/// Parses an endpoint URL strictly (WHATWG rules via the `url` crate) and returns its host.
///
/// Returns `None` (treated as "unknown / off-device" by callers) when the input is not a
/// well-formed absolute URL, carries userinfo (`user:pass@`), contains a backslash, whitespace or
/// control characters anywhere, or has no host. Rejecting these up front prevents parser
/// differentials such as `http://evil.com\@127.0.0.1/`, where a naive splitter sees
/// `127.0.0.1` but HTTP clients (WHATWG treats `\` as `/`) connect to `evil.com`.
pub(crate) fn parse_endpoint_host(raw: &str) -> Option<(String, url::Host<String>)> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|c| c == '\\' || c.is_whitespace() || c.is_control())
    {
        return None;
    }
    let parsed = url::Url::parse(trimmed).ok()?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    let host = parsed.host()?.to_owned();
    Some((parsed.scheme().to_ascii_lowercase(), host))
}

/// Returns `true` when the host of `url` is exactly `localhost` (case-insensitive, optional
/// trailing dot), an IPv4 loopback address (`127.0.0.0/8`), or the IPv6 loopback `::1`.
///
/// Anything that fails strict parsing (see [`parse_endpoint_host`]) is treated as off-device,
/// so look-alikes such as `http://localhost.evil.example`, `http://127.0.0.1@evil.example` or
/// `http://evil.example\@127.0.0.1/` are never considered local.
pub(crate) fn is_loopback_url(url: &str) -> bool {
    match parse_endpoint_host(url) {
        Some((_, url::Host::Domain(d))) => {
            let d = d.strip_suffix('.').unwrap_or(&d);
            d.eq_ignore_ascii_case("localhost")
        }
        Some((_, url::Host::Ipv4(v4))) => v4.is_loopback(),
        Some((_, url::Host::Ipv6(v6))) => v6 == std::net::Ipv6Addr::LOCALHOST,
        None => false,
    }
}

/// Suggested Conventional Commit draft generated from staged diffs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitMessageSuggestion {
    /// Subject line (`type(scope): description`).
    pub subject: String,
    /// Optional multi-line explanatory body.
    pub body: Option<String>,
}

/// Input range for summarizing multiple commits or a Pull Request diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeSummaryRequest {
    /// Base commit/branch OID.
    pub base_oid: GitOid,
    /// Tip commit/branch OID.
    pub tip_oid: GitOid,
    /// Commit subjects included in the range.
    pub commit_subjects: Vec<String>,
    /// Compact unified diff excerpt bounded to model context budget.
    pub truncated_diff_context: String,
}

/// Suggested resolution strategy plus concise explanation for a 3-way merge conflict region.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictResolutionAdvice {
    /// Recommended high-level strategy (`TakeOurs`, `TakeTheirs`, `TakeBothOursFirst`, or custom).
    pub suggested_strategy: ConflictResolution,
    /// Proposed merged code snippet when `suggested_strategy` is `ManualEdit` or custom merge.
    pub proposed_code: String,
    /// Concise 1-3 sentence explanation of why both edits interacted and what the merge preserves.
    pub rationale: String,
}

/// Provider-agnostic AI Copilot trait decoupled from UI and core Git engines.
#[async_trait]
pub trait AiCopilotProvider: Send + Sync {
    /// Generates a Conventional Commit message from the user's staged diff text.
    async fn generate_commit_message(
        &self,
        config: &AiEndpointConfig,
        staged_diff_excerpt: &str,
        recent_repo_subjects: &[String],
    ) -> AiCopilotResult<CommitMessageSuggestion>;

    /// Streams incremental markdown summary tokens for a commit range or Pull Request.
    async fn summarize_range_streaming(
        &self,
        config: &AiEndpointConfig,
        request: RangeSummaryRequest,
        token_tx: mpsc::Sender<String>,
    ) -> AiCopilotResult<()>;

    /// Recommends a safe resolution for a specific 3-way merge conflict hunk.
    async fn suggest_conflict_resolution(
        &self,
        config: &AiEndpointConfig,
        file_path: &RepoRelPath,
        base_snippet: Option<&str>,
        ours_snippet: &str,
        theirs_snippet: &str,
    ) -> AiCopilotResult<ConflictResolutionAdvice>;
}

#[cfg(test)]
mod tests {
    use super::is_loopback_url;

    #[test]
    fn loopback_hosts_are_recognised() {
        for url in [
            "http://localhost:11434",
            "http://LOCALHOST/",
            "http://localhost.:8080/v1",
            "http://127.0.0.1:11434",
            "http://127.1.2.3",
            "http://[::1]:8080/",
            "http://[0:0:0:0:0:0:0:1]/",
            "  http://localhost:1234  ",
        ] {
            assert!(is_loopback_url(url), "{url} should be loopback");
        }
    }

    #[test]
    fn look_alikes_and_malformed_urls_are_off_device() {
        for url in [
            "http://evil.com\\@127.0.0.1/",
            "http://evil.com\\@localhost/",
            "http://127.0.0.1\\@evil.com/",
            "http://127.0.0.1@evil.invalid",
            "http://user:pw@localhost:11434",
            "http://localhost@evil.invalid",
            "http://localhost.evil.invalid",
            "http://127.0.0.1.evil.invalid:11434",
            "http://local host/",
            "http://localhost\t/",
            "http://localhost/\u{0}",
            "localhost:11434",
            "127.0.0.1",
            "",
            "not a url",
            "https://api.openai.com",
            "http://[::2]/",
            "http://0.0.0.0/",
        ] {
            assert!(!is_loopback_url(url), "{url:?} must not be loopback");
        }
    }
}
