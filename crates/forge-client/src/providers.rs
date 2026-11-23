//! HTTP REST implementations of [`ForgeProvider`] for GitHub, GitLab, Bitbucket, and Gitea.

use crate::traits::{
    CiCheckRun, CiCheckStatus, CreatePullRequestInput, ForgeError, ForgeKind, ForgeProvider,
    ForgeRemoteTarget, ForgeResult, PullRequestState, PullRequestSummary, TokenVault,
};
use async_trait::async_trait;
use branvio_core::ids::GitOid;
use compact_str::CompactString;
use reqwest::header::HeaderMap;
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

/// Default account key checked inside [`TokenVault`] when user has not overridden an account label.
pub const DEFAULT_TOKEN_ACCOUNT: &str = "default";

struct ForgeHttpCore {
    client: Client,
    vault: Option<Arc<dyn TokenVault>>,
    account: String,
}

impl ForgeHttpCore {
    fn new(vault: Option<Arc<dyn TokenVault>>, account: Option<String>) -> ForgeResult<Self> {
        let client = Client::builder()
            .user_agent("Branvio/0.1.0")
            .timeout(Duration::from_secs(20))
            // Tokens are attached per request under several header names (Authorization,
            // GitLab `PRIVATE-TOKEN`). reqwest only strips the standard ones on cross-host
            // redirects, so never follow a redirect that leaves the original origin.
            .redirect(same_origin_redirect_policy())
            .referer(false)
            .build()?;
        Ok(Self {
            client,
            vault,
            account: account.unwrap_or_else(|| DEFAULT_TOKEN_ACCOUNT.to_string()),
        })
    }

    fn lookup_token(&self, target: &ForgeRemoteTarget) -> Option<String> {
        let vault = self.vault.as_ref()?;
        // Never hand a token to a plaintext or unparsable API origin.
        if !token_transport_allowed(&target.api_base_url) {
            return None;
        }
        // Only the configured account label is consulted. The repository owner is NOT used
        // as a vault key: it comes from an untrusted remote URL, so it must not choose which
        // stored credential gets sent.
        token_host_candidates(&extract_host(&target.api_base_url))
            .into_iter()
            .find_map(|h| match vault.load_token(target.kind, &h, &self.account) {
                Ok(Some(tok)) if !tok.trim().is_empty() => Some(tok),
                _ => None,
            })
    }

    async fn send_json<T: for<'de> Deserialize<'de>>(
        &self,
        req: RequestBuilder,
        host: &str,
    ) -> ForgeResult<T> {
        let resp = req.send().await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(map_http_error(host, status, &headers, &body));
        }
        serde_json::from_str(&body)
            .map_err(|e| ForgeError::Protocol(format!("invalid JSON from {host}: {e}")))
    }
}

/// GitHub & GitHub Enterprise REST API v3 adapter.
pub struct GitHubForgeProvider {
    http: Arc<ForgeHttpCore>,
}

/// GitLab.com & self-managed GitLab REST API v4 adapter.
pub struct GitLabForgeProvider {
    http: Arc<ForgeHttpCore>,
}

/// Atlassian Bitbucket REST API 2.0 adapter.
pub struct BitbucketForgeProvider {
    http: Arc<ForgeHttpCore>,
}

/// Gitea / Forgejo REST API v1 adapter.
pub struct GiteaForgeProvider {
    http: Arc<ForgeHttpCore>,
}

/// Multi-forge router dispatching requests according to [`ForgeRemoteTarget::kind`].
pub struct UnifiedForgeProvider {
    github: GitHubForgeProvider,
    gitlab: GitLabForgeProvider,
    bitbucket: BitbucketForgeProvider,
    gitea: GiteaForgeProvider,
}

