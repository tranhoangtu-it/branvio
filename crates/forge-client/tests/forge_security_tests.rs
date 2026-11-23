//! Security regression tests: tokens must never leave the host they were stored for,
//! never travel over plaintext HTTP to a remote host, and never appear in Debug output.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use compact_str::CompactString;
use forge_client::{
    ForgeKind, ForgeProvider, ForgeRemoteTarget, ForgeResult, InMemoryTokenVault, TokenVault,
    UnifiedForgeProvider, parse_remote_url,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type Recorded = Arc<Mutex<Vec<(String, HashMap<String, String>)>>>;

/// Minimal HTTP/1.1 mock: records request line + headers, replies with the handler's
/// `(status, extra headers, body)`.
async fn spawn_server(
    handler: impl Fn(&str) -> (u16, Vec<(String, String)>, String) + Send + Sync + 'static,
) -> (u16, Recorded) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let recorded: Recorded = Arc::new(Mutex::new(Vec::new()));
    let rec_outer = Arc::clone(&recorded);
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let h = Arc::clone(&handler);
            let rec = Arc::clone(&rec_outer);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                let mut lines = raw.split("\r\n");
                let req_line = lines.next().unwrap_or("").to_string();
                let path = req_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let mut headers = HashMap::new();
                for line in lines {
                    if line.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                    }
                }
                rec.lock().unwrap().push((req_line, headers));
                let (status, extra, body) = h(&path);
                let mut resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in extra {
                    resp.push_str(&format!("{k}: {v}\r\n"));
                }
                resp.push_str("\r\n");
                resp.push_str(&body);
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });
    (port, recorded)
}

fn target(kind: ForgeKind, base: &str) -> ForgeRemoteTarget {
    ForgeRemoteTarget {
        kind,
        api_base_url: base.to_string(),
        owner: CompactString::const_new("acme"),
        repo_name: CompactString::const_new("repo"),
    }
}

fn header_values_contain(rec: &Recorded, needle: &str) -> bool {
    rec.lock()
        .unwrap()
        .iter()
        .any(|(_, h)| h.values().any(|v| v.contains(needle)))
}

/// Vault wrapper recording every host it was asked about.
#[derive(Default)]
struct RecordingVault {
    inner: InMemoryTokenVault,
    queried_hosts: Mutex<Vec<String>>,
}

impl TokenVault for RecordingVault {
    fn store_token(
        &self,
        kind: ForgeKind,
        host: &str,
        account: &str,
        token: &str,
    ) -> ForgeResult<()> {
        self.inner.store_token(kind, host, account, token)
    }
    fn load_token(
        &self,
        kind: ForgeKind,
        host: &str,
        account: &str,
    ) -> ForgeResult<Option<String>> {
        self.queried_hosts.lock().unwrap().push(host.to_string());
        self.inner.load_token(kind, host, account)
    }
    fn delete_token(&self, kind: ForgeKind, host: &str, account: &str) -> ForgeResult<()> {
        self.inner.delete_token(kind, host, account)
    }
}

