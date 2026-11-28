use super::is_unique_violation;
use crate::error::ApiError;
use crate::models::{gen_token, now_iso, valid_handle, Agent};
use crate::state::AppState;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::params;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const SESSION_COOKIE: &str = "aghub_session";
const SESSION_TTL_SECS: u64 = 30 * 86400;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signup {
    pub email: String,
    pub password: String,
    pub handle: String,
    pub name: String,
    pub webhook_url: Option<String>,
    /// Workspace invite token (audit gap P1-1): when valid, unused and
    /// unexpired, the signup joins that workspace as a member instead of
    /// creating a new one.
    #[serde(default)]
    pub invite_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    pub email: String,
    pub password: String,
}

pub struct Session {
    pub email: String,
    pub agent: Agent,
}

fn hash_password(password: &str) -> String {
    let salt = gen_token();
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(b"$");
    hasher.update(password.as_bytes());
    format!("{salt}${:x}", hasher.finalize())
}

fn verify_password(password: &str, stored: &str) -> bool {
    let Some((salt, expected)) = stored.split_once('$') else {
        return false;
    };
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(b"$");
    hasher.update(password.as_bytes());
    expected == format!("{:x}", hasher.finalize())
}

fn valid_email(email: &str) -> bool {
    email.len() <= 254
        && email
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@.-_+".contains(c))
        && email.contains('@')
        && !email.starts_with('@')
        && !email.ends_with('@')
}

fn valid_password(password: &str) -> bool {
    (8..=128).contains(&password.len())
}

/// URL-safe slug: lowercase, every run of non-alphanumerics becomes one
/// hyphen, trimmed. Empty input falls back to 'workspace'.
fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut last_hyphen = true; // trims leading hyphens
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_hyphen = false;
        } else if !last_hyphen {
            slug.push('-');
            last_hyphen = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "workspace".into()
    } else {
        slug
    }
}

/// First free slug: `base`, then `base-2`, `base-3`, ...
fn unique_slug(c: &rusqlite::Connection, base: &str) -> String {
    let mut n = 1;
    loop {
        let candidate = if n == 1 { base.to_string() } else { format!("{base}-{n}") };
        let taken: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM workspaces WHERE slug = ?1",
                [candidate.as_str()],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if taken == 0 {
            return candidate;
        }
        n += 1;
    }
}

fn agent_from_row(offset: usize) -> impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<Agent> {
    move |r: &rusqlite::Row<'_>| {
        Ok(Agent {
            id: r.get(offset)?,
            handle: r.get(offset + 1)?,
            name: r.get(offset + 2)?,
            webhook_url: r.get(offset + 3)?,
            role: r.get(offset + 4)?,
            created_at: r.get(offset + 5)?,
            workspace_id: r.get(offset + 6)?,
        })
    }
}

/// Resolve the human session from the cookie header (server-side only).
pub fn current_session(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<Session>, ApiError> {
    let cookies = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = cookies
        .split(';')
        .filter_map(|p| p.trim().strip_prefix(&format!("{SESSION_COOKIE}=")))
        .next()
        .unwrap_or("");
    if token.is_empty() {
        return Ok(None);
    }
    state.db.try_with(|c| {
        let row = c.query_row(
            "SELECT s.expires_at, u.email, a.id, a.handle, a.name, a.webhook_url, a.role, a.created_at, a.workspace_id
             FROM sessions s
             JOIN users u ON u.id = s.user_id
             JOIN agents a ON a.id = s.agent_id
             WHERE s.id = ?1",
            [token],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    agent_from_row(2)(r)?,
                ))
            },
        );
        match row {
            Ok((expires_at, email, agent)) if expires_at > now_iso() => {
                Ok(Some(Session { email, agent }))
            }
            Ok(_) => Ok(None), // expired; treat as signed out
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    })
}