impl UnifiedForgeProvider {
    /// Creates a [`UnifiedForgeProvider`] sharing one connection pool and optional [`TokenVault`].
    pub fn new(vault: Option<Arc<dyn TokenVault>>) -> ForgeResult<Self> {
        let core = Arc::new(ForgeHttpCore::new(vault, None)?);
        Ok(Self {
            github: GitHubForgeProvider {
                http: Arc::clone(&core),
            },
            gitlab: GitLabForgeProvider {
                http: Arc::clone(&core),
            },
            bitbucket: BitbucketForgeProvider {
                http: Arc::clone(&core),
            },
            gitea: GiteaForgeProvider { http: core },
        })
    }

    fn delegate(&self, kind: ForgeKind) -> &dyn ForgeProvider {
        match kind {
            ForgeKind::GitHub => &self.github,
            ForgeKind::GitLab => &self.gitlab,
            ForgeKind::Bitbucket => &self.bitbucket,
            ForgeKind::Gitea => &self.gitea,
        }
    }
}

impl GitHubForgeProvider {
    /// Builds a standalone GitHub provider with optional token vault.
    pub fn new(vault: Option<Arc<dyn TokenVault>>) -> ForgeResult<Self> {
        Ok(Self {
            http: Arc::new(ForgeHttpCore::new(vault, None)?),
        })
    }
}

impl GitLabForgeProvider {
    /// Builds a standalone GitLab provider with optional token vault.
    pub fn new(vault: Option<Arc<dyn TokenVault>>) -> ForgeResult<Self> {
        Ok(Self {
            http: Arc::new(ForgeHttpCore::new(vault, None)?),
        })
    }
}

impl BitbucketForgeProvider {
    /// Builds a standalone Bitbucket provider with optional token vault.
    pub fn new(vault: Option<Arc<dyn TokenVault>>) -> ForgeResult<Self> {
        Ok(Self {
            http: Arc::new(ForgeHttpCore::new(vault, None)?),
        })
    }
}

impl GiteaForgeProvider {
    /// Builds a standalone Gitea / Forgejo provider with optional token vault.
    pub fn new(vault: Option<Arc<dyn TokenVault>>) -> ForgeResult<Self> {
        Ok(Self {
            http: Arc::new(ForgeHttpCore::new(vault, None)?),
        })
    }
}

#[async_trait]
impl ForgeProvider for UnifiedForgeProvider {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitHub
    }

    async fn list_pull_requests(
        &self,
        target: &ForgeRemoteTarget,
        state_filter: Option<PullRequestState>,
    ) -> ForgeResult<Vec<PullRequestSummary>> {
        self.delegate(target.kind)
            .list_pull_requests(target, state_filter)
            .await
    }

    async fn fetch_pr_checks(
        &self,
        target: &ForgeRemoteTarget,
        pr_number: u64,
    ) -> ForgeResult<Vec<CiCheckRun>> {
        self.delegate(target.kind)
            .fetch_pr_checks(target, pr_number)
            .await
    }

    async fn create_pull_request(
        &self,
        target: &ForgeRemoteTarget,
        input: CreatePullRequestInput,
    ) -> ForgeResult<PullRequestSummary> {
        self.delegate(target.kind)
            .create_pull_request(target, input)
            .await
    }

    fn local_checkout_refspec(&self, pr_number: u64) -> String {
        self.github.local_checkout_refspec(pr_number)
    }
}

#[derive(Deserialize)]
struct GhRef {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
}

#[derive(Deserialize)]
struct GhUser {
    login: String,
}

#[derive(Deserialize)]
struct GhPull {
    number: u64,
    title: String,
    body: Option<String>,
    state: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    merged_at: Option<String>,
    html_url: String,
    user: GhUser,
    head: GhRef,
    base: GhRef,
}

#[derive(Deserialize)]
struct GhCheckRunsResponse {
    check_runs: Vec<GhCheckRun>,
}

#[derive(Deserialize)]
struct GhCheckRun {
    name: String,
    status: String,
    conclusion: Option<String>,
    html_url: Option<String>,
}

