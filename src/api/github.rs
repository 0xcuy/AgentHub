//! Channel GitHub integration: link repositories to a channel and browse
//! their open/closed pull requests through the swappable `GitHubTransport`
//! on `AppState`.
//!
//! Access follows the memory handlers: members see and manage the links of
//! their channels, with the 404/403 distinction (unknown channels look like
//! they do not exist). Linking is a channel-admin operation, mirroring the
//! channel-settings permissions. Pull requests are cached in SQLite and
//! refreshed from GitHub when the cache is empty or older than five minutes;
//! a failed refresh serves the cache.

use super::require_agent;
use crate::error::ApiError;
use crate::github::GitHubApiError;
use crate::models::{
    now_iso, valid_pull_state, CreatePullRequest, GitHubPull, GitHubRepo, LinkGitHubRepo,
    PullsQuery,
};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::OptionalExtension;

const MAX_SLUG_CHARS: usize = 100;
const DEFAULT_PULL_LIMIT: i64 = 50;
const MAX_PULL_LIMIT: i64 = 200;

/// GitHub owner rules (simplified): alphanumerics and hyphens, no leading or
/// trailing hyphen. Keeps the transport path single-segment.
fn valid_owner(s: &str) -> bool {
    (1..=MAX_SLUG_CHARS).contains(&s.len())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// GitHub repository-name rules (simplified): alphanumerics, hyphens, dots
/// and underscores. Real repos can be named `.github`, so a leading dot is
/// fine; only path tricks like `.` and `..` are rejected.
fn valid_repo(s: &str) -> bool {
    (1..=MAX_SLUG_CHARS).contains(&s.len())
        && s != "."
        && s != ".."
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
}

fn map_repo_row(r: &rusqlite::Row) -> rusqlite::Result<GitHubRepo> {
    Ok(GitHubRepo {
        id: r.get(0)?,
        channel_id: r.get(1)?,
        owner: r.get(2)?,
        name: r.get(3)?,
        default_branch: r.get(4)?,
        linked_by: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
    })
}

fn map_pull_row(r: &rusqlite::Row) -> rusqlite::Result<GitHubPull> {
    let draft: i64 = r.get(3)?;
    Ok(GitHubPull {
        number: r.get(0)?,
        title: r.get(1)?,
        state: r.get(2)?,
        draft: draft != 0,
        author: r.get(4)?,
        url: r.get(5)?,
        fetched_at: r.get(6)?,
    })
}

/// Membership check with the memory handlers' 404/403 distinction. Runs
/// inside the caller's transaction so checks and writes stay consistent.
fn require_channel_member(
    c: &rusqlite::Connection,
    channel_id: i64,
    agent_id: i64,
) -> Result<(), ApiError> {
    let member: i64 = c.query_row(
        "SELECT COUNT(*) FROM channel_members WHERE channel_id = ?1 AND agent_id = ?2",
        rusqlite::params![channel_id, agent_id],
        |r| r.get(0),
    )?;
    if member == 0 {
        // Distinguish 404 (no such channel) from 403 (not a member).
        let exists: i64 = c
            .query_row("SELECT COUNT(*) FROM channels WHERE id = ?1", [channel_id], |r| r.get(0))?;
        if exists == 0 {
            return Err(ApiError::not_found("channel not found"));
        }
        return Err(ApiError::forbidden_msg("you are not a member of this channel"));
    }
    Ok(())
}

/// The channel code's exact admin gate (`role = 'admin'` in
/// channel_members), reused for repository management. Assumes membership
/// was already established; runs inside the caller's transaction.
fn require_channel_admin(
    c: &rusqlite::Connection,
    channel_id: i64,
    agent_id: i64,
) -> Result<(), ApiError> {
    let role: Option<String> = c
        .query_row(
            "SELECT role FROM channel_members WHERE channel_id = ?1 AND agent_id = ?2",
            rusqlite::params![channel_id, agent_id],
            |r| r.get(0),
        )
        .optional()?;
    if role.as_deref() != Some("admin") {
        return Err(ApiError::forbidden_msg("only channel admins can manage GitHub repositories"));
    }
    Ok(())
}

/// Map a transport error onto the API error shape. 404 is honest (GitHub
/// says so); a 422 is the caller's head/base being rejected upstream; other
/// statuses and transport failures are a bad gateway with context.
fn github_error(context: &str, what: &str, e: GitHubApiError) -> ApiError {
    match e.status {
        Some(404) => ApiError::with_code(
            StatusCode::NOT_FOUND,
            "github_not_found",
            format!("not found on GitHub: {what} (the token may lack access)"),
        ),
        Some(status) if (400..500).contains(&status) => ApiError::with_code(
            StatusCode::BAD_REQUEST,
            "github_rejected",
            format!("{context} for {what}: GitHub returned {status}: {}", e.message),
        ),
        Some(status) => ApiError::with_code(
            StatusCode::BAD_GATEWAY,
            "github_error",
            format!("{context} for {what}: GitHub returned {status}: {}", e.message),
        ),
        None => ApiError::with_code(
            StatusCode::BAD_GATEWAY,
            "github_unreachable",
            format!("{context} for {what}: {}", e.message),
        ),
    }
}

fn unconfigured() -> ApiError {
    ApiError::with_code(
        StatusCode::SERVICE_UNAVAILABLE,
        "github_unavailable",
        "GitHub integration is not configured (set GITHUB_TOKEN)",
    )
}

/// Link a repository to a channel. The repo is fetched from GitHub first so
/// a link only exists for repositories the transport can actually see
/// (existence plus token scope). Re-linking an existing slug refreshes its
/// default branch and returns the existing link.
pub async fn link_repo(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel_id): Path<i64>,
    Json(input): Json<LinkGitHubRepo>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let owner = input.owner.trim().to_string();
    let name = input.name.trim().to_string();
    if !valid_owner(&owner) {
        return Err(ApiError::bad_request(
            "owner must be 1-100 ASCII letters, digits or hyphens",
        ));
    }
    if !valid_repo(&name) {
        return Err(ApiError::bad_request(
            "name must be 1-100 ASCII letters, digits, hyphens, dots or underscores",
        ));
    }
    if !state.github.enabled() {
        return Err(unconfigured());
    }

    let what = format!("{owner}/{name}");
    let body = state
        .github
        .get(&format!("/repos/{owner}/{name}"), "")
        .await
        .map_err(|e| github_error("could not link repository", &what, e))?;
    let default_branch = body
        .get("default_branch")
        .and_then(|v| v.as_str())
        .unwrap_or("main")
        .to_string();

    let now = now_iso();
    let repo = state.db.try_with(|c| {
        let tx = c.transaction()?;
        require_channel_member(&tx, channel_id, agent.id)?;
        require_channel_admin(&tx, channel_id, agent.id)?;
        let linked: i64 = tx.query_row(
            "SELECT COUNT(*) FROM github_repos WHERE channel_id = ?1",
            [channel_id],
            |r| r.get(0),
        )?;
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM github_repos WHERE channel_id = ?1 AND owner = ?2 AND name = ?3",
                rusqlite::params![channel_id, owner, name],
                |r| r.get(0),
            )
            .optional()?;
        let max_repos = state.github_max_repos;
        if existing.is_none() && linked >= max_repos {
            return Err(ApiError::with_code(
                StatusCode::FORBIDDEN,
                "github_repo_cap_reached",
                format!(
                    "channel repo cap reached ({max_repos} repos per channel); \
                     unlink a repository before linking more"
                ),
            ));
        }
        let row = tx.query_row(
            "INSERT INTO github_repos
                 (channel_id, owner, name, default_branch, linked_by, created_at, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
               ON CONFLICT(channel_id, owner, name) DO UPDATE SET
                 default_branch = excluded.default_branch,
                 updated_at = excluded.updated_at
               RETURNING id, created_at",
            rusqlite::params![channel_id, owner, name, default_branch, agent.id, now, now],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )?;
        tx.commit()?;
        Ok(GitHubRepo {
            id: row.0,
            channel_id,
            owner,
            name,
            default_branch,
            linked_by: agent.id,
            created_at: row.1,
            updated_at: now,
        })
    })?;

    Ok((StatusCode::OK, Json(serde_json::json!({ "repo": repo }))))
}

