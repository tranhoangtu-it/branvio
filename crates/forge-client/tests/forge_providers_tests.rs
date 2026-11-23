#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use compact_str::CompactString;
use forge_client::{
    CiCheckStatus, CreatePullRequestInput, ForgeError, ForgeKind, ForgeProvider, ForgeRemoteTarget,
    InMemoryTokenVault, PullRequestState, TokenVault, UnifiedForgeProvider, parse_remote_url,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn spawn_mock_http_server(
    handler: impl Fn(&str, &str, &HashMap<String, String>) -> (u16, Vec<(String, String)>, String)
    + Send
    + Sync
    + 'static,
) -> (String, Arc<Mutex<Vec<(String, HashMap<String, String>)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let recorded_clone = Arc::clone(&recorded);
    let handler = Arc::new(handler);

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let h = Arc::clone(&handler);
            let rec = Arc::clone(&recorded_clone);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let raw = String::from_utf8_lossy(&buf[..n]);
                let mut lines = raw.split("\r\n");
                let req_line = lines.next().unwrap_or("");
                let mut parts = req_line.split_whitespace();
                let method = parts.next().unwrap_or("GET").to_string();
                let path = parts.next().unwrap_or("/").to_string();

                let mut headers = HashMap::new();
                for line in lines {
                    if line.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                    }
                }
                rec.lock()
                    .unwrap()
                    .push((format!("{method} {path}"), headers.clone()));

                let (status, extra_headers, body) = h(&method, &path, &headers);
                let reason = match status {
                    200 => "OK",
                    201 => "Created",
                    401 => "Unauthorized",
                    429 => "Too Many Requests",
                    _ => "Error",
                };
                let mut resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (hk, hv) in extra_headers {
                    resp.push_str(&format!("{hk}: {hv}\r\n"));
                }
                resp.push_str("\r\n");
                resp.push_str(&body);
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });

    (format!("http://127.0.0.1:{}", addr.port()), recorded)
}

#[test]
fn parses_ssh_https_and_nested_gitlab_remote_urls() {
    let gh = parse_remote_url("git@github.com:branvio-org/branvio.git", None).unwrap();
    assert_eq!(gh.kind, ForgeKind::GitHub);
    assert_eq!(gh.api_base_url, "https://api.github.com");
    assert_eq!(gh.owner.as_str(), "branvio-org");
    assert_eq!(gh.repo_name.as_str(), "branvio");

    let gl = parse_remote_url(
        "https://oauth2:secret@gitlab.company.internal/platform/team-a/core-app.git",
        Some(ForgeKind::GitLab),
    )
    .unwrap();
    assert_eq!(gl.kind, ForgeKind::GitLab);
    assert_eq!(gl.api_base_url, "https://gitlab.company.internal/api/v4");
    assert!(!gl.api_base_url.contains("secret"));
    assert_eq!(gl.owner.as_str(), "platform/team-a");
    assert_eq!(gl.repo_name.as_str(), "core-app");

    let bb = parse_remote_url("ssh://git@bitbucket.org/acme/widget", None).unwrap();
    assert_eq!(bb.kind, ForgeKind::Bitbucket);
    assert_eq!(bb.api_base_url, "https://api.bitbucket.org/2.0");
}