#[async_trait]
impl ForgeProvider for GitHubForgeProvider {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitHub
    }

    async fn list_pull_requests(
        &self,
        target: &ForgeRemoteTarget,
        state_filter: Option<PullRequestState>,
    ) -> ForgeResult<Vec<PullRequestSummary>> {
        let base = target.api_base_url.trim_end_matches('/');
        let host = extract_host(base);
        let state_param = match state_filter {
            None | Some(PullRequestState::Open) | Some(PullRequestState::Draft) => "open",
            Some(PullRequestState::Merged) | Some(PullRequestState::Closed) => "closed",
        };
        let url = format!(
            "{base}/repos/{}/{}/pulls?state={state_param}&per_page=50",
            target.owner, target.repo_name
        );
        let mut req = self
            .http
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json");
        if let Some(tok) = self.http.lookup_token(target) {
            req = req.bearer_auth(tok);
        }
        let pulls: Vec<GhPull> = self.http.send_json(req, &host).await?;
        let mut mapped: Vec<PullRequestSummary> = pulls
            .into_iter()
            .map(map_gh_pull)
            .filter(|pr| state_filter.is_none_or(|f| pr.state == f))
            .collect();
        for pr in &mut mapped {
            if let Ok(checks) = self.fetch_pr_checks(target, pr.number).await {
                pr.ci_status = aggregate_ci_status(&checks);
                pr.checks = checks;
            }
        }
        Ok(mapped)
    }

    async fn fetch_pr_checks(
        &self,
        target: &ForgeRemoteTarget,
        pr_number: u64,
    ) -> ForgeResult<Vec<CiCheckRun>> {
        let base = target.api_base_url.trim_end_matches('/');
        let host = extract_host(base);
        let pr_url = format!(
            "{base}/repos/{}/{}/pulls/{pr_number}",
            target.owner, target.repo_name
        );
        let mut pr_req = self
            .http
            .client
            .get(pr_url)
            .header("Accept", "application/vnd.github+json");
        if let Some(tok) = self.http.lookup_token(target) {
            pr_req = pr_req.bearer_auth(tok);
        }
        let pr: GhPull = self.http.send_json(pr_req, &host).await?;
        let checks_url = format!(
            "{base}/repos/{}/{}/commits/{}/check-runs",
            target.owner, target.repo_name, pr.head.sha
        );
        let mut checks_req = self
            .http
            .client
            .get(checks_url)
            .header("Accept", "application/vnd.github+json");
        if let Some(tok) = self.http.lookup_token(target) {
            checks_req = checks_req.bearer_auth(tok);
        }
        let resp: GhCheckRunsResponse = self.http.send_json(checks_req, &host).await?;
        Ok(resp
            .check_runs
            .into_iter()
            .map(|c| {
                let status = if c.status != "completed" {
                    CiCheckStatus::Pending
                } else if matches!(
                    c.conclusion.as_deref(),
                    Some("success" | "neutral" | "skipped")
                ) {
                    CiCheckStatus::Passing
                } else {
                    CiCheckStatus::Failing
                };
                CiCheckRun {
                    name: CompactString::from(c.name),
                    status,
                    details_url: c.html_url,
                }
            })
            .collect())
    }

    async fn create_pull_request(
        &self,
        target: &ForgeRemoteTarget,
        input: CreatePullRequestInput,
    ) -> ForgeResult<PullRequestSummary> {
        let base = target.api_base_url.trim_end_matches('/');
        let host = extract_host(base);
        let url = format!("{base}/repos/{}/{}/pulls", target.owner, target.repo_name);
        let payload = serde_json::json!({
            "title": input.title,
            "body": input.body,
            "head": input.head_branch.as_str(),
            "base": input.base_branch.as_str(),
            "draft": input.draft,
        });
        let mut req = self
            .http
            .client
            .post(url)
            .header("Accept", "application/vnd.github+json")
            .json(&payload);
        if let Some(tok) = self.http.lookup_token(target) {
            req = req.bearer_auth(tok);
        }
        let pr: GhPull = self.http.send_json(req, &host).await?;
        Ok(map_gh_pull(pr))
    }

    fn local_checkout_refspec(&self, pr_number: u64) -> String {
        format!("+refs/pull/{pr_number}/head:refs/remotes/origin/pr/{pr_number}")
    }
}