/// List a channel's linked repositories. Members only.
pub async fn list_repos(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel_id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let repos: Vec<GitHubRepo> = state.db.try_with(|c| {
        require_channel_member(c, channel_id, agent.id)?;
        let mut stmt = c.prepare(
            "SELECT id, channel_id, owner, name, default_branch, linked_by, created_at, updated_at
             FROM github_repos WHERE channel_id = ?1 ORDER BY id",
        )?;
        let mut rows = Vec::new();
        for r in stmt.query_map([channel_id], map_repo_row)? {
            rows.push(r?);
        }
        Ok(rows)
    })?;
    Ok(Json(serde_json::json!({ "repos": repos })))
}

/// Remove a repository link and its cached pull requests. Members only; an
/// outsider gets the same denial as for any other channel operation and
/// cannot probe a channel's links.
pub async fn unlink_repo(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((channel_id, repo_id)): Path<(i64, i64)>,
) -> Result<StatusCode, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let deleted = state.db.try_with(|c| {
        let tx = c.transaction()?;
        require_channel_member(&tx, channel_id, agent.id)?;
        require_channel_admin(&tx, channel_id, agent.id)?;
        let linked: Option<i64> = tx
            .query_row(
                "SELECT channel_id FROM github_repos WHERE id = ?1 AND channel_id = ?2",
                rusqlite::params![repo_id, channel_id],
                |r| r.get(0),
            )
            .optional()?;
        if linked.is_none() {
            return Ok(false);
        }
        // github_pulls rows go with the link via ON DELETE CASCADE.
        tx.execute("DELETE FROM github_repos WHERE id = ?1", [repo_id])?;
        tx.commit()?;
        Ok(true)
    })?;
    if !deleted {
        return Err(ApiError::not_found("repo link not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Parse one GitHub pulls-array response into cached rows. Unexpected or
/// missing fields are skipped rather than failing the whole refresh.
fn parse_pulls(body: &serde_json::Value, fetched_at: &str) -> Vec<GitHubPull> {
    body.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    let number = p.get("number")?.as_i64()?;
                    let state = if p.get("state").and_then(|s| s.as_str()) == Some("closed") {
                        "closed"
                    } else {
                        "open"
                    };
                    Some(GitHubPull {
                        number,
                        title: p.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                        state: state.to_string(),
                        draft: p.get("draft").and_then(|d| d.as_bool()).unwrap_or(false),
                        author: p.pointer("/user/login").and_then(|u| u.as_str()).map(String::from),
                        url: p
                            .get("html_url")
                            .and_then(|u| u.as_str())
                            .unwrap_or("")
                            .to_string(),
                        fetched_at: fetched_at.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn load_pulls(
    c: &mut rusqlite::Connection,
    repo_id: i64,
    state_filter: &str,
    limit: i64,
) -> Result<Vec<GitHubPull>, ApiError> {
    let sql = match state_filter {
        "open" => {
            "SELECT number, title, state, draft, author, url, fetched_at FROM github_pulls
             WHERE repo_id = ?1 AND state = 'open' ORDER BY number DESC LIMIT ?2"
        }
        "closed" => {
            "SELECT number, title, state, draft, author, url, fetched_at FROM github_pulls
             WHERE repo_id = ?1 AND state = 'closed' ORDER BY number DESC LIMIT ?2"
        }
        _ => {
            "SELECT number, title, state, draft, author, url, fetched_at FROM github_pulls
             WHERE repo_id = ?1 ORDER BY number DESC LIMIT ?2"
        }
    };
    let mut stmt = c.prepare(sql)?;
    let mut rows = Vec::new();
    for r in stmt.query_map(rusqlite::params![repo_id, limit], map_pull_row)? {
        rows.push(r?);
    }
    Ok(rows)
}

/// List a linked repository's pull requests (open, closed or all), served
/// from the SQLite cache and refreshed from GitHub when the cache is empty
/// or older than five minutes. A failed refresh serves the cache and logs.
pub async fn list_pulls(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((channel_id, repo_id)): Path<(i64, i64)>,
    Query(q): Query<PullsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let state_filter = q.state.clone().unwrap_or_else(|| "all".into());
    if !valid_pull_state(&state_filter) {
        return Err(ApiError::bad_request("state must be 'open', 'closed' or 'all'"));
    }
    let limit = q.limit.unwrap_or(DEFAULT_PULL_LIMIT).clamp(1, MAX_PULL_LIMIT);
    if !state.github.enabled() {
        return Err(unconfigured());
    }

    let cutoff = (chrono::Utc::now()
        - chrono::Duration::seconds(state.github_cache_ttl_secs))
    .to_rfc3339();
    let (repo, mut pulls) = state.db.try_with(|c| {
        require_channel_member(c, channel_id, agent.id)?;
        let repo = c
            .query_row(
                "SELECT id, channel_id, owner, name, default_branch, linked_by, created_at, updated_at
                 FROM github_repos WHERE id = ?1 AND channel_id = ?2",
                rusqlite::params![repo_id, channel_id],
                map_repo_row,
            )
            .optional()?
            .ok_or_else(|| ApiError::not_found("repo link not found"))?;
        let pulls = load_pulls(c, repo.id, &state_filter, limit)?;
        Ok((repo, pulls))
    })?;

    let stale = pulls.is_empty() || pulls.iter().any(|p| p.fetched_at < cutoff);
    if stale {
        let what = format!("{}/{}", repo.owner, repo.name);
        let path = format!("/repos/{}/{}/pulls", repo.owner, repo.name);
        match state.github.get(&path, "state=all&per_page=100").await {
            Ok(body) => {
                let now = now_iso();
                let fresh = parse_pulls(&body, &now);
                let stored: Result<Vec<GitHubPull>, ApiError> = state.db.try_with(|c| {
                    let tx = c.transaction()?;
                    tx.execute("DELETE FROM github_pulls WHERE repo_id = ?1", [repo.id])?;
                    for p in &fresh {
                        tx.execute(
                            "INSERT INTO github_pulls
                                 (repo_id, number, title, state, draft, author, url, fetched_at)
                               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                               ON CONFLICT(repo_id, number) DO UPDATE SET
                                 title = excluded.title,
                                 state = excluded.state,
                                 draft = excluded.draft,
                                 author = excluded.author,
                                 url = excluded.url,
                                 fetched_at = excluded.fetched_at",
                            rusqlite::params![
                                repo.id, p.number, p.title, p.state, p.draft as i64,
                                p.author, p.url, p.fetched_at
                            ],
                        )?;
                    }
                    tx.commit()?;
                    // Serve the requested state filter over what we stored.
                    load_pulls(c, repo.id, &state_filter, limit)
                });
                match stored {
                    Ok(fresh_filtered) => pulls = fresh_filtered,
                    Err(e) => {
                        tracing::warn!(error = ?e, repo = %what, "github pull cache write failed");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, repo = %what, "github pull refresh failed; serving cached");
            }
        }
    }

    Ok(Json(serde_json::json!({ "pulls": pulls })))
}

/// Create a pull request in a linked repository. Channel admins only, like
/// linking and unlinking: the transport POSTs to GitHub, and the returned PR
/// (or a 422 with GitHub's message) is cached and answered with its number
/// and URL.
pub async fn create_pull(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((channel_id, repo_id)): Path<(i64, i64)>,
    Json(input): Json<CreatePullRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let head = input.head.trim().to_string();
    let base = input.base.trim().to_string();
    let title = input.title.trim().to_string();
    if head.is_empty() || head.len() > MAX_SLUG_CHARS {
        return Err(ApiError::bad_request("head must be 1-100 characters"));
    }
    if base.is_empty() || base.len() > MAX_SLUG_CHARS {
        return Err(ApiError::bad_request("base must be 1-100 characters"));
    }
    if title.is_empty() || title.chars().count() > 256 {
        return Err(ApiError::bad_request("title must be 1-256 characters"));
    }
    if !state.github.enabled() {
        return Err(unconfigured());
    }

    let repo = state.db.try_with(|c| {
        require_channel_member(c, channel_id, agent.id)?;
        let repo = c
            .query_row(
                "SELECT id, channel_id, owner, name, default_branch, linked_by, created_at, updated_at
                 FROM github_repos WHERE id = ?1 AND channel_id = ?2",
                rusqlite::params![repo_id, channel_id],
                map_repo_row,
            )
            .optional()?
            .ok_or_else(|| ApiError::not_found("repo link not found"))?;
        // Same admin gate as link_repo/unlink_repo.
        require_channel_admin(c, channel_id, agent.id)?;
        Ok(repo)
    })?;

    let what = format!("{}/{}", repo.owner, repo.name);
    let gh_path = format!("/repos/{}/{}/pulls", repo.owner, repo.name);
    let gh_body = serde_json::json!({
        "head": head,
        "base": base,
        "title": title,
        "body": input.body.clone().unwrap_or_default(),
    });
    let created = state
        .github
        .post(&gh_path, gh_body)
        .await
        .map_err(|e| github_error("could not create pull request", &what, e))?;
    let now = now_iso();
    let pull = GitHubPull {
        number: created.get("number").and_then(|n| n.as_i64()).unwrap_or_default(),
        title: created
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or(&title)
            .to_string(),
        state: if created.get("state").and_then(|s| s.as_str()) == Some("closed") {
            "closed".to_string()
        } else {
            "open".to_string()
        },
        draft: created.get("draft").and_then(|d| d.as_bool()).unwrap_or(false),
        author: Some(agent.handle.clone()),
        url: created
            .get("html_url")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string(),
        fetched_at: now.clone(),
    };

    // Cache the returned PR so the next list shows it immediately.
    let cached = state.db.with(|c| {
        c.execute(
            "INSERT INTO github_pulls
                 (repo_id, number, title, state, draft, author, url, fetched_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
               ON CONFLICT(repo_id, number) DO UPDATE SET
                 title = excluded.title,
                 state = excluded.state,
                 draft = excluded.draft,
                 author = excluded.author,
                 url = excluded.url,
                 fetched_at = excluded.fetched_at",
            rusqlite::params![
                repo.id, pull.number, pull.title, pull.state, pull.draft as i64,
                pull.author, pull.url, pull.fetched_at
            ],
        )
    });
    if let Err(e) = cached {
        tracing::warn!(error = ?e, repo = %what, "github created-pull cache write failed");
    }

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "pull": { "number": pull.number, "url": pull.url, "state": pull.state } })),
    ))
}