/// Every provider (incl. GitLab's custom `PRIVATE-TOKEN` header, which reqwest's built-in
/// sensitive-header stripping does NOT cover) must not forward credentials on a redirect
/// to another host.
#[tokio::test]
async fn tokens_are_not_forwarded_on_cross_host_redirect() {
    let (evil_port, evil_rec) = spawn_server(|_| (200, Vec::new(), "[]".to_string())).await;
    let (good_port, good_rec) = spawn_server(move |path| {
        (
            302,
            vec![(
                "Location".to_string(),
                format!("http://localhost:{evil_port}{path}"),
            )],
            String::new(),
        )
    })
    .await;

    let base = format!("http://127.0.0.1:{good_port}");
    let vault = Arc::new(InMemoryTokenVault::new());
    for kind in [
        ForgeKind::GitHub,
        ForgeKind::GitLab,
        ForgeKind::Bitbucket,
        ForgeKind::Gitea,
    ] {
        vault
            .store_token(
                kind,
                &format!("127.0.0.1:{good_port}"),
                "default",
                "SECRET_TOKEN_123",
            )
            .unwrap();
    }
    let provider = UnifiedForgeProvider::new(Some(vault)).unwrap();

    for kind in [
        ForgeKind::GitHub,
        ForgeKind::GitLab,
        ForgeKind::Bitbucket,
        ForgeKind::Gitea,
    ] {
        let _ = provider
            .list_pull_requests(&target(kind, &base), None)
            .await;
    }

    // Sanity: the legitimate (loopback) host did receive the token.
    assert!(header_values_contain(&good_rec, "SECRET_TOKEN_123"));
    // The redirect target must never see it, under any header name.
    assert!(
        !header_values_contain(&evil_rec, "SECRET_TOKEN_123"),
        "token leaked to redirect target: {:?}",
        evil_rec.lock().unwrap()
    );
}

/// A same-origin redirect (e.g. GitHub renamed-repo 301) keeps working with auth.
#[tokio::test]
async fn same_origin_redirect_is_followed() {
    let (port, rec) = spawn_server(|path| {
        if path.starts_with("/repos/acme/repo/") {
            (
                301,
                vec![(
                    "Location".to_string(),
                    path.replace("/acme/repo/", "/acme/renamed/"),
                )],
                String::new(),
            )
        } else {
            (200, Vec::new(), "[]".to_string())
        }
    })
    .await;
    let vault = Arc::new(InMemoryTokenVault::new());
    vault
        .store_token(
            ForgeKind::GitHub,
            &format!("127.0.0.1:{port}"),
            "default",
            "tok_same",
        )
        .unwrap();
    let provider = UnifiedForgeProvider::new(Some(vault)).unwrap();
    let prs = provider
        .list_pull_requests(
            &target(ForgeKind::GitHub, &format!("http://127.0.0.1:{port}")),
            None,
        )
        .await
        .unwrap();
    assert!(prs.is_empty());
    let guard = rec.lock().unwrap();
    assert!(guard.iter().any(|(l, h)| {
        l.contains("/acme/renamed/")
            && h.get("authorization")
                .is_some_and(|v| v == "Bearer tok_same")
    }));
}

/// Plain-HTTP API bases on non-loopback hosts must never receive a token: the vault is
/// not even consulted.
#[tokio::test]
async fn plaintext_http_remote_never_gets_token() {
    let vault = Arc::new(RecordingVault::default());
    vault
        .store_token(
            ForgeKind::Gitea,
            "git.example.invalid",
            "default",
            "plain_tok",
        )
        .unwrap();
    let provider =
        UnifiedForgeProvider::new(Some(Arc::clone(&vault) as Arc<dyn TokenVault>)).unwrap();
    let t = parse_remote_url(
        "http://git.example.invalid/acme/repo.git",
        Some(ForgeKind::Gitea),
    )
    .unwrap();
    assert!(t.api_base_url.starts_with("http://"));
    let _ = provider.list_pull_requests(&t, None).await;
    assert!(
        vault.queried_hosts.lock().unwrap().is_empty(),
        "vault consulted for plaintext host: {:?}",
        vault.queried_hosts.lock().unwrap()
    );
}

/// Credentials stored for github.com must not be looked up for look-alike hosts or
/// for hosts smuggled through userinfo in a hand-built API base URL.
#[tokio::test]
async fn lookalike_and_userinfo_hosts_do_not_match_stored_token() {
    let vault = Arc::new(RecordingVault::default());
    vault
        .store_token(ForgeKind::GitHub, "github.com", "default", "gh_tok")
        .unwrap();
    let provider =
        UnifiedForgeProvider::new(Some(Arc::clone(&vault) as Arc<dyn TokenVault>)).unwrap();
    for base in [
        "https://github.com.example.invalid/api/v3",
        "https://github.com@example.invalid/api/v3",
        "https://api.github.com@example.invalid",
        "https://example.invalid\\@api.github.com",
    ] {
        let _ = provider
            .list_pull_requests(&target(ForgeKind::GitHub, base), None)
            .await;
    }
    let queried = vault.queried_hosts.lock().unwrap().clone();
    assert!(
        !queried
            .iter()
            .any(|h| h == "github.com" || h == "api.github.com"),
        "github.com token looked up for foreign host: {queried:?}"
    );
}