fn map_gh_pull(pr: GhPull) -> PullRequestSummary {
    let state = if pr.merged_at.is_some() {
        PullRequestState::Merged
    } else if pr.state == "closed" {
        PullRequestState::Closed
    } else if pr.draft {
        PullRequestState::Draft
    } else {
        PullRequestState::Open
    };
    PullRequestSummary {
        number: pr.number,
        title: pr.title,
        body: pr.body.unwrap_or_default(),
        author: CompactString::from(pr.user.login),
        state,
        head_branch: CompactString::from(pr.head.ref_name),
        base_branch: CompactString::from(pr.base.ref_name),
        head_oid: GitOid::from_hex(&pr.head.sha).ok(),
        ci_status: CiCheckStatus::None,
        checks: Vec::new(),
        web_url: pr.html_url,
    }
}

#[derive(Deserialize)]
struct GlAuthor {
    username: String,
}

#[derive(Deserialize)]
struct GlMergeRequest {
    iid: u64,
    title: String,
    description: Option<String>,
    state: String,
    #[serde(default)]
    draft: bool,
    source_branch: String,
    target_branch: String,
    sha: Option<String>,
    web_url: String,
    author: GlAuthor,
}

#[derive(Deserialize)]
struct GlPipeline {
    id: u64,
    status: String,
    web_url: Option<String>,
}

fn gitlab_encoded_project(target: &ForgeRemoteTarget) -> String {
    let raw = format!("{}/{}", target.owner, target.repo_name);
    raw.replace('/', "%2F")
}

#[async_trait]
impl ForgeProvider for GitLabForgeProvider {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitLab
    }

    async fn list_pull_requests(
        &self,
        target: &ForgeRemoteTarget,
        state_filter: Option<PullRequestState>,
    ) -> ForgeResult<Vec<PullRequestSummary>> {
        let base = target.api_base_url.trim_end_matches('/');
        let host = extract_host(base);
        let proj = gitlab_encoded_project(target);
        let state_q = match state_filter {
            None | Some(PullRequestState::Open) | Some(PullRequestState::Draft) => "opened",
            Some(PullRequestState::Merged) => "merged",
            Some(PullRequestState::Closed) => "closed",
        };
        let url = format!("{base}/projects/{proj}/merge_requests?state={state_q}&per_page=50");
        let mut req = self.http.client.get(url);
        if let Some(tok) = self.http.lookup_token(target) {
            req = req.header("PRIVATE-TOKEN", tok);
        }
        let mrs: Vec<GlMergeRequest> = self.http.send_json(req, &host).await?;
        Ok(mrs
            .into_iter()
            .map(map_gl_mr)
            .filter(|pr| state_filter.is_none_or(|f| pr.state == f))
            .collect())
    }

    async fn fetch_pr_checks(
        &self,
        target: &ForgeRemoteTarget,
        pr_number: u64,
    ) -> ForgeResult<Vec<CiCheckRun>> {
        let base = target.api_base_url.trim_end_matches('/');
        let host = extract_host(base);
        let proj = gitlab_encoded_project(target);
        let url = format!("{base}/projects/{proj}/merge_requests/{pr_number}/pipelines");
        let mut req = self.http.client.get(url);
        if let Some(tok) = self.http.lookup_token(target) {
            req = req.header("PRIVATE-TOKEN", tok);
        }
        let pipes: Vec<GlPipeline> = self.http.send_json(req, &host).await?;
        Ok(pipes
            .into_iter()
            .map(|p| {
                let status = match p.status.as_str() {
                    "success" => CiCheckStatus::Passing,
                    "failed" | "canceled" => CiCheckStatus::Failing,
                    _ => CiCheckStatus::Pending,
                };
                CiCheckRun {
                    name: CompactString::from(format!("pipeline #{}", p.id)),
                    status,
                    details_url: p.web_url,
                }
            })
            .collect())
    }

    async fn create_pull_request(
        &self,
        target: &ForgeRemoteTarget,
        input: CreatePullRequestInput,
    ) -> ForgeResult<PullRequestSummary> {
        let base = target.api_base_url.trim_end_matches('/');
        let host = extract_host(base);
        let proj = gitlab_encoded_project(target);
        let title = if input.draft && !input.title.starts_with("Draft:") {
            format!("Draft: {}", input.title)
        } else {
            input.title
        };
        let url = format!("{base}/projects/{proj}/merge_requests");
        let payload = serde_json::json!({
            "source_branch": input.head_branch.as_str(),
            "target_branch": input.base_branch.as_str(),
            "title": title,
            "description": input.body,
        });
        let mut req = self.http.client.post(url).json(&payload);
        if let Some(tok) = self.http.lookup_token(target) {
            req = req.header("PRIVATE-TOKEN", tok);
        }
        let mr: GlMergeRequest = self.http.send_json(req, &host).await?;
        Ok(map_gl_mr(mr))
    }

    fn local_checkout_refspec(&self, pr_number: u64) -> String {
        format!("+refs/merge-requests/{pr_number}/head:refs/remotes/origin/mr/{pr_number}")
    }
}

