//! Git remote URL parser mapping SSH, SCP, and HTTP(S) remotes to [`ForgeRemoteTarget`].
//!
//! Security notes: the returned `api_base_url` is what the token vault is keyed on, so the
//! host is canonicalised (lowercase ASCII, IDN → punycode, default port dropped) and any
//! ambiguous authority (whitespace, backslashes, empty host) is rejected rather than guessed.

use crate::traits::{ForgeError, ForgeKind, ForgeRemoteTarget, ForgeResult};
use compact_str::CompactString;

/// Parses SSH (`git@host:owner/repo.git`, `ssh://...`) or HTTP(S) remote URLs into a normalized [`ForgeRemoteTarget`].
pub fn parse_remote_url(
    raw_url: &str,
    hint_kind: Option<ForgeKind>,
) -> ForgeResult<ForgeRemoteTarget> {
    let trimmed = raw_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(ForgeError::Protocol("empty Git remote URL".to_string()));
    }

    let (scheme, host_and_port, path_part) =
        if let Some(rest) = strip_prefix_ci(trimmed, "https://") {
            split_scheme_url("https", rest)?
        } else if let Some(rest) = strip_prefix_ci(trimmed, "http://") {
            split_scheme_url("http", rest)?
        } else if let Some(rest) = strip_prefix_ci(trimmed, "ssh://") {
            let (_s, hp, path) = split_scheme_url("https", rest)?;
            // Drop optional SSH port from HTTPS API origin when converting ssh:// URLs.
            ("https", strip_port(&hp).to_string(), path)
        } else if let Some((left, right)) = trimmed.split_once(':') {
            let host = left
                .rsplit_once('@')
                .map_or(left, |(_user, h)| h)
                .to_string();
            if host.is_empty() || right.is_empty() || host.contains(['/', '\\']) {
                return Err(ForgeError::Protocol(format!(
                    "invalid SSH remote URL '{trimmed}'"
                )));
            }
            ("https", host, right.to_string())
        } else {
            return Err(ForgeError::Protocol(format!(
                "unsupported Git remote URL format '{trimmed}'"
            )));
        };

    let clean_path = path_part
        .trim_matches('/')
        .strip_suffix(".git")
        .unwrap_or_else(|| path_part.trim_matches('/'));
    let segments: Vec<&str> = clean_path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return Err(ForgeError::Protocol(format!(
            "remote URL '{trimmed}' missing owner/repository path segments"
        )));
    }

    let (host_no_port, host_and_port) = normalize_authority(scheme, &host_and_port)?;
    let kind = hint_kind.unwrap_or_else(|| detect_forge_kind_from_host(&host_no_port));

    let (owner, repo_name) = match kind {
        ForgeKind::GitLab => {
            let repo = segments[segments.len() - 1];
            let group_path = segments[..segments.len() - 1].join("/");
            (group_path, repo.to_string())
        }
        _ => (segments[0].to_string(), segments[1].to_string()),
    };

    let origin = format!("{scheme}://{host_and_port}");
    let api_base_url = resolve_api_base_url(kind, &host_no_port, &origin);

    Ok(ForgeRemoteTarget {
        kind,
        api_base_url,
        owner: CompactString::from(owner),
        repo_name: CompactString::from(repo_name),
    })
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// Removes a trailing `:port` while keeping bracketed IPv6 literals intact.
fn strip_port(host_and_port: &str) -> &str {
    if host_and_port.starts_with('[') {
        return host_and_port
            .find(']')
            .map_or(host_and_port, |end| &host_and_port[..=end]);
    }
    host_and_port.split(':').next().unwrap_or(host_and_port)
}

