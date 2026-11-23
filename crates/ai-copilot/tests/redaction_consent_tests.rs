#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use ai_copilot::{
    AiCopilotError, AiCopilotProvider, AiEndpointConfig, AiProviderKind, DefaultApiKeyLookup,
    HttpAiCopilotProvider, RangeSummaryRequest,
};
use branvio_core::ids::GitOid;
use branvio_core::paths::RepoRelPath;
use compact_str::CompactString;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// Mock HTTP server that reads each full request body (honouring `Content-Length`) and records it.
async fn spawn_recording_server(response_body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let bodies_ref = Arc::clone(&bodies);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let cap = Arc::clone(&bodies_ref);
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw);
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if body.len() >= len {
                            cap.lock().unwrap().push(body.to_string());
                            break;
                        }
                    }
                }
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{}", addr.port()), bodies)
}

fn cfg(kind: AiProviderKind, base_url: &str, allow_cloud_upload: bool) -> AiEndpointConfig {
    AiEndpointConfig {
        kind,
        base_url: base_url.to_string(),
        model_name: CompactString::const_new("test-model"),
        custom_commit_style_hint: None,
        allow_cloud_upload,
    }
}

fn range_request(diff: &str) -> RangeSummaryRequest {
    RangeSummaryRequest {
        base_oid: GitOid::from_hex(&"a".repeat(40)).unwrap(),
        tip_oid: GitOid::from_hex(&"b".repeat(40)).unwrap(),
        commit_subjects: vec!["feat: x".to_string()],
        truncated_diff_context: diff.to_string(),
    }
}

const SECRET_DIFF: &str = "\
diff --git a/src/config.rs b/src/config.rs
--- a/src/config.rs
+++ b/src/config.rs
@@ -1,2 +1,4 @@
+const GH: &str = \"ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789\";
+let password = \"hunter2-very-secret\";
+let token_count = 5;
diff --git a/.env b/.env
--- /dev/null
+++ b/.env
@@ -0,0 +1 @@
+STRIPE_LIVE=rk_live_should_never_leave_machine
";

#[tokio::test]
async fn cloud_providers_without_consent_fail_before_any_network_call() {
    // `.invalid` is guaranteed never to resolve; reaching the network would yield `Transport`.
    let keys = Arc::new(DefaultApiKeyLookup::new());
    keys.set_key(AiProviderKind::AnthropicClaude, Some("k".to_string()));
    keys.set_key(AiProviderKind::GoogleGemini, Some("k".to_string()));
    keys.set_key(AiProviderKind::OpenAiCompatible, Some("k".to_string()));
    let provider = HttpAiCopilotProvider::new(Some(keys)).unwrap();

    for (kind, label) in [
        (AiProviderKind::AnthropicClaude, "Anthropic"),
        (AiProviderKind::GoogleGemini, "Gemini"),
        (AiProviderKind::OpenAiCompatible, "OpenAI"),
        (AiProviderKind::OllamaLocal, "Ollama"),
    ] {
        let c = cfg(kind, "https://ai.invalid", false);
        let err = provider
            .generate_commit_message(&c, SECRET_DIFF, &[])
            .await
            .unwrap_err();
        match err {
            AiCopilotError::CloudUploadNotAllowed { provider } => assert_eq!(provider, label),
            other => panic!("expected CloudUploadNotAllowed for {kind:?}, got {other:?}"),
        }

        let (tx, _rx) = mpsc::channel(4);
        let err = provider
            .summarize_range_streaming(&c, range_request(SECRET_DIFF), tx)
            .await
            .unwrap_err();
        assert!(matches!(err, AiCopilotError::CloudUploadNotAllowed { .. }));

        let err = provider
            .suggest_conflict_resolution(
                &c,
                &RepoRelPath::normalize("src/lib.rs").unwrap(),
                None,
                "a",
                "b",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AiCopilotError::CloudUploadNotAllowed { .. }));
    }

    // Loopback look-alikes are still treated as off-device.
    for url in [
        "http://localhost.evil.invalid",
        "http://127.0.0.1@evil.invalid",
        "http://127.0.0.1.evil.invalid:11434",
    ] {
        let c = cfg(AiProviderKind::OpenAiCompatible, url, false);
        assert!(c.sends_off_device(), "{url}");
        let err = provider
            .generate_commit_message(&c, "+x", &[])
            .await
            .unwrap_err();
        assert!(
            matches!(err, AiCopilotError::CloudUploadNotAllowed { .. }),
            "{url}"
        );
    }
}