fn map_gl_mr(mr: GlMergeRequest) -> PullRequestSummary {
    let state = match mr.state.as_str() {
        "merged" => PullRequestState::Merged,
        "closed" => PullRequestState::Closed,
        _ if mr.draft || mr.title.starts_with("Draft:") => PullRequestState::Draft,
        _ => PullRequestState::Open,
    };
    PullRequestSummary {
        number: mr.iid,
        title: mr.title,
        body: mr.description.unwrap_or_default(),
        author: CompactString::from(mr.author.username),
        state,
        head_branch: CompactString::from(mr.source_branch),
        base_branch: CompactString::from(mr.target_branch),
        head_oid: mr.sha.as_deref().and_then(|h| GitOid::from_hex(h).ok()),
        ci_status: CiCheckStatus::None,
        checks: Vec::new(),
        web_url: mr.web_url,
    }
}

#[derive(Deserialize)]
struct BbBranch {
    name: String,
}

#[derive(Deserialize)]
struct BbCommit {
    hash: String,
}

#[derive(Deserialize)]
struct BbEndpoint {
    branch: BbBranch,
    commit: Option<BbCommit>,
}

#[derive(Deserialize)]
struct BbAuthor {
    display_name: String,
}

#[derive(Deserialize)]
struct BbLink {
    href: String,
}

#[derive(Deserialize)]
struct BbLinks {
    html: Option<BbLink>,
}

#[derive(Deserialize)]
struct BbPullRequest {
    id: u64,
    title: String,
    description: Option<String>,
    state: String,
    author: BbAuthor,
    source: BbEndpoint,
    destination: BbEndpoint,
    links: Option<BbLinks>,
}

#[derive(Deserialize)]
struct BbListResp<T> {
    values: Vec<T>,
}

#[derive(Deserialize)]
struct BbStatus {
    name: Option<String>,
    key: String,
    state: String,
    url: Option<String>,
}

#[async_trait]
impl ForgeProvider for BitbucketForgeProvider {
    fn kind(&self) -> ForgeKind {
        ForgeKind::Bitbucket
    }

