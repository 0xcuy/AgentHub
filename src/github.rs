//! GitHub access for the channel GitHub integration.
//!
//! `GitHubTransport` is the swappable API seam, mirroring the embedding
//! provider design: the real client talks to the GitHub REST API with a
//! bearer token, and `MockGitHub` is a deterministic in-memory double so
//! features and tests never touch the network.
//!
//! The transport is opt-in and resolved from the environment at startup:
//! setting `GITHUB_TOKEN` activates the real client (default base URL
//! `https://api.github.com`, override with `AGHUB_GITHUB_API_URL` for GitHub
//! Enterprise Server or a test stub). Without a token the hub runs
//! `NoopGitHub` and GitHub endpoints answer `503 github_unavailable`.

use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Result of one GitHub API call: a JSON body or a status-carrying error.
pub type GitHubResult = Result<Value, GitHubApiError>;
/// Boxed future so the trait stays object-safe (`Arc<dyn GitHubTransport>`).
pub type GitHubFuture = Pin<Box<dyn Future<Output = GitHubResult> + Send>>;

/// Error from the GitHub transport. `status` is the HTTP status GitHub
/// answered with, or `None` when the request never completed.
#[derive(Debug, Clone)]
pub struct GitHubApiError {
    pub status: Option<u16>,
    pub message: String,
}

impl std::fmt::Display for GitHubApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(f, "github api error ({status}): {}", self.message),
            None => write!(f, "github request failed: {}", self.message),
        }
    }
}

/// Object-safe seam over the GitHub REST API. Only GETs are needed for the
/// MVP; `path` starts with `/`, `query` is the raw query string ("" if none).
pub trait GitHubTransport: Send + Sync {
    /// Stable transport name, surfaced in logs and diagnostics.
    fn name(&self) -> &'static str;
    /// True when the transport can actually reach GitHub.
    fn enabled(&self) -> bool;
    /// GET one GitHub API resource, preserving the caller's path and query.
    fn get(&self, path: &str, query: &str) -> GitHubFuture;
    /// POST a JSON body to one GitHub API resource (e.g. pull creation).
    fn post(&self, path: &str, body: Value) -> GitHubFuture;
}

/// Real client: GitHub REST API with a bearer token, 10s per-request timeout.
pub struct GitHubClient {
    http: reqwest::Client,
    token: String,
    base_url: String,
}

impl GitHubTransport for GitHubClient {
    fn name(&self) -> &'static str {
        "github"
    }

    fn enabled(&self) -> bool {
        true
    }

    fn get(&self, path: &str, query: &str) -> GitHubFuture {
        let url = if query.is_empty() {
            format!("{}{path}", self.base_url)
        } else {
            format!("{}{path}?{query}", self.base_url)
        };
        let http = self.http.clone();
        let token = self.token.clone();
        Box::pin(async move {
            let resp = http
                .get(url)
                .bearer_auth(&token)
                .header("User-Agent", "aghub")
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .map_err(|e| GitHubApiError { status: None, message: e.to_string() })?;
            let status = resp.status().as_u16();
            let body: Value = resp
                .json()
                .await
                .map_err(|e| GitHubApiError { status: Some(status), message: e.to_string() })?;
            if !(200..300).contains(&status) {
                let message = body
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("request failed")
                    .to_string();
                return Err(GitHubApiError { status: Some(status), message });
            }
            Ok(body)
        })
    }

    fn post(&self, path: &str, body: Value) -> GitHubFuture {
        let url = format!("{}{path}", self.base_url);
        let http = self.http.clone();
        let token = self.token.clone();
        Box::pin(async move {
            let resp = http
                .post(url)
                .bearer_auth(&token)
                .header("User-Agent", "aghub")
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .timeout(Duration::from_secs(10))
                .json(&body)
                .send()
                .await
                .map_err(|e| GitHubApiError { status: None, message: e.to_string() })?;
            let status = resp.status().as_u16();
            let body: Value = resp
                .json()
                .await
                .map_err(|e| GitHubApiError { status: Some(status), message: e.to_string() })?;
            if !(200..300).contains(&status) {
                let message = body
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("request failed")
                    .to_string();
                let first_error = body
                    .pointer("/errors/0/message")
                    .and_then(Value::as_str)
                    .map(|m| format!(": {m}"))
                    .unwrap_or_default();
                return Err(GitHubApiError {
                    status: Some(status),
                    message: format!("{message}{first_error}"),
                });
            }
            Ok(body)
        })
    }
}

