//! Best-effort secret masking and sensitive-file filtering applied to every prompt before it is
//! sent to any AI provider (local or cloud).
//!
//! The scanner is hand-written (no regex dependency) and operates on ASCII byte patterns only,
//! so every span boundary it produces lies on a UTF-8 character boundary and it never panics on
//! arbitrary UTF-8 input.

/// Fixed placeholder substituted for every detected secret.
pub const REDACTED: &str = "[REDACTED]";

/// Result of [`mask_secrets`]: the masked text plus how many secret spans were replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskedText {
    /// Input text with every detected secret replaced by [`REDACTED`].
    pub text: String,
    /// Number of distinct (merged) secret spans that were masked.
    pub findings: usize,
}

/// Result of [`filter_sensitive_diff`]: the diff without sensitive file sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredDiff {
    /// Unified diff text with sensitive `diff --git` sections replaced by a one-line note.
    pub text: String,
    /// Repository paths whose diff sections were dropped.
    pub dropped_paths: Vec<String>,
}

/// Masks well-known credential formats in `text` with [`REDACTED`].
///
/// Detected formats: AWS access key ids (`AKIA`/`ASIA`), GitHub tokens (`ghp_`, `gho_`, `ghu_`,
/// `ghs_`, `ghr_`, `github_pat_`), OpenAI/Anthropic style keys (`sk-…`, `sk-ant-…`), Google API
/// keys (`AIza…`), Slack tokens (`xox[abprs]-…`), Stripe secret/restricted keys
/// (`sk_live_`/`sk_test_`/`rk_live_`/`rk_test_`), GitLab (`glpat-`), npm (`npm_`) and PyPI
/// (`pypi-`) tokens, JWTs, PEM private key blocks (whole block, or through end of text when the
/// `END` marker is missing), `Bearer <token>` / `Authorization: Basic <b64>` credentials,
/// passwords (and token-like usernames) embedded in URLs (`scheme://user:pass@host`), and the
/// *values* of generic `password`/`secret`/`token`/`api_key`/`credentials`/`auth` style
/// assignments.
///
/// Known limitation: a token that is split across two lines (for example a long key wrapped
/// over consecutive `+` lines of a diff) is only masked if each fragment independently matches
/// a rule; fragments that are too short to be recognised on their own are left as-is.
pub fn mask_secrets(text: &str) -> MaskedText {
    let bytes = text.as_bytes();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    collect_pem_spans(text, &mut spans);
    collect_token_spans(bytes, &mut spans);
    collect_assignment_spans(bytes, &mut spans);
    collect_auth_header_spans(bytes, &mut spans);
    collect_url_credential_spans(bytes, &mut spans);

    if spans.is_empty() {
        return MaskedText {
            text: text.to_string(),
            findings: 0,
        };
    }
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (s, e) in spans {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut findings = 0;
    for (s, e) in merged {
        // All spans start/end on ASCII bytes or text end; `get` keeps this panic-free regardless.
        let (Some(before), Some(_)) = (text.get(cursor..s), text.get(s..e)) else {
            continue;
        };
        out.push_str(before);
        out.push_str(REDACTED);
        cursor = e;
        findings += 1;
    }
    out.push_str(text.get(cursor..).unwrap_or(""));
    MaskedText {
        text: out,
        findings,
    }
}

/// Returns `true` when `path` names a file that commonly holds credentials and must never be
/// sent to an AI provider (`.env*`, keys/certificates, SSH identities, keystores, registry
/// credential files, `secrets.*`, cloud/Kubernetes/Terraform/Docker credential and state files,
/// anything under `.ssh/` except `*.pub`, and anything under a `secrets/` or `.secrets/`
/// directory).
pub fn is_sensitive_path(path: &str) -> bool {
    let trimmed = path.trim().trim_matches('"').to_ascii_lowercase();
    let components: Vec<&str> = trimmed
        .split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    let Some((&name, dirs)) = components.split_last() else {
        return false;
    };
    const EXACT: &[&str] = &[
        ".env",
        "credentials",
        "credentials.json",
        ".npmrc",
        ".pypirc",
        ".netrc",
        "_netrc",
        ".git-credentials",
        ".htpasswd",
        "kubeconfig",
        ".dockercfg",
    ];
    const PREFIXES: &[&str] = &[
        ".env.",
        "secrets.",
        "secring.",
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
    ];
    const SUFFIXES: &[&str] = &[
        ".pem",
        ".key",
        ".p12",
        ".pfx",
        ".keystore",
        ".jks",
        ".env",
        ".tfvars",
        ".tfvars.json",
        ".tfstate",
        ".tfstate.backup",
    ];
    if EXACT.contains(&name)
        || PREFIXES.iter().any(|p| name.starts_with(p))
        || SUFFIXES.iter().any(|s| name.ends_with(s))
    {
        return true;
    }
    if (name.starts_with("service-account") || name.starts_with("service_account"))
        && name.ends_with(".json")
    {
        return true;
    }
    let parent = dirs.last().copied();
    if (parent == Some(".kube") && name == "config")
        || (parent == Some(".docker") && name == "config.json")
    {
        return true;
    }
    if dirs.contains(&".ssh") && !name.ends_with(".pub") {
        return true;
    }
    dirs.iter().any(|d| *d == "secrets" || *d == ".secrets")
}

/// Drops every `diff --git a/X b/Y` section of a unified diff whose path is sensitive per
/// [`is_sensitive_path`], replacing it with a one-line placeholder note.
pub fn filter_sensitive_diff(diff: &str) -> FilteredDiff {
    let mut text = String::with_capacity(diff.len());
    let mut dropped_paths = Vec::new();
    let mut skipping = false;
    for line in diff.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let paths = parse_diff_git_paths(rest.trim_end());
            if let Some(hit) = paths.iter().find(|p| is_sensitive_path(p)) {
                skipping = true;
                text.push_str("# [omitted diff of sensitive file: ");
                text.push_str(hit);
                text.push_str("]\n");
                dropped_paths.push((*hit).to_string());
                continue;
            }
            skipping = false;
        }
        if !skipping {
            text.push_str(line);
        }
    }
    FilteredDiff {
        text,
        dropped_paths,
    }
}