    async fn list_pull_requests(
        &self,
        target: &ForgeRemoteTarget,
        state_filter: Option<PullRequestState>,
    ) -> ForgeResult<Vec<PullRequestSummary>> {
        let base = target.api_base_url.trim_end_matches('/');
        let host = extract_host(base);
        let state_q = match state_filter {
            None | Some(PullRequestState::Open) | Some(PullRequestState::Draft) => "OPEN",
            Some(PullRequestState::Merged) => "MERGED",
            Some(PullRequestState::Closed) => "DECLINED",
        };
        let url = format!(
            "{base}/repositories/{}/{}/pullrequests?state={state_q}",
            target.owner, target.repo_name
        );
        let mut req = self.http.client.get(url);
        if let Some(tok) = self.http.lookup_token(target) {
            req = req.bearer_auth(tok);
        }
        let resp: BbListResp<BbPullRequest> = self.http.send_json(req, &host).await?;
        Ok(resp.values.into_iter().map(map_bb_pr).collect())
    }

    async fn fetch_pr_checks(
        &self,
        target: &ForgeRemoteTarget,
        pr_number: u64,
    ) -> ForgeResult<Vec<CiCheckRun>> {
        let base = target.api_base_url.trim_end_matches('/');
        let host = extract_host(base);
        let url = format!(
            "{base}/repositories/{}/{}/pullrequests/{pr_number}/statuses",
            target.owner, target.repo_name
        );
        let mut req = self.http.client.get(url);
        if let Some(tok) = self.http.lookup_token(target) {
            req = req.bearer_auth(tok);
        }
        let resp: BbListResp<BbStatus> = self.http.send_json(req, &host).await?;
        Ok(resp
            .values
            .into_iter()
            .map(|s| {
                let status = match s.state.as_str() {
                    "SUCCESSFUL" => CiCheckStatus::Passing,
                    "FAILED" | "STOPPED" => CiCheckStatus::Failing,
                    _ => CiCheckStatus::Pending,
                };
                CiCheckRun {
                    name: CompactString::from(s.name.unwrap_or(s.key)),
                    status,
                    details_url: s.url,
                }
            })
            .collect())
    }

    async fn create_pull_request(
        &self,
        target: &ForgeRemoteTarget,
        input: CreatePullRequestInput,
    ) -> ForgeResult<PullRequestSummary> {
        let base = target.api_base_url.trim_end_matches('/');
        let host = extract_host(base);
        let url = format!(
            "{base}/repositories/{}/{}/pullrequests",
            target.owner, target.repo_name
        );
        let payload = serde_json::json!({
            "title": input.title,
            "description": input.body,
            "source": { "branch": { "name": input.head_branch.as_str() } },
            "destination": { "branch": { "name": input.base_branch.as_str() } },
        });
        let mut req = self.http.client.post(url).json(&payload);
        if let Some(tok) = self.http.lookup_token(target) {
            req = req.bearer_auth(tok);
        }
        let pr: BbPullRequest = self.http.send_json(req, &host).await?;
        Ok(map_bb_pr(pr))
    }

    fn local_checkout_refspec(&self, pr_number: u64) -> String {
        format!("+refs/pull-requests/{pr_number}/from:refs/remotes/origin/pr/{pr_number}")
    }
}

fn map_bb_pr(pr: BbPullRequest) -> PullRequestSummary {
    let state = match pr.state.as_str() {
        "MERGED" => PullRequestState::Merged,
        "DECLINED" | "SUPERSEDED" => PullRequestState::Closed,
        _ => PullRequestState::Open,
    };
    PullRequestSummary {
        number: pr.id,
        title: pr.title,
        body: pr.description.unwrap_or_default(),
        author: CompactString::from(pr.author.display_name),
        state,
        head_branch: CompactString::from(pr.source.branch.name),
        base_branch: CompactString::from(pr.destination.branch.name),
        head_oid: pr
            .source
            .commit
            .as_ref()
            .and_then(|c| GitOid::from_hex(&c.hash).ok()),
        ci_status: CiCheckStatus::None,
        checks: Vec::new(),
        web_url: pr
            .links
            .and_then(|l| l.html)
            .map(|h| h.href)
            .unwrap_or_default(),
    }
}