/// Disabled transport: every call fails so handlers answer `503
/// github_unavailable`. FTS5-style — the feature simply stays off.
pub struct NoopGitHub;

impl GitHubTransport for NoopGitHub {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn enabled(&self) -> bool {
        false
    }

    fn get(&self, _path: &str, _query: &str) -> GitHubFuture {
        Box::pin(async {
            Err(GitHubApiError {
                status: None,
                message: "GitHub integration is not configured (set GITHUB_TOKEN)".into(),
            })
        })
    }

    fn post(&self, _path: &str, _body: Value) -> GitHubFuture {
        Box::pin(async {
            Err(GitHubApiError {
                status: None,
                message: "GitHub integration is not configured (set GITHUB_TOKEN)".into(),
            })
        })
    }
}

/// Deterministic in-memory double for tests and local UI runs: no network.
///
/// ```no_run
/// use aghub::github::MockGitHub;
/// use serde_json::json;
///
/// let mock = MockGitHub::new();
/// mock.set_repo("octocat", "Hello-World", json!({"full_name": "octocat/Hello-World"}));
/// mock.set_pulls("octocat", "Hello-World", json!([
///     {"number": 1, "title": "Fix bug", "state": "open", "draft": false,
///      "user": {"login": "octocat"}, "html_url": "https://github.com/octocat/Hello-World/pull/1"}
/// ]));
/// // state.github = Arc::new(mock); — the hub now serves that repo offline.
/// ```
pub struct MockGitHub {
    inner: Arc<Mutex<MockState>>,
}

#[derive(Default)]
struct MockState {
    /// Per-repo metadata or a canned error, keyed by lowercased owner/name.
    repos: HashMap<String, Result<Value, (u16, String)>>,
    /// Per-repo pull-request list pages, keyed the same way.
    pulls: HashMap<String, Value>,
    /// Every requested path, in order, for call assertions.
    calls: Vec<String>,
    /// Responses for POSTs, popped per call, keyed by lowercased path.
    /// An empty queue answers 201 with an empty object.
    posts: HashMap<String, Vec<GitHubResult>>,
}

impl MockGitHub {
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(MockState::default())) }
    }

    /// Register repository metadata the mock will serve.
    pub fn set_repo(&self, owner: &str, name: &str, repo: Value) {
        self.inner
            .lock()
            .unwrap()
            .repos
            .insert(mock_key(owner, name), Ok(repo));
    }

    /// Make the mock answer with a GitHub error for this repository.
    pub fn set_repo_error(&self, owner: &str, name: &str, status: u16, message: &str) {
        self.inner
            .lock()
            .unwrap()
            .repos
            .insert(mock_key(owner, name), Err((status, message.to_string())));
    }

    /// Register the pull-request list the mock will serve (a JSON array).
    pub fn set_pulls(&self, owner: &str, name: &str, pulls: Value) {
        self.inner.lock().unwrap().pulls.insert(mock_key(owner, name), pulls);
    }

    /// Paths requested so far (with query strings), in call order.
    pub fn calls(&self) -> Vec<String> {
        self.inner.lock().unwrap().calls.clone()
    }

    /// Script the response of the next POST to this path (201 on success).
    /// Multiple queued responses are served in order.
    pub fn push_post_response(&self, path: &str, result: GitHubResult) {
        let mut state = self.inner.lock().unwrap();
        state.posts.entry(path.to_lowercase()).or_default().push(result);
    }
}

fn mock_key(owner: &str, name: &str) -> String {
    format!("{}/{}", owner.to_lowercase(), name.to_lowercase())
}

/// Resolve the one path-shaped call the mock understands:
/// `/repos/{owner}/{name}` for metadata or `/repos/{owner}/{name}/pulls`.
fn mock_handle(inner: &mut MockState, path: &str) -> GitHubResult {
    let rest = path.strip_prefix("/repos/").unwrap_or("");
    let mut parts = rest.split('/');
    let (owner, name) = match (parts.next(), parts.next()) {
        (Some(o), Some(n)) if !o.is_empty() && !n.is_empty() => (o, n),
        _ => {
            return Err(GitHubApiError { status: Some(404), message: "Not Found".into() });
        }
    };
    let key = mock_key(owner, name);
    let tail = parts.next().is_some();
    if tail {
        if !rest.ends_with("/pulls") {
            return Err(GitHubApiError { status: Some(404), message: "Not Found".into() });
        }
        if !inner.repos.contains_key(&key) {
            return Err(GitHubApiError { status: Some(404), message: "Not Found".into() });
        }
        return Ok(inner
            .pulls
            .get(&key)
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())));
    }
    match inner.repos.get(&key) {
        Some(Ok(repo)) => Ok(repo.clone()),
        Some(Err((status, message))) => {
            Err(GitHubApiError { status: Some(*status), message: message.clone() })
        }
        None => Err(GitHubApiError { status: Some(404), message: "Not Found".into() }),
    }
}