#[tokio::test]
async fn cloud_consent_allows_request_and_payload_is_masked() {
    let (base_url, bodies) = spawn_recording_server(
        r#"{"content":[{"text":"{\"subject\":\"feat: cfg\",\"body\":null}"}]}"#,
    )
    .await;
    let keys = Arc::new(DefaultApiKeyLookup::new());
    keys.set_key(AiProviderKind::AnthropicClaude, Some("k".to_string()));
    let provider = HttpAiCopilotProvider::new(Some(keys)).unwrap();

    // Loopback mock with consent granted (consent is irrelevant for loopback, masking is not).
    let c = cfg(AiProviderKind::AnthropicClaude, &base_url, true);
    let out = provider
        .generate_commit_message(
            &c,
            SECRET_DIFF,
            &["chore: rotate key AKIAIOSFODNN7EXAMPLE".to_string()],
        )
        .await
        .unwrap();
    assert_eq!(out.subject, "feat: cfg");

    let sent = bodies.lock().unwrap().join("\n");
    assert!(sent.contains("[REDACTED]"), "body: {sent}");
    assert!(!sent.contains("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"));
    assert!(!sent.contains("hunter2-very-secret"));
    assert!(!sent.contains("AKIAIOSFODNN7EXAMPLE"));
    assert!(!sent.contains("rk_live_should_never_leave_machine"));
    assert!(sent.contains("omitted diff of sensitive file: .env"));
    assert!(sent.contains("token_count = 5"));
}

#[tokio::test]
async fn local_ollama_works_without_consent_and_is_masked() {
    let (base_url, bodies) = spawn_recording_server(
        r#"{"message":{"content":"{\"subject\":\"fix: local\",\"body\":null}"}}"#,
    )
    .await;
    let provider = HttpAiCopilotProvider::new(None).unwrap();
    let c = cfg(AiProviderKind::OllamaLocal, &base_url, false);
    assert!(!c.sends_off_device());

    let out = provider
        .generate_commit_message(&c, SECRET_DIFF, &[])
        .await
        .unwrap();
    assert_eq!(out.subject, "fix: local");

    let sent = bodies.lock().unwrap().join("\n");
    assert!(sent.contains("[REDACTED]"));
    assert!(!sent.contains("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"));
    assert!(!sent.contains("rk_live_should_never_leave_machine"));
}

#[tokio::test]
async fn streaming_summary_payload_is_masked() {
    let (base_url, bodies) = spawn_recording_server("").await;
    let provider = HttpAiCopilotProvider::new(None).unwrap();
    let c = cfg(AiProviderKind::OllamaLocal, &base_url, false);
    let (tx, _rx) = mpsc::channel(4);
    provider
        .summarize_range_streaming(&c, range_request(SECRET_DIFF), tx)
        .await
        .unwrap();
    let sent = bodies.lock().unwrap().join("\n");
    assert!(sent.contains("[REDACTED]"));
    assert!(!sent.contains("hunter2-very-secret"));
    assert!(!sent.contains("rk_live_should_never_leave_machine"));
}

#[tokio::test]
async fn conflict_advice_refuses_sensitive_paths_and_masks_snippets() {
    let (base_url, bodies) = spawn_recording_server(
        r#"{"message":{"content":"{\"strategy\":\"ours\",\"proposed_code\":\"x\",\"rationale\":\"r\"}"}}"#,
    )
    .await;
    let provider = HttpAiCopilotProvider::new(None).unwrap();
    let c = cfg(AiProviderKind::OllamaLocal, &base_url, false);

    let err = provider
        .suggest_conflict_resolution(
            &c,
            &RepoRelPath::normalize("config/.env.production").unwrap(),
            None,
            "A=1",
            "A=2",
        )
        .await
        .unwrap_err();
    assert!(matches!(err, AiCopilotError::SensitivePathExcluded { .. }));
    assert!(bodies.lock().unwrap().is_empty());

    provider
        .suggest_conflict_resolution(
            &c,
            &RepoRelPath::normalize("src/lib.rs").unwrap(),
            None,
            "let api_key = \"sk-ant-api03-AbCdEf0123456789_xyz\";",
            "let x = 1;",
        )
        .await
        .unwrap();
    let sent = bodies.lock().unwrap().join("\n");
    assert!(sent.contains("[REDACTED]"));
    assert!(!sent.contains("sk-ant-api03-AbCdEf0123456789_xyz"));
}

#[test]
fn config_deserializes_without_consent_field_as_false() {
    let json = r#"{"kind":"AnthropicClaude","base_url":"https://api.anthropic.com","model_name":"claude-sonnet-5-5","custom_commit_style_hint":null}"#;
    let c: AiEndpointConfig = serde_json::from_str(json).unwrap();
    assert!(!c.allow_cloud_upload);
    assert!(c.sends_off_device());
}
