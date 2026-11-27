use super::{is_unique_violation, require_agent};
use crate::error::ApiError;
use crate::models::{
    gen_token, now_iso, valid_handle, Agent, CreateAgent, UpdateAgent,
};
use crate::state::AppState;
use axum::extract::{Path, State};
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
        role: r.get(4)?,
        created_at: r.get(5)?,
        workspace_id: r.get(6)?,
    })
}

const AGENT_COLS: &str = "id, handle, name, webhook_url, role, created_at, workspace_id";

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
    let (webhook_set, webhook_url) = match &input.webhook_url {
        Some(Some(url)) => {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(ApiError::bad_request("webhook_url must be an http(s) URL"));
            }
            (true, Some(url.as_str()))
        }
        Some(None) => (true, None),
        None => (false, None),
    };
    if let Some(role) = &input.role {
        if role.len() > 64 {
            return Err(ApiError::bad_request("role must be at most 64 characters"));
        }
    }
    let updated = state.db.with(|c| {
        c.query_row(
            &format!(
                "UPDATE agents SET
                   webhook_url = CASE WHEN ?1 THEN ?2 ELSE webhook_url END,
                   role = COALESCE(?3, role)
                 WHERE id = ?4
                 RETURNING {AGENT_COLS}"
            ),
            params![webhook_set, webhook_url, input.role.as_deref(), agent.id],
            map_agent_row,
        )
    })?;
    Ok(Json(serde_json::json!({ "agent": updated })))
}

/// PATCH /v1/agents/{id}: set an agent's role. The agent itself may set its
/// own role; otherwise only the workspace admin (the agent tied to the
/// workspace's human user account) may set it. Always scoped to the caller's
/// workspace.
pub async fn update_agent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<UpdateAgent>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = require_agent(&state, &headers)?;
    if input.webhook_url.is_some() {
        return Err(ApiError::forbidden_msg(
            "webhook_url can only be changed by the agent itself (PATCH /v1/agents/me)",
        ));
    }
    let Some(role) = input.role.as_deref() else {
        return Err(ApiError::bad_request("role is required"));
    };
    if role.len() > 64 {
        return Err(ApiError::bad_request("role must be at most 64 characters"));
    }
    let target: i64 = state.db.with(|c| {
        c.query_row(
            "SELECT id FROM agents WHERE id = ?1 AND workspace_id = ?2",
            params![id, caller.workspace_id],
            |r| r.get(0),
        )
    })?;
    if target != caller.id {
        let is_workspace_admin: bool = state.db.with(|c| {
            c.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM workspaces w
                   JOIN users u ON u.id = w.owner_user_id
                   WHERE w.id = ?1 AND u.agent_id = ?2
                 )",
                params![caller.workspace_id, caller.id],
                |r| r.get(0),
            )
        })?;
        if !is_workspace_admin {
            return Err(ApiError::forbidden_msg(
                "only the agent itself or a workspace admin can set its role",
            ));
        }
    }
    let updated = state.db.with(|c| {
        c.query_row(
            &format!("UPDATE agents SET role = ?1 WHERE id = ?2 RETURNING {AGENT_COLS}"),
            params![role, target],
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
