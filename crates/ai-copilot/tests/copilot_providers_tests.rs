#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use ai_copilot::{
    AiCopilotError, AiCopilotProvider, AiEndpointConfig, AiProviderKind, DefaultApiKeyLookup,
    HttpAiCopilotProvider, RangeSummaryRequest,
};
use branvio_core::domain::ConflictResolution;
use branvio_core::ids::GitOid;
use branvio_core::paths::RepoRelPath;
use compact_str::CompactString;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

async fn spawn_mock_ai_server(
    handler: impl Fn(&str, &HashMap<String, String>, &str) -> (u16, String, String)
    + Send
    + Sync
    + 'static,
) -> (String, Arc<Mutex<Vec<(String, HashMap<String, String>)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_ref = Arc::clone(&captured);
    let handler = Arc::new(handler);

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let h = Arc::clone(&handler);
            let cap = Arc::clone(&captured_ref);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 32 * 1024];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let raw = String::from_utf8_lossy(&buf[..n]);
                let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
                let mut lines = head.split("\r\n");
                let req_line = lines.next().unwrap_or("");
                let path = req_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let mut headers = HashMap::new();
                for line in lines {
                    if let Some((k, v)) = line.split_once(':') {
                        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                    }
                }
                cap.lock().unwrap().push((path.clone(), headers.clone()));
                let (status, ctype, resp_body) = h(&path, &headers, body);
                let resp = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                    resp_body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{}", addr.port()), captured)
}

#[tokio::test]
async fn ollama_commit_message_conflict_advice_and_streaming_summary() {
    let (base_url, _) = spawn_mock_ai_server(|path, _headers, body| {
        if path == "/api/chat" && body.contains("\"stream\":true") {
            let ndjson = concat!(
                "{\"message\":{\"content\":\"Highlights: \"},\"done\":false}\n",
                "{\"message\":{\"content\":\"added Phase 5.\\n\"},\"done\":false}\n",
                "{\"message\":{\"content\":\"\"},\"done\":true}\n"
            );
            (200, "application/x-ndjson".to_string(), ndjson.to_string())
        } else if path == "/api/chat" && body.contains("CONFLICT") {
            let json = serde_json::json!({
                "message": {
                    "content": "{\"strategy\":\"both_ours_first\",\"proposed_code\":\"a()\\nb()\\n\",\"rationale\":\"Preserves both helpers.\"}"
                }
            });
            (200, "application/json".to_string(), json.to_string())
        } else {
            let json = serde_json::json!({
                "message": {
                    "content": "```json\n{\"subject\":\"feat(ai): wire copilot client\",\"body\":\"Adds streaming support.\"}\n```"
                }
            });
            (200, "application/json".to_string(), json.to_string())
        }
    })
    .await;

    let provider = HttpAiCopilotProvider::new(None).unwrap();
    let cfg = AiEndpointConfig {
        kind: AiProviderKind::OllamaLocal,
        base_url,
        model_name: CompactString::const_new("qwen2.5-coder:14b"),
        custom_commit_style_hint: None,
        allow_cloud_upload: false,
    };

    let commit = provider
        .generate_commit_message(&cfg, "+ fn hello() {}", &[])
        .await
        .unwrap();
    assert_eq!(commit.subject, "feat(ai): wire copilot client");
    assert_eq!(commit.body.as_deref(), Some("Adds streaming support."));

    let advice = provider
        .suggest_conflict_resolution(
            &cfg,
            &RepoRelPath::normalize("src/lib.rs").unwrap(),
            Some("// CONFLICT base\n"),
            "a()\n",
            "b()\n",
        )
        .await
        .unwrap();
    assert_eq!(
        advice.suggested_strategy,
        ConflictResolution::TakeBothOursFirst
    );
    assert!(advice.proposed_code.contains("b()"));

    let (tx, mut rx) = mpsc::channel(16);
    provider
        .summarize_range_streaming(
            &cfg,
            RangeSummaryRequest {
                base_oid: GitOid::from_hex(&"a".repeat(40)).unwrap(),
                tip_oid: GitOid::from_hex(&"b".repeat(40)).unwrap(),
                commit_subjects: vec!["feat: p5".to_string()],
                truncated_diff_context: "+mod p5;".to_string(),
            },
            tx,
        )
        .await
        .unwrap();
    let mut collected = String::new();
    while let Some(tok) = rx.recv().await {
        collected.push_str(&tok);
    }
    assert_eq!(collected, "Highlights: added Phase 5.\n");
}

#[tokio::test]
async fn cloud_providers_require_key_and_stream_sse_cleanly() {
    let (base_url, recorded) = spawn_mock_ai_server(|path, _headers, _body| {
        if path.contains("streamGenerateContent") {
            let sse = concat!(
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Gemini \"}]}}]}\n\n",
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"OK\"}]}}]}\n\n"
            );
            (200, "text/event-stream".to_string(), sse.to_string())
        } else {
            let json = r#"{"choices":[{"message":{"content":"{\"subject\":\"fix: cloud\",\"body\":null}"}}]}"#;
            (200, "application/json".to_string(), json.to_string())
        }
    })
    .await;

    let keys = Arc::new(DefaultApiKeyLookup::new());
    let provider = HttpAiCopilotProvider::new(Some(keys.clone())).unwrap();

    let gemini_cfg = AiEndpointConfig {
        kind: AiProviderKind::GoogleGemini,
        base_url: base_url.clone(),
        model_name: CompactString::const_new("gemini-2.5-flash"),
        custom_commit_style_hint: None,
        allow_cloud_upload: true,
    };

    // Missing API key -> NotConfigured
    let (tx_missing, _rx_missing) = mpsc::channel(4);
    let err = provider
        .summarize_range_streaming(
            &gemini_cfg,
            RangeSummaryRequest {
                base_oid: GitOid::from_hex(&"a".repeat(40)).unwrap(),
                tip_oid: GitOid::from_hex(&"b".repeat(40)).unwrap(),
                commit_subjects: vec!["a".to_string()],
                truncated_diff_context: "".to_string(),
            },
            tx_missing,
        )
        .await;
    assert!(matches!(err, Err(AiCopilotError::NotConfigured(_))));

    keys.set_key(
        AiProviderKind::GoogleGemini,
        Some("gm_secret_xyz".to_string()),
    );
    let (tx, mut rx) = mpsc::channel(8);
    provider
        .summarize_range_streaming(
            &gemini_cfg,
            RangeSummaryRequest {
                base_oid: GitOid::from_hex(&"a".repeat(40)).unwrap(),
                tip_oid: GitOid::from_hex(&"b".repeat(40)).unwrap(),
                commit_subjects: vec!["a".to_string()],
                truncated_diff_context: "".to_string(),
            },
            tx,
        )
        .await
        .unwrap();
    let mut streamed = String::new();
    while let Some(tok) = rx.recv().await {
        streamed.push_str(&tok);
    }
    assert_eq!(streamed, "Gemini OK");

    // Verify API key passed cleanly via header, not leaked into request URL query params
    let calls = recorded.lock().unwrap();
    assert!(!calls[0].0.contains("gm_secret_xyz"));
    assert_eq!(
        calls[0].1.get("x-goog-api-key").map(String::as_str),
        Some("gm_secret_xyz")
    );
}