/// Drops sensitive file sections and then masks secrets in the remaining diff text.
pub fn sanitize_diff(diff: &str) -> MaskedText {
    let filtered = filter_sensitive_diff(diff);
    let mut masked = mask_secrets(&filtered.text);
    masked.findings += filtered.dropped_paths.len();
    masked
}

fn strip_diff_side_prefix(p: &str) -> &str {
    let p = p.trim().trim_matches('"');
    p.strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p)
}

fn parse_diff_git_paths(rest: &str) -> Vec<&str> {
    let strip = strip_diff_side_prefix;
    if let Some(idx) = rest.rfind(" b/").or_else(|| rest.rfind(" \"b/")) {
        let (a, b) = rest.split_at(idx);
        vec![strip(a), strip(b)]
    } else {
        rest.split_whitespace().map(strip).collect()
    }
}

// ---------------------------------------------------------------------------------------------
// PEM private key blocks
// ---------------------------------------------------------------------------------------------

fn collect_pem_spans(text: &str, spans: &mut Vec<(usize, usize)>) {
    const BEGIN: &str = "-----BEGIN ";
    let mut search_from = 0;
    while let Some(rel) = text.get(search_from..).and_then(|t| t.find(BEGIN)) {
        let start = search_from + rel;
        let after_begin = start + BEGIN.len();
        let line_end = text
            .get(after_begin..)
            .and_then(|t| t.find('\n'))
            .map_or(text.len(), |i| after_begin + i);
        let header = text.get(after_begin..line_end).unwrap_or("");
        if !header.contains("PRIVATE KEY-----") {
            search_from = after_begin;
            continue;
        }
        let end = match text.get(line_end..).and_then(|t| t.find("-----END ")) {
            Some(i) => {
                let end_marker = line_end + i + "-----END ".len();
                text.get(end_marker..)
                    .and_then(|t| t.find("-----"))
                    .map_or(text.len(), |j| end_marker + j + "-----".len())
            }
            None => text.len(),
        };
        spans.push((start, end));
        search_from = end;
    }
}

// ---------------------------------------------------------------------------------------------
// Prefixed tokens
// ---------------------------------------------------------------------------------------------

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_b64url_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Length of the run of bytes starting at `from` satisfying `pred`.
fn run_len(bytes: &[u8], from: usize, pred: impl Fn(u8) -> bool) -> usize {
    bytes
        .get(from..)
        .map_or(0, |rest| rest.iter().take_while(|b| pred(**b)).count())
}

fn collect_token_spans(bytes: &[u8], spans: &mut Vec<(usize, usize)>) {
    let mut i = 0;
    while i < bytes.len() {
        let at_boundary = i == 0 || bytes.get(i - 1).is_none_or(|b| !is_word_byte(*b));
        if at_boundary && let Some(end) = match_token_at(bytes, i) {
            spans.push((i, end));
            i = end;
            continue;
        }
        i += 1;
    }
}