pub async fn signup(
    State(state): State<AppState>,
    Json(req): Json<Signup>,
) -> Result<Response, ApiError> {
    let email = req.email.trim().to_lowercase();
    if !valid_email(&email) {
        return Err(ApiError::bad_request("enter a valid email address"));
    }
    if !valid_password(&req.password) {
        return Err(ApiError::bad_request("password must be 8 to 128 characters"));
    }
    if !valid_handle(&req.handle) {
        return Err(ApiError::bad_request(
            "handle must be 2-32 chars: a-z, 0-9, '-' or '_'",
        ));
    }
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }

    let token = gen_token();
    let session_id = gen_token();
    let now = now_iso();
    let expires = crate::models::now_plus(std::time::Duration::from_secs(SESSION_TTL_SECS));

    let (agent_id, workspace_id, workspace_name, workspace_slug): (i64, i64, String, String) = state.db.try_with(|c| {
        let tx = c.transaction()?;
        let agent_id: i64 = tx
            .query_row(
                "INSERT INTO agents (handle, name, token, webhook_url, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
                params![req.handle, name, token, req.webhook_url, now],
                |r| r.get(0),
            )
            .map_err(|e| {
                if is_unique_violation(&e) {
                    ApiError::conflict("handle is already taken")
                } else {
                    e.into()
                }
            })?;
        let user_id: i64 = tx
            .query_row(
                "INSERT INTO users (email, password_hash, agent_id, created_at) VALUES (?1, ?2, ?3, ?4) RETURNING id",
                params![email, hash_password(&req.password), agent_id, now],
                |r| r.get(0),
            )
            .map_err(|e| {
                if is_unique_violation(&e) {
                    ApiError::conflict("an account with this email already exists")
                } else {
                    e.into()
                }
            })?;
        tx.execute(
            "INSERT INTO sessions (id, user_id, agent_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, user_id, agent_id, now, expires],
        )?;
        // Workspace invite (audit gap P1-1): a valid, unused, unexpired token
        // moves the freshly created agent into the invited workspace as a
        // member of its #general and is marked used — no new workspace.
        if let Some(invite_token) = req.invite_token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            let (workspace_id, ws_name, workspace_slug) =
                crate::api::invites::accept_signup_invite(&tx, agent_id, invite_token, &now)?;
            tx.commit()?;
            return Ok((agent_id, workspace_id, ws_name, workspace_slug));
        }
        // 1 email = 1 workspace: the new user gets their OWN workspace with a
        // fresh #general where they are admin. They never join the owner's
        // forum. Signup stays open (no approval step).
        let ws_name = format!("{name} workspace");
        let workspace_slug = unique_slug(&tx, &slugify(&ws_name));
        let workspace_id: i64 = tx.query_row(
            "INSERT INTO workspaces (owner_user_id, slug, name, created_at) VALUES (?1, ?2, ?3, ?4) RETURNING id",
            params![user_id, workspace_slug, ws_name, now],
            |r| r.get(0),
        )?;
        // The agent row had to exist before users (FK), so it took the legacy
        // default; move it into the user's fresh workspace.
        tx.execute(
            "UPDATE agents SET workspace_id = ?1 WHERE id = ?2",
            params![workspace_id, agent_id],
        )?;
        let channel_id: i64 = tx.query_row(
            "INSERT INTO channels (workspace_id, name, topic, post_policy, created_at)
             VALUES (?1, 'general', 'Workspace kamu', 'members', ?2) RETURNING id",
            params![workspace_id, now],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO channel_members (channel_id, agent_id, joined_at, role) VALUES (?1, ?2, ?3, 'admin')",
            params![channel_id, agent_id, now],
        )?;
        tx.commit()?;
        Ok((agent_id, workspace_id, ws_name, workspace_slug))
    })?;

    let body = serde_json::json!({
        "email": email,
        "agent": {
            "id": agent_id, "handle": req.handle, "name": name,
            "webhook_url": req.webhook_url, "created_at": now,
            // Same bearer token /ui/api/me returns to the cookie holder: the
            // fresh human session can drive the linked agent immediately.
            "token": token,
        },
        "workspace": {
            "id": workspace_id,
            "name": workspace_name,
            "slug": workspace_slug,
        },
    });
    Ok(session_response(&session_id, body))
}

pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<Login>,
) -> Result<Response, ApiError> {
    let email = req.email.trim().to_lowercase();
    // Per-email brute-force guard on top of the per-IP throttle middleware:
    // an attacker spreading guesses for one account across many source IPs
    // must still hit the same limit. Same fixed 15-minute window as the IP
    // guard, so a blocked account unlocks when the window rolls over and a
    // successful login clears it immediately — never a permanent lockout.
    let email_key = format!("email:{email}");
    if let Some(retry_after) = state.login_throttle.blocked_for(&email_key) {
        return Ok(too_many_logins(retry_after));
    }
    let row: Option<(String, String, Agent)> = state.db.with(|c| {
        c.query_row(
            "SELECT u.password_hash, u.email, a.id, a.handle, a.name, a.webhook_url, a.role, a.created_at, a.workspace_id
             FROM users u JOIN agents a ON a.id = u.agent_id WHERE u.email = ?1",
            [&email],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    agent_from_row(2)(r)?,
                ))
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    })?;
    let Some((hash, stored_email, agent)) = row else {
        state.login_throttle.record_failure(&email_key);
        return Err(ApiError::unauthorized_msg("invalid email or password"));
    };
    if !verify_password(&req.password, &hash) {
        state.login_throttle.record_failure(&email_key);
        return Err(ApiError::unauthorized_msg("invalid email or password"));
    }
    state.login_throttle.reset(&email_key);

    let session_id = gen_token();
    state.db.with(|c| {
        c.execute(
            "INSERT INTO sessions (id, user_id, agent_id, created_at, expires_at)
             SELECT ?1, u.id, u.agent_id, ?2, ?3 FROM users u WHERE u.email = ?4",
            params![
                session_id,
                now_iso(),
                crate::models::now_plus(std::time::Duration::from_secs(SESSION_TTL_SECS)),
                &stored_email
            ],
        )
    })?;

    let body = serde_json::json!({
        "email": stored_email,
        "agent": agent,
    });
    Ok(session_response(&session_id, body))
}

pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let cookies = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = cookies
        .split(';')
        .filter_map(|p| p.trim().strip_prefix(&format!("{SESSION_COOKIE}=")))
        .next()
        .unwrap_or("");
    if !token.is_empty() {
        state
            .db
            .with(|c| c.execute("DELETE FROM sessions WHERE id = ?1", [token]))?;
    }
    Ok((
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            format!("{SESSION_COOKIE}=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0"),
        )],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response())
}

/// The logged-in human's agent identity — metadata only. The agent's bearer
/// token is never returned to a browser: the workspace UI authenticates its
/// /v1 calls with this session cookie, which the server bridges to the
/// agent's bearer token (see `main.rs`). Agents themselves keep using their
/// bearer token directly, unchanged.
pub async fn me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let body = match current_session(&state, &headers)? {
        None => return Err(ApiError::unauthorized_msg("not signed in")),
        Some(s) => {
            let (workspace_name, workspace_slug): (String, String) = state.db.with(|c| {
                c.query_row(
                    "SELECT w.name, w.slug FROM workspaces w
                     JOIN agents a ON a.workspace_id = w.id
                     WHERE a.id = ?1",
                    params![s.agent.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            })?;
            // The owner uses the Hub UI to create agents and rotate their
            // keys, so the UI needs to know whether the signed-in human's
            // agent is the workspace admin. Still metadata only — no token.
            let is_workspace_admin = super::agents::is_workspace_admin(&state, &s.agent)?;
            serde_json::json!({
                "email": s.email,
                "is_workspace_admin": is_workspace_admin,
                "agent": {
                    "id": s.agent.id,
                    "handle": s.agent.handle,
                    "role": s.agent.role,
                    "name": s.agent.name,
                    "webhook_url": s.agent.webhook_url,
                    "created_at": s.agent.created_at,
                },
                "workspace": {
                    "id": s.agent.workspace_id,
                    "name": workspace_name,
                    "slug": workspace_slug,
                },
            })
        }
    };
    // The answer depends on this visitor's cookie: no cache — not the phone's,
    // not a CDN's — may keep a 401 (or an identity) after it stops being true.
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response())
}

/// 429 for the per-email login guard, same shape and Retry-After header as
/// the per-IP throttle middleware.
fn too_many_logins(retry_after: std::time::Duration) -> Response {
    let secs = retry_after.as_secs().max(1);
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, secs.to_string())],
        Json(serde_json::json!({
            "error": {
                "code": "rate_limited",
                "message": format!("too many failed logins, retry after {secs}s"),
            }
        })),
    )
        .into_response()
}

fn session_response(session_id: &str, body: serde_json::Value) -> Response {
    (
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            format!(
                "{SESSION_COOKIE}={session_id}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={SESSION_TTL_SECS}"
            ),
        )],
        Json(body),
    )
        .into_response()
}

impl ApiError {
    pub fn unauthorized_msg(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
            message: msg.into(),
        }
    }
}
