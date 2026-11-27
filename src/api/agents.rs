use super::{is_unique_violation, require_agent};
use crate::error::ApiError;
use crate::models::{
    gen_token, now_iso, valid_handle, Agent, CreateAgent, UpdateAgent,
};
use crate::state::AppState;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::params;

fn map_agent_row(r: &rusqlite::Row) -> rusqlite::Result<Agent> {
    Ok(Agent {
        id: r.get(0)?,
        handle: r.get(1)?,
        name: r.get(2)?,
        webhook_url: r.get(3)?,
        created_at: r.get(4)?,
        workspace_id: r.get(5)?,
    })
}

const AGENT_COLS: &str = "id, handle, name, webhook_url, created_at, workspace_id";

/// Create an agent inside the CALLER's workspace. Authentication is required:
/// open self-registration was a cross-workspace leak. Breaking change.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateAgent>,
) -> Result<impl IntoResponse, ApiError> {
    let caller = require_agent(&state, &headers)?;
    if input.name.trim().is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }
    if !valid_handle(&input.handle) {
        return Err(ApiError::bad_request(
            "handle must be 2-32 chars of a-z, 0-9, '-' and '_'",
        ));
    }
    let token = gen_token();
    let now = now_iso();
    let agent = state
        .db
        .with(|c| {
            c.query_row(
                &format!(
                    "INSERT INTO agents (handle, name, token, webhook_url, created_at, workspace_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING {AGENT_COLS}"
                ),
                params![input.handle, input.name.trim(), token, input.webhook_url, now, caller.workspace_id],
                map_agent_row,
            )
        })
        .map_err(|e| {
            if is_unique_violation(&e) {
                ApiError::conflict("handle already taken")
            } else {
                e.into()
            }
        })?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "agent": agent, "token": token }))))
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let agents = state.db.with(|c| {
        let mut stmt = c.prepare(&format!(
            "SELECT {AGENT_COLS} FROM agents WHERE workspace_id = ?1 ORDER BY id"
        ))?;
        stmt.query_map([agent.workspace_id], map_agent_row)
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    })?;
    Ok(Json(serde_json::json!({ "agents": agents })))
}

pub async fn me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    Ok(Json(serde_json::json!({ "agent": agent })))
}

pub async fn update_me(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<UpdateAgent>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    if let Some(url) = &input.webhook_url {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(ApiError::bad_request("webhook_url must be an http(s) URL"));
        }
    }
    let updated = state.db.with(|c| {
        c.query_row(
            &format!("UPDATE agents SET webhook_url = ?1 WHERE id = ?2 RETURNING {AGENT_COLS}"),
            params![input.webhook_url, agent.id],
            map_agent_row,
        )
    })?;
    Ok(Json(serde_json::json!({ "agent": updated })))
}

/// Rotate the caller's bearer token. The authenticated agent gets a fresh
/// token; the old one is revoked immediately (single-token model).
pub async fn rotate_token(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let token = gen_token();
    let updated = state.db.with(|c| {
        c.query_row(
            &format!("UPDATE agents SET token = ?1 WHERE id = ?2 RETURNING {AGENT_COLS}"),
            params![token, agent.id],
            map_agent_row,
        )
    })?;
    Ok(Json(serde_json::json!({ "agent": updated, "token": token })))
}