#[derive(Deserialize)]
struct GtStatus {
    context: Option<String>,
    status: String,
    target_url: Option<String>,
}

#[async_trait]
impl ForgeProvider for GiteaForgeProvider {
    fn kind(&self) -> ForgeKind {
        ForgeKind::Gitea
    }

    async fn list_pull_requests(
        &self,
        target: &ForgeRemoteTarget,
        state_filter: Option<PullRequestState>,
    ) -> ForgeResult<Vec<PullRequestSummary>> {
        let base = target.api_base_url.trim_end_matches('/');
        let host = extract_host(base);
        let state_q = match state_filter {
            None | Some(PullRequestState::Open) | Some(PullRequestState::Draft) => "open",
            Some(PullRequestState::Merged) | Some(PullRequestState::Closed) => "closed",
        };
        let url = format!(
            "{base}/repos/{}/{}/pulls?state={state_q}",
            target.owner, target.repo_name
        );
        let mut req = self.http.client.get(url);
        if let Some(tok) = self.http.lookup_token(target) {
            req = req.header("Authorization", format!("token {tok}"));
        }
        let pulls: Vec<GhPull> = self.http.send_json(req, &host).await?;
        Ok(pulls
            .into_iter()
            .map(map_gh_pull)
            .filter(|pr| state_filter.is_none_or(|f| pr.state == f))
            .collect())
    }

    async fn fetch_pr_checks(
        &self,
        target: &ForgeRemoteTarget,
        pr_number: u64,
    ) -> ForgeResult<Vec<CiCheckRun>> {
        let base = target.api_base_url.trim_end_matches('/');
        let host = extract_host(base);
        let pr_url = format!(
            "{base}/repos/{}/{}/pulls/{pr_number}",
            target.owner, target.repo_name
        );
        let mut pr_req = self.http.client.get(pr_url);
        if let Some(tok) = self.http.lookup_token(target) {
            pr_req = pr_req.header("Authorization", format!("token {tok}"));
        }
        let pr: GhPull = self.http.send_json(pr_req, &host).await?;
        let st_url = format!(
            "{base}/repos/{}/{}/statuses/{}",
            target.owner, target.repo_name, pr.head.sha
        );
        let mut st_req = self.http.client.get(st_url);
        if let Some(tok) = self.http.lookup_token(target) {
            st_req = st_req.header("Authorization", format!("token {tok}"));
        }
        let statuses: Vec<GtStatus> = self.http.send_json(st_req, &host).await?;
        Ok(statuses
            .into_iter()
            .map(|s| {
                let status = match s.status.as_str() {
                    "success" => CiCheckStatus::Passing,
                    "failure" | "error" => CiCheckStatus::Failing,
                    _ => CiCheckStatus::Pending,
                };
                CiCheckRun {
                    name: CompactString::from(s.context.unwrap_or_else(|| "ci".to_string())),
                    status,
                    details_url: s.target_url,
                }
            })
            .collect())
    }

    async fn create_pull_request(
        &self,
        target: &ForgeRemoteTarget,
        input: CreatePullRequestInput,
    ) -> ForgeResult<PullRequestSummary> {
        let base = target.api_base_url.trim_end_matches('/');
        let host = extract_host(base);
        let url = format!("{base}/repos/{}/{}/pulls", target.owner, target.repo_name);
        let payload = serde_json::json!({
            "title": input.title,
            "body": input.body,
            "head": input.head_branch.as_str(),
            "base": input.base_branch.as_str(),
        });
        let mut req = self.http.client.post(url).json(&payload);
        if let Some(tok) = self.http.lookup_token(target) {
            req = req.header("Authorization", format!("token {tok}"));
        }
        let pr: GhPull = self.http.send_json(req, &host).await?;
        Ok(map_gh_pull(pr))
    }