fn split_scheme_url(
    scheme: &'static str,
    rest: &str,
) -> ForgeResult<(&'static str, String, String)> {
    let (authority, path) = rest
        .split_once('/')
        .ok_or_else(|| ForgeError::Protocol("remote URL missing path component".to_string()))?;
    // A backslash or whitespace in the authority is parsed differently by different URL
    // parsers (e.g. WHATWG treats `\` as a path separator); refuse instead of guessing.
    if authority
        .chars()
        .any(|c| c == '\\' || c.is_whitespace() || c.is_control())
    {
        return Err(ForgeError::Protocol(
            "remote URL has malformed authority".to_string(),
        ));
    }
    // Strip embedded user/password credentials so secrets never leak into logs or base URLs.
    let host_and_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_creds, hp)| hp);
    if host_and_port.is_empty() {
        return Err(ForgeError::Protocol("remote URL missing host".to_string()));
    }
    Ok((scheme, host_and_port.to_string(), path.to_string()))
}

/// Validates and canonicalises an authority (`host[:port]`) via the WHATWG URL parser:
/// lowercases ASCII, converts IDN to punycode, drops the scheme's default port, and rejects
/// whitespace, backslashes, stray `@`, or empty hosts. Returns `(host, host[:port])`.
fn normalize_authority(scheme: &str, authority: &str) -> ForgeResult<(String, String)> {
    let invalid = || ForgeError::Protocol(format!("invalid host in remote URL '{authority}'"));
    if authority.is_empty()
        || authority.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '\\' | '@' | '/' | '?' | '#')
        })
    {
        return Err(invalid());
    }
    let parsed = reqwest::Url::parse(&format!("{scheme}://{authority}/")).map_err(|_| invalid())?;
    let host = parsed
        .host_str()
        .ok_or_else(invalid)?
        .trim_end_matches('.')
        .to_string();
    if host.is_empty() {
        return Err(invalid());
    }
    let host_and_port = match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.clone(),
    };
    Ok((host, host_and_port))
}

/// Picks the forge kind from an already-normalised hostname. Well-known SaaS hosts match
/// exactly (or as a subdomain); self-hosted instances are recognised only by their *first*
/// DNS label (e.g. `gitlab.company.internal`, `gitea-01.lan`), never by a substring
/// anywhere in the name. Unknown hosts default to GitHub (Enterprise) semantics; callers
/// can always pass an explicit `hint_kind`.
fn detect_forge_kind_from_host(host_no_port: &str) -> ForgeKind {
    let is_or_sub = |domain: &str| {
        host_no_port == domain
            || host_no_port
                .strip_suffix(domain)
                .is_some_and(|prefix| prefix.ends_with('.'))
    };
    let first_label = host_no_port.split('.').next().unwrap_or_default();
    let label_is = |name: &str| {
        first_label.strip_prefix(name).is_some_and(|rest| {
            rest.is_empty() || rest.starts_with('-') || rest.chars().all(|c| c.is_ascii_digit())
        })
    };
    if is_or_sub("github.com") {
        ForgeKind::GitHub
    } else if is_or_sub("gitlab.com") || label_is("gitlab") {
        ForgeKind::GitLab
    } else if is_or_sub("bitbucket.org") || label_is("bitbucket") {
        ForgeKind::Bitbucket
    } else if is_or_sub("codeberg.org") || label_is("gitea") || label_is("forgejo") {
        ForgeKind::Gitea
    } else {
        ForgeKind::GitHub
    }
}

fn resolve_api_base_url(kind: ForgeKind, host_no_port: &str, origin: &str) -> String {
    match kind {
        ForgeKind::GitHub => {
            if host_no_port == "github.com" {
                "https://api.github.com".to_string()
            } else {
                format!("{origin}/api/v3")
            }
        }
        ForgeKind::GitLab => format!("{origin}/api/v4"),
        ForgeKind::Bitbucket => {
            if host_no_port == "bitbucket.org" {
                "https://api.bitbucket.org/2.0".to_string()
            } else {
                format!("{origin}/rest/api/1.0")
            }
        }
        ForgeKind::Gitea => format!("{origin}/api/v1"),
    }
}
