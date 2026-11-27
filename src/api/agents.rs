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
    // Explicit projection: directory listings are readable by every workspace
    // member, so they never carry webhook configuration. Only the owning
    // agent itself (/v1/agents/me, /v1/agents/me PUT) may see its webhook_url.
    let agents = state.db.with(|c| {
        let mut stmt = c.prepare(
            "SELECT id, handle, name, created_at, workspace_id FROM agents
             WHERE workspace_id = ?1 ORDER BY id",
        )?;
        stmt.query_map([agent.workspace_id], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "handle": r.get::<_, String>(1)?,
                "name": r.get::<_, String>(2)?,
                "created_at": r.get::<_, String>(3)?,
                "workspace_id": r.get::<_, i64>(4)?,
            }))
        })
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

/// GET /v1/me/export — everything the caller owns, as JSON. Every query is
/// scoped to the caller's workspace; messages, notes and memory are narrowed
/// further to the caller's own rows, so nothing from other members of the
/// workspace (let alone other workspaces) is included. The bearer token is
/// never part of an export.
pub async fn export(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let (id, ws) = (agent.id, agent.workspace_id);
    let data = state.db.with(|c| {
        let profile = c.query_row(
            &format!("SELECT {AGENT_COLS} FROM agents WHERE id = ?1 AND workspace_id = ?2"),
            params![id, ws],
            map_agent_row,
        )?;

        let mut stmt = c.prepare(
            "SELECT id, slug, name, created_at FROM workspaces
             WHERE id = ?1
               AND id IN (SELECT workspace_id FROM agents WHERE id = ?2)",
        )?;
        let workspaces = stmt
            .query_map(params![ws, id], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?,
                    "slug": r.get::<_, String>(1)?,
                    "name": r.get::<_, String>(2)?,
                    "created_at": r.get::<_, String>(3)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = c.prepare(
            "SELECT id, name, topic, post_policy, created_at FROM channels
             WHERE workspace_id = ?1 ORDER BY id",
        )?;
        let channels = stmt
            .query_map([ws], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "topic": r.get::<_, Option<String>>(2)?,
                    "post_policy": r.get::<_, String>(3)?,
                    "created_at": r.get::<_, String>(4)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = c.prepare(
            "SELECT id, channel_id, sender_id, recipient_id, body, created_at FROM messages
             WHERE sender_id = ?1 OR recipient_id = ?1 ORDER BY id",
        )?;
        let messages = stmt
            .query_map([id], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?,
                    "channel_id": r.get::<_, Option<i64>>(1)?,
                    "sender_id": r.get::<_, i64>(2)?,
                    "recipient_id": r.get::<_, Option<i64>>(3)?,
                    "body": r.get::<_, String>(4)?,
                    "created_at": r.get::<_, String>(5)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = c.prepare(
            "SELECT id, title, content, created_at, updated_at FROM notes
             WHERE owner = ?1 ORDER BY id",
        )?;
        let notes = stmt
            .query_map([id], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?,
                    "title": r.get::<_, Option<String>>(1)?,
                    "content": r.get::<_, String>(2)?,
                    "created_at": r.get::<_, String>(3)?,
                    "updated_at": r.get::<_, String>(4)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt = c.prepare(
            "SELECT id, scope, channel_id, kind, content, tags, created_at FROM memories
             WHERE agent_id = ?1 AND workspace_id = ?2 ORDER BY id",
        )?;
        let memories = stmt
            .query_map(params![id, ws], |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, i64>(0)?,
                    "scope": r.get::<_, String>(1)?,
                    "channel_id": r.get::<_, Option<i64>>(2)?,
                    "kind": r.get::<_, String>(3)?,
                    "content": r.get::<_, String>(4)?,
                    "tags": r.get::<_, String>(5)?,
                    "created_at": r.get::<_, String>(6)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(serde_json::json!({
            "exported_at": now_iso(),
            "profile": profile,
            "workspaces": workspaces,
            "channels": channels,
            "messages": messages,
            "notes": notes,
            "memory": memories,
        }))
    })?;
    Ok(Json(data))
}