    fn local_checkout_refspec(&self, pr_number: u64) -> String {
        format!("+refs/pull/{pr_number}/head:refs/remotes/origin/pr/{pr_number}")
    }
}

fn aggregate_ci_status(checks: &[CiCheckRun]) -> CiCheckStatus {
    if checks.is_empty() {
        return CiCheckStatus::None;
    }
    if checks.iter().any(|c| c.status == CiCheckStatus::Failing) {
        return CiCheckStatus::Failing;
    }
    if checks.iter().any(|c| c.status == CiCheckStatus::Pending) {
        return CiCheckStatus::Pending;
    }
    CiCheckStatus::Passing
}

/// Maximum same-origin redirect hops (e.g. GitHub renamed-repository 301s).
const MAX_REDIRECTS: usize = 5;

/// Follows redirects only when scheme, host, and port are unchanged; otherwise returns the
/// 3xx response as-is so credentials never reach another origin.
fn same_origin_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        // Compare scheme/host/port directly; `Url::origin()` recurses on `blob:` URLs
        // (RUSTSEC-2026-0592) and must not be fed attacker-controlled `Location` values.
        let next = attempt.url();
        let same_origin = attempt.previous().last().is_some_and(|prev| {
            matches!(next.scheme(), "http" | "https")
                && prev.scheme() == next.scheme()
                && prev.host_str() == next.host_str()
                && prev.port_or_known_default() == next.port_or_known_default()
        });
        if same_origin {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

/// Tokens may only travel over HTTPS, or plain HTTP to a loopback address (local mocks /
/// SSH-tunnelled instances). Userinfo in the API base URL is treated as malformed.
fn token_transport_allowed(api_base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(api_base_url) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    match url.scheme() {
        "https" => true,
        "http" => url.host_str().is_some_and(|h| {
            let bare = h.trim_start_matches('[').trim_end_matches(']');
            h.eq_ignore_ascii_case("localhost")
                || bare
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }),
        _ => false,
    }
}

/// Canonical `host[:port]` (lowercase, punycode, default port omitted) of an API base URL,
/// or an empty string if it cannot be parsed. Parsed with the WHATWG URL parser so that
/// `user@host`, backslashes, and IDN tricks resolve exactly as the HTTP client will.
fn extract_host(url: &str) -> String {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return String::new();
    };
    let Some(host) = parsed.host_str() else {
        return String::new();
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    }
}

/// Vault host keys that may supply a token for the canonical API `host[:port]`.
///
/// Exact match only (port included), plus the web host of the two SaaS forges whose API lives
/// on a dedicated subdomain. There is deliberately no port-less fallback: a token stored for
/// `host` must never be sent to `host:<other-port>`, which may be a different service.
fn token_host_candidates(host: &str) -> Vec<String> {
    match host {
        "" => Vec::new(),
        "api.github.com" => vec![host.to_string(), "github.com".to_string()],
        "api.bitbucket.org" => vec![host.to_string(), "bitbucket.org".to_string()],
        _ => vec![host.to_string()],
    }
}

fn map_http_error(host: &str, status: StatusCode, headers: &HeaderMap, body: &str) -> ForgeError {
    let retry_after = headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    let rate_remaining_zero = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim() == "0");

    if status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN && rate_remaining_zero)
    {
        return ForgeError::RateLimited {
            host: host.to_string(),
            retry_after_secs: retry_after,
        };
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return ForgeError::Unauthorized {
            host: host.to_string(),
            message: format!("HTTP {}: {}", status.as_u16(), truncate_diag(body)),
        };
    }
    ForgeError::Protocol(format!(
        "HTTP {} from {host}: {}",
        status.as_u16(),
        truncate_diag(body)
    ))
}

fn truncate_diag(body: &str) -> &str {
    let mut end = 240.min(body.len());
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}
