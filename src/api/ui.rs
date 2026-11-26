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

fn agent_from_row(offset: usize) -> impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<Agent> {
    move |r: &rusqlite::Row<'_>| {
        Ok(Agent {
            id: r.get(offset)?,
            handle: r.get(offset + 1)?,
            name: r.get(offset + 2)?,
            webhook_url: r.get(offset + 3)?,
            created_at: r.get(offset + 4)?,
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
            "SELECT s.expires_at, u.email, a.id, a.handle, a.name, a.webhook_url, a.created_at
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

    let agent_id: i64 = state.db.try_with(|c| {
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
        tx.commit()?;
        Ok(agent_id)
    })?;

    let body = serde_json::json!({
        "email": email,
        "agent": {
            "id": agent_id, "handle": req.handle, "name": name,
            "webhook_url": req.webhook_url, "created_at": now,
        },
    });
    Ok(session_response(&session_id, body))
}

pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<Login>,
) -> Result<Response, ApiError> {
    let email = req.email.trim().to_lowercase();
    let row: Option<(String, String, Agent)> = state.db.with(|c| {
        c.query_row(
            "SELECT u.password_hash, u.email, a.id, a.handle, a.name, a.webhook_url, a.created_at
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
        return Err(ApiError::unauthorized_msg("invalid email or password"));
    };
    if !verify_password(&req.password, &hash) {
        return Err(ApiError::unauthorized_msg("invalid email or password"));
    }

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
            format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"),
        )],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response())
}

/// The logged-in human's agent identity. Returns the agent's bearer token so
/// the workspace UI can call /v1 on the agent's behalf; agents themselves
/// keep authenticating with the bearer token directly, unchanged.
pub async fn me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    match current_session(&state, &headers)? {
        None => Err(ApiError::unauthorized_msg("not signed in")),
        Some(s) => {
            let token: String = state.db.with(|c| {
                c.query_row(
                    "SELECT token FROM agents WHERE id = ?1",
                    params![s.agent.id],
                    |r| r.get(0),
                )
            })?;
            Ok(Json(serde_json::json!({
                "email": s.email,
                "agent": {
                    "id": s.agent.id,
                    "handle": s.agent.handle,
                    "name": s.agent.name,
                    "webhook_url": s.agent.webhook_url,
                    "created_at": s.agent.created_at,
                    "token": token,
                },
            })))
        }
    }
}

fn session_response(session_id: &str, body: serde_json::Value) -> Response {
    (
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            format!(
                "{SESSION_COOKIE}={session_id}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_TTL_SECS}"
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