/// The repository owner comes from an untrusted remote URL; it must not select a stored
/// credential (previously the owner was tried as a vault account label).
#[tokio::test]
async fn repository_owner_does_not_select_stored_token() {
    let (port, rec) = spawn_server(|_| (200, Vec::new(), "[]".to_string())).await;
    let host = format!("127.0.0.1:{port}");
    let vault = Arc::new(RecordingVault::default());
    // A token stored under an account label that happens to equal the repo owner ("acme").
    vault
        .store_token(ForgeKind::GitHub, &host, "acme", "OWNER_KEYED_TOKEN")
        .unwrap();
    let provider =
        UnifiedForgeProvider::new(Some(Arc::clone(&vault) as Arc<dyn TokenVault>)).unwrap();
    let prs = provider
        .list_pull_requests(&target(ForgeKind::GitHub, &format!("http://{host}")), None)
        .await
        .unwrap();
    assert!(prs.is_empty());
    assert!(
        !header_values_contain(&rec, "OWNER_KEYED_TOKEN"),
        "owner-keyed token was sent: {:?}",
        rec.lock().unwrap()
    );
}

/// A token stored for `host` (no port) must not be sent to `host:<port>`.
#[tokio::test]
async fn portless_token_is_not_sent_to_other_port() {
    let (port, rec) = spawn_server(|_| (200, Vec::new(), "[]".to_string())).await;
    let vault = Arc::new(RecordingVault::default());
    for kind in [
        ForgeKind::GitHub,
        ForgeKind::GitLab,
        ForgeKind::Bitbucket,
        ForgeKind::Gitea,
    ] {
        vault
            .store_token(kind, "127.0.0.1", "default", "PORTLESS_TOKEN")
            .unwrap();
    }
    let provider =
        UnifiedForgeProvider::new(Some(Arc::clone(&vault) as Arc<dyn TokenVault>)).unwrap();
    for kind in [
        ForgeKind::GitHub,
        ForgeKind::GitLab,
        ForgeKind::Bitbucket,
        ForgeKind::Gitea,
    ] {
        let _ = provider
            .list_pull_requests(&target(kind, &format!("http://127.0.0.1:{port}")), None)
            .await;
    }
    assert!(
        !header_values_contain(&rec, "PORTLESS_TOKEN"),
        "port-less token sent to another port: {:?}",
        rec.lock().unwrap()
    );
    let queried = vault.queried_hosts.lock().unwrap().clone();
    assert!(
        queried.iter().all(|h| h == &format!("127.0.0.1:{port}")),
        "vault consulted for hosts other than the exact host:port: {queried:?}"
    );
}

/// The exact `host:port` key and the configured account still work.
#[tokio::test]
async fn exact_host_and_port_token_is_used() {
    let (port, rec) = spawn_server(|_| (200, Vec::new(), "[]".to_string())).await;
    let vault = Arc::new(InMemoryTokenVault::new());
    vault
        .store_token(
            ForgeKind::Gitea,
            &format!("127.0.0.1:{port}"),
            "default",
            "EXACT_TOKEN",
        )
        .unwrap();
    let provider = UnifiedForgeProvider::new(Some(vault)).unwrap();
    let _ = provider
        .list_pull_requests(
            &target(ForgeKind::Gitea, &format!("http://127.0.0.1:{port}")),
            None,
        )
        .await;
    assert!(header_values_contain(&rec, "EXACT_TOKEN"));
}