#[tokio::test]
async fn unified_provider_lists_prs_checks_creates_pr_and_maps_auth_and_rate_limits() {
    let (base_url, recorded) = spawn_mock_http_server(|method, path, _headers| {
        if method == "GET" && path == "/repos/acme/branvio/pulls?state=open&per_page=50" {
            (
                200,
                Vec::new(),
                r#"[{"number":42,"title":"feat: phase 5","body":"Ready","state":"open","draft":false,"merged_at":null,"html_url":"https://github.com/acme/branvio/pull/42","user":{"login":"alice"},"head":{"ref":"feat/p5","sha":"0123456789abcdef0123456789abcdef01234567"},"base":{"ref":"main","sha":"fedcba9876543210fedcba9876543210fedcba98"}}]"#.to_string(),
            )
        } else if method == "GET" && path == "/repos/acme/branvio/pulls/42" {
            (
                200,
                Vec::new(),
                r#"{"number":42,"title":"feat: phase 5","body":"Ready","state":"open","draft":false,"merged_at":null,"html_url":"https://github.com/acme/branvio/pull/42","user":{"login":"alice"},"head":{"ref":"feat/p5","sha":"0123456789abcdef0123456789abcdef01234567"},"base":{"ref":"main","sha":"fedcba9876543210fedcba9876543210fedcba98"}}"#.to_string(),
            )
        } else if method == "GET"
            && path == "/repos/acme/branvio/commits/0123456789abcdef0123456789abcdef01234567/check-runs"
        {
            (
                200,
                Vec::new(),
                r#"{"check_runs":[{"name":"cargo clippy","status":"completed","conclusion":"success","html_url":"https://ci/1"}]}"#.to_string(),
            )
        } else if method == "POST" && path == "/repos/acme/branvio/pulls" {
            (
                201,
                Vec::new(),
                r#"{"number":43,"title":"New PR","body":"Body","state":"open","draft":true,"merged_at":null,"html_url":"https://github.com/acme/branvio/pull/43","user":{"login":"alice"},"head":{"ref":"feat/new","sha":"1111111111111111111111111111111111111111"},"base":{"ref":"main","sha":"fedcba9876543210fedcba9876543210fedcba98"}}"#.to_string(),
            )
        } else if path.contains("/projects/") {
            (
                200,
                Vec::new(),
                r#"[{"iid":9,"title":"Draft: MR","description":"gl","state":"opened","draft":true,"source_branch":"feat/gl","target_branch":"main","sha":null,"web_url":"https://gitlab.local/mr/9","author":{"username":"bob"}}]"#.to_string(),
            )
        } else if path.contains("/repositories/rate") {
            (
                429,
                vec![("Retry-After".to_string(), "15".to_string())],
                r#"{"error":"slow down"}"#.to_string(),
            )
        } else {
            (401, Vec::new(), r#"{"message":"Bad credentials"}"#.to_string())
        }
    })
    .await;

    let vault = Arc::new(InMemoryTokenVault::new());
    let host = base_url.trim_start_matches("http://");
    vault
        .store_token(ForgeKind::GitHub, host, "default", "ghp_secret_test")
        .unwrap();

    let provider = UnifiedForgeProvider::new(Some(vault)).unwrap();
    let gh_target = ForgeRemoteTarget {
        kind: ForgeKind::GitHub,
        api_base_url: base_url.clone(),
        owner: CompactString::const_new("acme"),
        repo_name: CompactString::const_new("branvio"),
    };

    let prs = provider
        .list_pull_requests(&gh_target, Some(PullRequestState::Open))
        .await
        .unwrap();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].number, 42);
    assert_eq!(prs[0].ci_status, CiCheckStatus::Passing);
    assert_eq!(prs[0].checks[0].name.as_str(), "cargo clippy");

    let created = provider
        .create_pull_request(
            &gh_target,
            CreatePullRequestInput {
                title: "New PR".to_string(),
                body: "Body".to_string(),
                head_branch: CompactString::const_new("feat/new"),
                base_branch: CompactString::const_new("main"),
                draft: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(created.number, 43);
    assert_eq!(created.state, PullRequestState::Draft);

    // Check Bearer auth header was attached from InMemoryTokenVault
    assert!(recorded.lock().unwrap().iter().any(|(_, h)| {
        h.get("authorization")
            .is_some_and(|v| v == "Bearer ghp_secret_test")
    }));

    // GitLab nested subgroup encoding check
    let gl_target = ForgeRemoteTarget {
        kind: ForgeKind::GitLab,
        api_base_url: base_url.clone(),
        owner: CompactString::const_new("org/team"),
        repo_name: CompactString::const_new("repo"),
    };
    let mrs = provider.list_pull_requests(&gl_target, None).await.unwrap();
    assert_eq!(mrs[0].number, 9);
    assert!(
        recorded
            .lock()
            .unwrap()
            .iter()
            .any(|(req, _)| req.contains("/projects/org%2Fteam%2Frepo/merge_requests"))
    );

    // Rate limiting and 401 mapping checks
    let rate_target = ForgeRemoteTarget {
        kind: ForgeKind::Bitbucket,
        api_base_url: base_url.clone(),
        owner: CompactString::const_new("rate"),
        repo_name: CompactString::const_new("limit"),
    };
    assert!(matches!(
        provider.list_pull_requests(&rate_target, None).await,
        Err(ForgeError::RateLimited {
            retry_after_secs: Some(15),
            ..
        })
    ));

    let unauth_target = ForgeRemoteTarget {
        kind: ForgeKind::Gitea,
        api_base_url: base_url,
        owner: CompactString::const_new("bad"),
        repo_name: CompactString::const_new("creds"),
    };
    assert!(matches!(
        provider.list_pull_requests(&unauth_target, None).await,
        Err(ForgeError::Unauthorized { .. })
    ));
}