impl GitHubTransport for MockGitHub {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn enabled(&self) -> bool {
        true
    }

    fn get(&self, path: &str, query: &str) -> GitHubFuture {
        let inner = Arc::clone(&self.inner);
        let path = path.to_string();
        let query = query.to_string();
        Box::pin(async move {
            let mut state = inner.lock().unwrap();
            state
                .calls
                .push(if query.is_empty() { path.clone() } else { format!("{path}?{query}") });
            mock_handle(&mut state, &path)
        })
    }

    fn post(&self, path: &str, _body: Value) -> GitHubFuture {
        let inner = Arc::clone(&self.inner);
        let path = path.to_string();
        Box::pin(async move {
            let mut state = inner.lock().unwrap();
            state.calls.push(path.clone());
            match state.posts.get_mut(&path.to_lowercase()) {
                Some(queue) if !queue.is_empty() => queue.remove(0),
                Some(_) | None => Ok(Value::Object(Default::default())),
            }
        })
    }
}

/// Build the transport from the environment: `GITHUB_TOKEN` activates the
/// real client, anything else stays on the disabled `NoopGitHub`.
pub fn from_env(http: reqwest::Client) -> std::sync::Arc<dyn GitHubTransport> {
    let token = std::env::var("GITHUB_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    match token {
        Some(token) => {
            let base_url = std::env::var("AGHUB_GITHUB_API_URL")
                .ok()
                .map(|b| b.trim().trim_end_matches('/').to_string())
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| "https://api.github.com".into());
            std::sync::Arc::new(GitHubClient { http, token, base_url })
        }
        None => std::sync::Arc::new(NoopGitHub),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn mock_serves_repo_metadata_and_pulls() {
        let mock = MockGitHub::new();
        mock.set_repo("octocat", "Hello-World", json!({"full_name": "octocat/Hello-World"}));
        mock.set_pulls(
            "octocat",
            "Hello-World",
            json!([{"number": 2, "title": "Second", "state": "closed"}]),
        );

        let repo = mock.get("/repos/octocat/Hello-World", "").await.unwrap();
        assert_eq!(repo["full_name"], "octocat/Hello-World");

        let pulls = mock.get("/repos/octocat/Hello-World/pulls", "state=all").await.unwrap();
        assert_eq!(pulls[0]["number"], 2);

        // Slug lookups are case-insensitive, like the real API.
        assert!(mock.get("/repos/OctoCat/hello-world", "").await.is_ok());
        assert_eq!(
            mock.calls(),
            vec![
                "/repos/octocat/Hello-World",
                "/repos/octocat/Hello-World/pulls?state=all",
                "/repos/OctoCat/hello-world",
            ]
        );
    }

    #[tokio::test]
    async fn mock_404s_unknown_resources() {
        let mock = MockGitHub::new();
        mock.set_repo("octocat", "Hello-World", json!({}));

        let err = mock.get("/repos/nobody/nothing", "").await.unwrap_err();
        assert_eq!(err.status, Some(404));
        // A registered repo without pulls answers an empty list.
        let pulls = mock.get("/repos/octocat/Hello-World/pulls", "").await.unwrap();
        assert!(pulls.as_array().unwrap().is_empty());
        // Paths that are not repos or pull lists are not understood.
        assert_eq!(mock.get("/other", "").await.unwrap_err().status, Some(404));
    }

    #[tokio::test]
    async fn mock_can_inject_upstream_errors() {
        let mock = MockGitHub::new();
        mock.set_repo_error("octocat", "Hello-World", 403, "rate limit");
        let err = mock.get("/repos/octocat/Hello-World", "").await.unwrap_err();
        assert_eq!(err.status, Some(403));
        assert_eq!(err.message, "rate limit");
    }
}