fn match_token_at(bytes: &[u8], i: usize) -> Option<usize> {
    let rest = bytes.get(i..)?;
    let first = *rest.first()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }

    // AWS access key id: AKIA/ASIA + exactly 16 uppercase alphanumerics.
    if rest.starts_with(b"AKIA") || rest.starts_with(b"ASIA") {
        let body = i + 4;
        let n = run_len(bytes, body, |b| {
            b.is_ascii_uppercase() || b.is_ascii_digit()
        });
        if n == 16 && bytes.get(body + n).is_none_or(|b| !is_word_byte(*b)) {
            return Some(body + n);
        }
    }

    // GitHub fine-grained PAT.
    if rest.starts_with(b"github_pat_") {
        let body = i + "github_pat_".len();
        let n = run_len(bytes, body, is_word_byte);
        if n >= 20 {
            return Some(body + n);
        }
    }

    // GitHub classic tokens.
    for prefix in [b"ghp_", b"gho_", b"ghu_", b"ghs_", b"ghr_"] {
        if rest.starts_with(prefix) {
            let body = i + prefix.len();
            let n = run_len(bytes, body, |b| b.is_ascii_alphanumeric());
            if n >= 20 {
                return Some(body + n);
            }
        }
    }

    // OpenAI / Anthropic style keys (`sk-…`, `sk-proj-…`, `sk-ant-…`).
    if rest.starts_with(b"sk-") {
        let body = i + 3;
        let n = run_len(bytes, body, is_b64url_byte);
        let has_digit = bytes
            .get(body..body + n)
            .is_some_and(|s| s.iter().any(u8::is_ascii_digit));
        if n >= 20 && has_digit {
            return Some(body + n);
        }
    }

    // Google API key: AIza + 35 base64url chars.
    if rest.starts_with(b"AIza") {
        let body = i + 4;
        let n = run_len(bytes, body, is_b64url_byte);
        if n >= 30 {
            return Some(body + n);
        }
    }

    // Slack tokens: xox[abprs]-…
    if rest.starts_with(b"xox")
        && matches!(rest.get(3), Some(b'a' | b'b' | b'p' | b'r' | b's'))
        && rest.get(4) == Some(&b'-')
    {
        let body = i + 5;
        let n = run_len(bytes, body, |b| b.is_ascii_alphanumeric() || b == b'-');
        if n >= 10 {
            return Some(body + n);
        }
    }

    // Stripe secret / restricted keys (publishable `pk_` keys are public and left alone).
    for prefix in [b"sk_live_", b"sk_test_", b"rk_live_", b"rk_test_"] {
        if rest.starts_with(prefix) {
            let body = i + prefix.len();
            let n = run_len(bytes, body, is_word_byte);
            if n >= 16 {
                return Some(body + n);
            }
        }
    }

    // GitLab personal access tokens.
    if rest.starts_with(b"glpat-") {
        let body = i + "glpat-".len();
        let n = run_len(bytes, body, is_b64url_byte);
        if n >= 20 {
            return Some(body + n);
        }
    }

    // npm automation / publish tokens: npm_ + 36 alphanumerics.
    if rest.starts_with(b"npm_") {
        let body = i + "npm_".len();
        let n = run_len(bytes, body, |b| b.is_ascii_alphanumeric());
        if n >= 36 {
            return Some(body + n);
        }
    }

    // PyPI API tokens (`pypi-` + long macaroon).
    if rest.starts_with(b"pypi-") {
        let body = i + "pypi-".len();
        let n = run_len(bytes, body, is_b64url_byte);
        if n >= 32 {
            return Some(body + n);
        }
    }

    // JWT: eyJ<seg>.<seg>.<seg>
    if rest.starts_with(b"eyJ") {
        let seg1 = run_len(bytes, i, is_b64url_byte);
        let dot1 = i + seg1;
        if seg1 >= 10 && bytes.get(dot1) == Some(&b'.') {
            let seg2 = run_len(bytes, dot1 + 1, is_b64url_byte);
            let dot2 = dot1 + 1 + seg2;
            if seg2 >= 10 && bytes.get(dot2) == Some(&b'.') {
                let seg3 = run_len(bytes, dot2 + 1, is_b64url_byte);
                if seg3 >= 10 {
                    return Some(dot2 + 1 + seg3);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// HTTP `Authorization` credentials
// ---------------------------------------------------------------------------------------------

/// `true` when `bytes[i..]` starts with `word` (ASCII case-insensitive) at a word boundary.
fn word_at_ci(bytes: &[u8], i: usize, word: &[u8]) -> bool {
    let Some(candidate) = bytes.get(i..i + word.len()) else {
        return false;
    };
    candidate.eq_ignore_ascii_case(word)
        && (i == 0 || bytes.get(i - 1).is_none_or(|b| !is_word_byte(*b)))
        && bytes.get(i + word.len()).is_none_or(|b| !is_word_byte(*b))
}

/// `true` when the line containing `i` has `authorization` (case-insensitive) before `i`.
fn line_has_authorization_before(bytes: &[u8], i: usize) -> bool {
    let line_start = bytes
        .get(..i)
        .and_then(|b| b.iter().rposition(|c| *c == b'\n'))
        .map_or(0, |p| p + 1);
    bytes.get(line_start..i).is_some_and(|line| {
        line.windows(13)
            .any(|w| w.eq_ignore_ascii_case(b"authorization"))
    })
}

/// Masks the credential in `Bearer <token>` (token ≥ 16 chars, anywhere) and
/// `Authorization: Basic <base64>` (base64 ≥ 8 chars, only after an `Authorization` header name
/// on the same line, to avoid prose such as "Basic configuration").
fn collect_auth_header_spans(bytes: &[u8], spans: &mut Vec<(usize, usize)>) {
    let token68 =
        |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'+' | b'/');
    let b64 = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/');
    let mut i = 0;
    while i < bytes.len() {
        let (scheme_len, min_len, is_basic) = if word_at_ci(bytes, i, b"bearer") {
            (6, 16, false)
        } else if word_at_ci(bytes, i, b"basic") {
            (5, 8, true)
        } else {
            i += 1;
            continue;
        };
        let after = i + scheme_len;
        let start = skip_inline_ws(bytes, after);
        if start == after || (is_basic && !line_has_authorization_before(bytes, i)) {
            i = after;
            continue;
        }
        let n = if is_basic {
            run_len(bytes, start, b64)
        } else {
            run_len(bytes, start, token68)
        };
        let pad = run_len(bytes, start + n, |b| b == b'=');
        if n >= min_len {
            spans.push((start, start + n + pad));
            i = start + n + pad;
        } else {
            i = after;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// URL credentials (`scheme://user:pass@host`)
// ---------------------------------------------------------------------------------------------

/// Heuristic: does a URL username look like an access token rather than a login name?
fn looks_like_token(user: &[u8]) -> bool {
    user.len() >= 16
        && user.iter().any(u8::is_ascii_digit)
        && user.iter().any(u8::is_ascii_alphabetic)
        && user.iter().all(|b| is_b64url_byte(*b) || *b == b'.')
}

/// Masks the password (and a token-like username) embedded in URL authorities.
fn collect_url_credential_spans(bytes: &[u8], spans: &mut Vec<(usize, usize)>) {
    let authority_byte = |b: u8| {
        b.is_ascii_graphic()
            && !matches!(
                b,
                b'/' | b'?' | b'#' | b'"' | b'\'' | b'`' | b'<' | b'>' | b'\\'
            )
    };
    let mut search = 0;
    while let Some(rel) = bytes
        .get(search..)
        .and_then(|rest| rest.windows(3).position(|w| w == b"://"))
    {
        let auth_start = search + rel + 3;
        let auth_len = run_len(bytes, auth_start, authority_byte);
        let auth_end = auth_start + auth_len;
        search = auth_start;
        let Some(authority) = bytes.get(auth_start..auth_end) else {
            continue;
        };
        let Some(at) = authority.iter().rposition(|b| *b == b'@') else {
            continue;
        };
        let Some(userinfo) = authority.get(..at) else {
            continue;
        };
        if userinfo.is_empty() {
            continue;
        }
        match userinfo.iter().position(|b| *b == b':') {
            Some(colon) => {
                let user = userinfo.get(..colon).unwrap_or_default();
                let pass_start = auth_start + colon + 1;
                let pass_end = auth_start + at;
                let pass = bytes.get(pass_start..pass_end).unwrap_or_default();
                if !pass.is_empty() && pass != REDACTED.as_bytes() {
                    spans.push((pass_start, pass_end));
                }
                if looks_like_token(user) {
                    spans.push((auth_start, auth_start + colon));
                }
            }
            None => {
                if looks_like_token(userinfo) {
                    spans.push((auth_start, auth_start + at));
                }
            }
        }
        search = auth_end.max(auth_start);
    }
}

// ---------------------------------------------------------------------------------------------
// Generic `password = "…"` / `token: …` assignments
// ---------------------------------------------------------------------------------------------

/// Lowercased identifier suffixes whose assigned value is treated as a secret.
const SECRET_KEY_SUFFIXES: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "api-key",
    "access_key",
    "secret_key",
    "private_key",
    "credential",
    "credentials",
    "passphrase",
    "auth",
];

/// Key-name words whose *unquoted* value is masked even without digits (when ≥ 8 chars and not
/// obviously code such as a type name, call or member access).
const STRONG_SECRET_WORDS: &[&str] = &["password", "passwd", "passphrase", "secret", "credential"];

/// Classification of an identifier as a secret-bearing key name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SecretKeyKind {
    /// Not a secret key name.
    None,
    /// `token`, `api_key`, … — unquoted values need a digit to be masked.
    Weak,
    /// `password`, `secret`, … — unquoted values ≥ 8 chars are masked without digits.
    Strong,
}

fn classify_secret_identifier(ident: &[u8]) -> SecretKeyKind {
    let lower: Vec<u8> = ident.iter().map(u8::to_ascii_lowercase).collect();
    let is_secret = SECRET_KEY_SUFFIXES
        .iter()
        .any(|suffix| lower.ends_with(suffix.as_bytes()));
    if !is_secret {
        return SecretKeyKind::None;
    }
    let strong = STRONG_SECRET_WORDS
        .iter()
        .any(|w| lower.windows(w.len()).any(|win| win == w.as_bytes()));
    if strong {
        SecretKeyKind::Strong
    } else {
        SecretKeyKind::Weak
    }
}

/// Unquoted digit-free values that are clearly code or placeholders rather than secrets.
fn looks_like_code_value(value: &[u8], next: Option<u8>) -> bool {
    const KEYWORDS: &[&[u8]] = &[
        b"undefined",
        b"required",
        b"optional",
        b"nullable",
        b"readonly",
        b"function",
    ];
    let Some(&first) = value.first() else {
        return true;
    };
    if matches!(first, b'&' | b'*' | b'$' | b'%' | b'@' | b'!' | b'#') {
        return true; // references, env/template expansions, decorators
    }
    if matches!(next, Some(b'(' | b'<' | b'[' | b'{')) {
        return true; // calls, generics, indexing
    }
    if value.windows(2).any(|w| w == b"::") {
        return true; // paths
    }
    let ident_like = |b: &u8| b.is_ascii_alphanumeric() || *b == b'_';
    // Member access chains: `self.password`, `cfg.secret.value`.
    if value.contains(&b'.')
        && value
            .split(|b| *b == b'.')
            .all(|seg| !seg.is_empty() && seg.iter().all(ident_like))
    {
        return true;
    }
    // CamelCase type names: `PasswordHash`, `SecretString`.
    if first.is_ascii_uppercase()
        && value.iter().all(u8::is_ascii_alphanumeric)
        && value.iter().any(u8::is_ascii_lowercase)
    {
        return true;
    }
    KEYWORDS.iter().any(|k| value.eq_ignore_ascii_case(k))
}

fn skip_inline_ws(bytes: &[u8], mut i: usize) -> usize {
    while matches!(bytes.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    i
}

fn collect_assignment_spans(bytes: &[u8], spans: &mut Vec<(usize, usize)>) {
    let ident_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    let mut i = 0;
    while i < bytes.len() {
        let starts_ident = bytes.get(i).is_some_and(|b| ident_byte(*b))
            && (i == 0 || bytes.get(i - 1).is_none_or(|b| !ident_byte(*b)));
        if !starts_ident {
            i += 1;
            continue;
        }
        let len = run_len(bytes, i, ident_byte);
        let ident_end = i + len;
        let kind = bytes
            .get(i..ident_end)
            .map_or(SecretKeyKind::None, classify_secret_identifier);
        if kind != SecretKeyKind::None
            && let Some(span) = assignment_value_span(bytes, ident_end, kind)
        {
            spans.push(span);
        }
        i = ident_end.max(i + 1);
    }
}

/// Given the index just past a secret-looking identifier, returns the span of the assigned value
/// (excluding surrounding quotes), if the identifier is followed by `=`, `:`, or `:=`.
///
/// Quoted values are always masked. Unquoted values are masked when they contain both a digit
/// and a letter (≥ 6 chars), or — for [`SecretKeyKind::Strong`] key names — when they are
/// ≥ 8 chars and do not look like code (see [`looks_like_code_value`]).
fn assignment_value_span(
    bytes: &[u8],
    ident_end: usize,
    kind: SecretKeyKind,
) -> Option<(usize, usize)> {
    let mut j = ident_end;
    // Allow a closing quote after a quoted key, e.g. `"password": "…"`.
    if matches!(bytes.get(j), Some(b'"' | b'\'')) {
        j += 1;
    }
    j = skip_inline_ws(bytes, j);
    match bytes.get(j) {
        Some(b'=') => {
            // Skip `==` comparisons and `=>` arrows.
            if matches!(bytes.get(j + 1), Some(b'=' | b'>')) {
                return None;
            }
            j += 1;
        }
        Some(b':') => {
            j += 1;
            if bytes.get(j) == Some(&b'=') {
                j += 1;
            } else if bytes.get(j) == Some(&b':') {
                return None; // Rust paths like `token::Kind`.
            }
        }
        _ => return None,
    }
    j = skip_inline_ws(bytes, j);
    match bytes.get(j) {
        Some(&q @ (b'"' | b'\'' | b'`')) => {
            let start = j + 1;
            let mut k = start;
            while let Some(&b) = bytes.get(k) {
                if b == b'\\' {
                    k += 2;
                    continue;
                }
                if b == q || b == b'\n' || b == b'\r' {
                    break;
                }
                k += 1;
            }
            let end = k.min(bytes.len());
            let value = bytes.get(start..end)?;
            (!value.is_empty() && value != REDACTED.as_bytes()).then_some((start, end))
        }
        Some(_) => {
            let unquoted = |b: u8| {
                b.is_ascii_graphic()
                    && !matches!(
                        b,
                        b',' | b';'
                            | b'('
                            | b')'
                            | b'<'
                            | b'>'
                            | b'{'
                            | b'}'
                            | b'['
                            | b']'
                            | b'"'
                            | b'\''
                    )
            };
            let n = run_len(bytes, j, unquoted);
            let value = bytes.get(j..j + n)?;
            let has_digit = value.iter().any(u8::is_ascii_digit);
            let has_alpha = value.iter().any(u8::is_ascii_alphabetic);
            let digit_rule = n >= 6 && has_digit && has_alpha;
            let strong_rule = kind == SecretKeyKind::Strong
                && n >= 8
                && !looks_like_code_value(value, bytes.get(j + n).copied());
            (digit_rule || strong_rule).then_some((j, j + n))
        }
        None => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn masked(s: &str) -> String {
        mask_secrets(s).text
    }

    #[test]
    fn masks_aws_access_key_ids() {
        let out = mask_secrets("aws_key_id AKIAIOSFODNN7EXAMPLE and ASIAABCDEFGHIJKLMNOP.");
        assert_eq!(out.text, "aws_key_id [REDACTED] and [REDACTED].");
        assert_eq!(out.findings, 2);
        // Wrong length / embedded in a word is left alone.
        assert_eq!(masked("AKIASHORT1"), "AKIASHORT1");
        assert_eq!(masked("XAKIAIOSFODNN7EXAMPLE"), "XAKIAIOSFODNN7EXAMPLE");
    }

    #[test]
    fn masks_github_tokens() {
        for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
            let input = format!("t {prefix}ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 end");
            assert_eq!(masked(&input), "t [REDACTED] end", "prefix {prefix}");
        }
        assert_eq!(
            masked("github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz"),
            "[REDACTED]"
        );
    }

    #[test]
    fn masks_openai_and_anthropic_keys() {
        assert_eq!(
            masked("OPENAI=sk-proj-abcDEF1234567890ghijKLMN"),
            "OPENAI=[REDACTED]"
        );
        assert_eq!(
            masked("key sk-ant-api03-AbCdEf0123456789_xyz-QRSTUV ok"),
            "key [REDACTED] ok"
        );
        // Not a key: CSS-ish identifiers without digits, or word-embedded `sk-`.
        assert_eq!(
            masked("class task-runner-default-layout"),
            "class task-runner-default-layout"
        );
        assert_eq!(masked("sk-short1"), "sk-short1");
    }

    #[test]
    fn masks_google_api_keys() {
        assert_eq!(
            masked("k=AIzaSyA1234567890abcdefghijklmnopqrstuv"),
            "k=[REDACTED]"
        );
    }

    #[test]
    fn masks_slack_tokens() {
        assert_eq!(
            masked("xoxb-123456789012-abcdefghijkl and xoxp-1-2-3-abcdef0123"),
            "[REDACTED] and [REDACTED]"
        );
        assert_eq!(masked("xoxz-1234567890abc"), "xoxz-1234567890abc");
    }

    #[test]
    fn masks_jwts() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        assert_eq!(masked(&format!("Bearer {jwt}")), "Bearer [REDACTED]");
    }

    #[test]
    fn masks_entire_pem_private_key_block() {
        let input = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\nabc+/=\n-----END RSA PRIVATE KEY-----\nafter";
        let out = mask_secrets(input);
        assert_eq!(out.text, "before\n[REDACTED]\nafter");
        assert_eq!(out.findings, 1);
        // Diff-prefixed lines are covered too.
        let diff = "+-----BEGIN OPENSSH PRIVATE KEY-----\n+b3BlbnNzaC1rZXktdjEA\n+-----END OPENSSH PRIVATE KEY-----\n";
        assert_eq!(masked(diff), "+[REDACTED]\n");
        // Truncated block without END is masked to end of text.
        assert_eq!(
            masked("x\n-----BEGIN PRIVATE KEY-----\nMIIE"),
            "x\n[REDACTED]"
        );
        // Public material is untouched.
        let cert = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----";
        assert_eq!(masked(cert), cert);
    }

    #[test]
    fn masks_generic_assignment_values_only() {
        assert_eq!(
            masked(r#"let password = "hunter2";"#),
            r#"let password = "[REDACTED]";"#
        );
        assert_eq!(
            masked(r#"{"client_secret": "abc def", "user": "bob"}"#),
            r#"{"client_secret": "[REDACTED]", "user": "bob"}"#
        );
        assert_eq!(masked("DB_PASSWORD=s3cretValue9"), "DB_PASSWORD=[REDACTED]");
        assert_eq!(masked("api_key: 'xyz'"), "api_key: '[REDACTED]'");
        assert_eq!(
            masked("authToken := \"t0k\""),
            "authToken := \"[REDACTED]\""
        );
        assert_eq!(
            masked("+  secret: abcd1234efgh\n"),
            "+  secret: [REDACTED]\n"
        );
    }

    #[test]
    fn masks_new_vendor_token_formats() {
        for tok in [
            "sk_live_4eC39HqLyjWDarjtT1zdp7dc",
            "sk_test_4eC39HqLyjWDarjtT1zdp7dc",
            "rk_live_51H8abcdEFGHijklMNOP",
            "rk_test_51H8abcdEFGHijklMNOP",
            "glpat-xxxxXXXX1234yyyyZZZZ",
            "npm_abcdefghijklmnopqrstuvwxyz0123456789",
            "pypi-AgEIcHlwaS5vcmcCJGFiY2RlZmdoLWlqa2wtbW5vcC0xMjM0",
        ] {
            assert_eq!(masked(&format!("x {tok} y")), "x [REDACTED] y", "{tok}");
        }
        // Publishable Stripe keys are public; short look-alikes are untouched.
        let pk = "pk_live_4eC39HqLyjWDarjtT1zdp7dc";
        assert_eq!(masked(pk), pk);
        assert_eq!(masked("npm_install"), "npm_install");
        assert_eq!(masked("glpat-short"), "glpat-short");
    }

    #[test]
    fn masks_authorization_headers() {
        assert_eq!(
            masked("Authorization: Bearer abcdef0123456789ABCDEF"),
            "Authorization: Bearer [REDACTED]"
        );
        assert_eq!(
            masked("curl -H 'authorization: bearer abc.def-ghi_jkl~mno+pqr/stu=='"),
            "curl -H 'authorization: bearer [REDACTED]'"
        );
        assert_eq!(
            masked("headers[\"Authorization\"] = \"Basic dXNlcjpwYXNzd29yZA==\""),
            "headers[\"Authorization\"] = \"Basic [REDACTED]\""
        );
        // Prose and short values are left alone.
        let prose = "Bearer authentication. Basic configuration is documented below.";
        assert_eq!(masked(prose), prose);
        assert_eq!(masked("Bearer {token}"), "Bearer {token}");
    }

    #[test]
    fn masks_url_credentials() {
        assert_eq!(
            masked("DATABASE_URL=postgres://admin:hunter2@db.internal:5432/app"),
            "DATABASE_URL=postgres://admin:[REDACTED]@db.internal:5432/app"
        );
        assert_eq!(
            masked("git clone https://x-access:abcdEFGH1234ijklMNOP@github.com/o/r.git"),
            "git clone https://x-access:[REDACTED]@github.com/o/r.git"
        );
        assert_eq!(
            masked("https://a1b2c3d4e5f6g7h8i9j0k1l2:x-oauth-basic@github.com/o/r"),
            "https://[REDACTED]:[REDACTED]@github.com/o/r"
        );
        assert_eq!(
            masked("https://a1b2c3d4e5f6g7h8i9j0k1l2@github.com/o/r"),
            "https://[REDACTED]@github.com/o/r"
        );
        // Plain URLs, emails, and login-only userinfo are untouched.
        for s in [
            "https://github.com/o/r",
            "ssh://git@github.com/o/r.git",
            "mailto:bob@example.com",
            "http://localhost:8080/path?q=a@b",
        ] {
            assert_eq!(masked(s), s);
        }
    }

    #[test]
    fn masks_strong_assignment_values_without_digits() {
        assert_eq!(
            masked("DB_PASSWORD=correcthorsebatterystaple"),
            "DB_PASSWORD=[REDACTED]"
        );
        assert_eq!(
            masked("+  passphrase: openSesameNow\n"),
            "+  passphrase: [REDACTED]\n"
        );
        assert_eq!(
            masked("aws_credentials = supersecretvalue"),
            "aws_credentials = [REDACTED]"
        );
        assert_eq!(
            masked("client_secret: notarealsecretvalue"),
            "client_secret: [REDACTED]"
        );
        assert_eq!(
            masked(r#"{"auth": "dXNlcjpwYXNz", "credential": "abc"}"#),
            r#"{"auth": "[REDACTED]", "credential": "[REDACTED]"}"#
        );
        // Quoted values of token-like keys are masked even without digits.
        assert_eq!(masked("token = \"abcdefgh\""), "token = \"[REDACTED]\"");
    }

    #[test]
    fn strong_assignment_rule_skips_code() {
        let code = r#"
let token_count = 5;
struct Creds { password: String, token: Option<String> }
struct S { password: PasswordHash, secret: SecretString }
let password = get_password();
let password = self.password_field;
let secret = Secret::new(raw);
password: ${DB_PASSWORD}
secret = &config.client_secret;
let token = lexer.next_token();
fn check(password: &str) {}
"#;
        let out = mask_secrets(code);
        assert_eq!(out.text, code);
        assert_eq!(out.findings, 0);
    }

    #[test]
    fn leaves_normal_code_untouched() {
        let code = r#"
let token_count = 5;
let password_len = input.len();
fn next_token(&mut self) -> Option<Token> { None }
if token == expected { return; }
let token = lexer.next();
struct Creds { password: String, token: Option<String> }
use crate::token::Kind;
match token { Token::Ident => {}, _ => {} }
let secret: &str = &cfg.secret;
println!("{}", "hello world");
"#;
        let out = mask_secrets(code);
        assert_eq!(out.text, code);
        assert_eq!(out.findings, 0);
    }

    #[test]
    fn is_idempotent() {
        let once = masked(r#"password = "hunter2" ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123"#);
        let twice = mask_secrets(&once);
        assert_eq!(twice.text, once);
        assert_eq!(twice.findings, 0);
    }

    #[test]
    fn never_panics_on_multibyte_utf8() {
        let samples = [
            "パスワード password = \"秘密🔑\" ok",
            "token: 日本語テキスト123abc",
            "émoji 🔥 sk-",
            "-----BEGIN PRIVATE KEY-----\n鍵🔑",
            "password=\"unterminated ünïcode",
            "eyJ🔥.eyJ.",
            "xoxb-",
            "AKIA",
            "\"",
            "",
        ];
        for s in samples {
            let _ = mask_secrets(s);
            for cut in 0..s.len() {
                if let Some(prefix) = s.get(..cut) {
                    let _ = mask_secrets(prefix);
                }
            }
        }
        assert_eq!(
            masked("パスワード password = \"秘密🔑\" ok"),
            "パスワード password = \"[REDACTED]\" ok"
        );
    }

    #[test]
    fn detects_sensitive_paths() {
        for p in [
            ".env",
            "config/.env.production",
            "certs/server.pem",
            "tls/private.key",
            "store.p12",
            "a/b/cert.PFX",
            "/home/u/.ssh/id_rsa",
            ".ssh/id_ed25519.pub",
            "android/release.keystore",
            "gcp/credentials.json",
            ".npmrc",
            "pkg/.pypirc",
            "config/secrets.yaml",
            "win\\path\\secrets.json",
            "/home/u/.aws/credentials",
            "web/.htpasswd",
            "kubeconfig",
            "/home/u/.kube/config",
            "infra/prod.tfvars",
            "infra/prod.tfvars.json",
            "infra/terraform.tfstate",
            "infra/terraform.tfstate.backup",
            "gcp/service-account-prod.json",
            "gcp/service_account.json",
            "deploy/prod.env",
            "/home/u/.ssh/config",
            "/home/u/.ssh/known_hosts",
            "k8s/secrets/db.yaml",
            "app/.secrets/token.txt",
            "/home/u/.gnupg/secring.gpg",
            "/home/u/.docker/config.json",
        ] {
            assert!(is_sensitive_path(p), "{p} should be sensitive");
        }
        for p in [
            "src/main.rs",
            "environment.rs",
            "docs/keys.md",
            "src/keyboard.rs",
            "README.md",
            "credentials_test.rs",
            "src/credentials.rs",
            "/home/u/.ssh/authorized_keys.pub",
            "config.json",
            "src/config",
            "docs/secrets.md.txt/readme",
            "src/secrets_manager.rs",
            "infra/main.tf",
            "pubring.gpg",
            "",
        ] {
            assert!(!is_sensitive_path(p), "{p} should not be sensitive");
        }
    }

    #[test]
    fn filters_sensitive_sections_from_multi_file_diff() {
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
index 111..222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1 +1 @@
-old
+new
diff --git a/.env b/.env
new file mode 100644
--- /dev/null
+++ b/.env
@@ -0,0 +1 @@
+DATABASE_URL=postgres://u:p@h/db
diff --git a/certs/server.key b/certs/server.key
--- a/certs/server.key
+++ b/certs/server.key
@@ -1 +1 @@
+MIIEsecret
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
+docs
";
        let out = filter_sensitive_diff(diff);
        assert_eq!(out.dropped_paths, vec![".env", "certs/server.key"]);
        assert!(out.text.contains("+new"));
        assert!(out.text.contains("+docs"));
        assert!(!out.text.contains("DATABASE_URL"));
        assert!(!out.text.contains("MIIEsecret"));
        assert!(
            out.text
                .contains("# [omitted diff of sensitive file: .env]")
        );
    }
}