#[test]
fn in_memory_vault_debug_does_not_print_tokens() {
    let vault = InMemoryTokenVault::new();
    vault
        .store_token(ForgeKind::GitHub, "github.com", "default", "ghp_DEBUG_LEAK")
        .unwrap();
    let dbg = format!("{vault:?} {vault:#?}");
    assert!(!dbg.contains("ghp_DEBUG_LEAK"), "Debug leaked token: {dbg}");
}

#[test]
fn parser_normalizes_host_case_userinfo_and_port() {
    let t = parse_remote_url("https://User:Pa55@GitHub.COM/Acme/Repo.git", None).unwrap();
    assert_eq!(t.kind, ForgeKind::GitHub);
    assert_eq!(t.api_base_url, "https://api.github.com");
    assert!(!t.api_base_url.contains("Pa55"));

    let t = parse_remote_url("git@GITLAB.com:group/sub/proj.git", None).unwrap();
    assert_eq!(t.kind, ForgeKind::GitLab);
    assert_eq!(t.api_base_url, "https://gitlab.com/api/v4");

    let t = parse_remote_url(
        "https://Git.Corp.Example:8443/a/b.git",
        Some(ForgeKind::Gitea),
    )
    .unwrap();
    assert_eq!(t.api_base_url, "https://git.corp.example:8443/api/v1");

    // Explicit default port is dropped so it maps to the same vault key.
    let t = parse_remote_url("https://github.com:443/a/b.git", None).unwrap();
    assert_eq!(t.api_base_url, "https://api.github.com");

    // ssh:// port is an SSH port, not an HTTPS one.
    let t = parse_remote_url("ssh://git@gitea.example:2222/a/b.git", None).unwrap();
    assert_eq!(t.kind, ForgeKind::Gitea);
    assert_eq!(t.api_base_url, "https://gitea.example/api/v1");
}

#[test]
fn parser_does_not_treat_lookalike_hosts_as_known_forges() {
    // Look-alike of github.com must not be routed to api.github.com.
    let t = parse_remote_url("https://github.com.evil.example/a/b.git", None).unwrap();
    assert_eq!(t.api_base_url, "https://github.com.evil.example/api/v3");

    let t = parse_remote_url("git@bitbucket.org.evil.example:a/b.git", None).unwrap();
    assert_ne!(t.api_base_url, "https://api.bitbucket.org/2.0");
    assert!(
        t.api_base_url
            .starts_with("https://bitbucket.org.evil.example/")
    );

    // Substring matches anywhere in the hostname no longer pick the forge kind.
    let t = parse_remote_url("https://notgitlab.example/a/b.git", None).unwrap();
    assert_eq!(t.kind, ForgeKind::GitHub);
    let t = parse_remote_url("https://evil-bitbucket.example/a/b.git", None).unwrap();
    assert_eq!(t.kind, ForgeKind::GitHub);

    // Conventional self-hosted names (first DNS label) still auto-detect.
    let t = parse_remote_url("https://gitlab.company.internal/a/b.git", None).unwrap();
    assert_eq!(t.kind, ForgeKind::GitLab);
    let t = parse_remote_url("https://codeberg.org/a/b.git", None).unwrap();
    assert_eq!(t.kind, ForgeKind::Gitea);

    // IDN hosts are normalised to punycode (never compared as raw Unicode).
    let t = parse_remote_url("https://gïthub.com/a/b.git", None).unwrap();
    assert!(t.api_base_url.starts_with("https://xn--"));
    assert_ne!(t.api_base_url, "https://api.github.com");
}

#[test]
fn parser_rejects_malformed_authorities() {
    for bad in [
        "https://exa mple.com/a/b",
        "https://evil.example\\@github.com/a/b",
        "https:///a/b",
        "https://@/a/b",
        "git@:a/b",
    ] {
        assert!(parse_remote_url(bad, None).is_err(), "accepted {bad}");
    }
}
