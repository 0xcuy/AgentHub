use super::{extract_id, require_agent};
use crate::error::ApiError;
use crate::models::{
    now_iso, reply_snippet, Agent, EditMessage, Message, MessageReply, SetTaskStatus, TASK_STATUSES,
};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use rusqlite::{params, OptionalExtension};

/// Column list every message SELECT shares, always followed by [`MESSAGE_JOINS`].
pub(super) const MESSAGE_COLUMNS: &str = "msg.id, msg.channel_id, msg.sender_id, a.handle, \
     msg.recipient_id, msg.body, msg.created_at, msg.urgent, msg.reply_to_id, ra.handle, \
     rr.body, msg.edited_at, msg.task_status";

/// Joins for the reply preview; `rr` is the replied-to message, `ra` its sender.
pub(super) const MESSAGE_JOINS: &str = "JOIN agents a ON a.id = msg.sender_id \
     LEFT JOIN messages rr ON rr.id = msg.reply_to_id \
     LEFT JOIN agents ra ON ra.id = rr.sender_id";

/// Map [`MESSAGE_COLUMNS`] starting at `base` (inbox rows put receipt columns first).
pub(super) fn map_message_at(r: &rusqlite::Row, base: usize) -> rusqlite::Result<Message> {
    let reply_to_id: Option<i64> = r.get(base + 8)?;
    let reply_handle: Option<String> = r.get(base + 9)?;
    let reply_body: Option<String> = r.get(base + 10)?;
    Ok(Message {
        id: r.get(base)?,
        channel_id: r.get(base + 1)?,
        sender_id: r.get(base + 2)?,
        sender_handle: r.get(base + 3)?,
        recipient_id: r.get(base + 4)?,
        body: r.get(base + 5)?,
        created_at: r.get(base + 6)?,
        urgent: r.get(base + 7)?,
        reply_to: reply_to_id.map(|id| MessageReply {
            id,
            sender_handle: reply_handle.unwrap_or_default(),
            body: reply_snippet(reply_body.as_deref().unwrap_or_default()),
        }),
        edited_at: r.get(base + 11)?,
        task_status: r.get(base + 12)?,
    })
}

pub(super) fn map_message_row(r: &rusqlite::Row) -> rusqlite::Result<Message> {
    map_message_at(r, 0)
}

/// Fetch and re-read a message after a write so the response carries reply
/// context and edit/status fields.
fn reload(c: &rusqlite::Connection, id: i64) -> Result<Message, ApiError> {
    c.query_row(
        &format!("SELECT {MESSAGE_COLUMNS} FROM messages msg {MESSAGE_JOINS} WHERE msg.id = ?1"),
        params![id],
        map_message_row,
    )
    .map_err(ApiError::from)
}

struct VisibleMessage {
    sender_id: i64,
    /// The caller's role in the message's channel; None for DMs.
    channel_role: Option<String>,
}

/// Load a message only if the caller may see it. Channel messages require
/// channel membership in the caller's workspace (404 across workspaces, 403
/// within one); DM messages require being one of the two parties.
fn load_visible(
    c: &rusqlite::Connection,
    agent: &Agent,
    id: i64,
) -> Result<VisibleMessage, ApiError> {
    let row: Option<(Option<i64>, i64, Option<i64>, Option<i64>, Option<String>)> = c
        .query_row(
            "SELECT msg.channel_id, msg.sender_id, msg.recipient_id, ch.workspace_id,
                    (SELECT role FROM channel_members WHERE channel_id = msg.channel_id AND agent_id = ?2)
             FROM messages msg LEFT JOIN channels ch ON ch.id = msg.channel_id
             WHERE msg.id = ?1",
            params![id, agent.id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((channel_id, sender_id, recipient_id, workspace_id, channel_role)) = row else {
        return Err(ApiError::not_found("message not found"));
    };
    match channel_id {
        Some(_) => {
            if workspace_id != Some(agent.workspace_id) {
                return Err(ApiError::not_found("message not found"));
            }
            if channel_role.is_none() {
                return Err(ApiError::forbidden());
            }
        }
        None => {
            if sender_id != agent.id && recipient_id != Some(agent.id) {
                return Err(ApiError::not_found("message not found"));
            }
        }
    }
    Ok(VisibleMessage {
        sender_id,
        channel_role,
    })
}

pub async fn edit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<EditMessage>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = extract_id(Ok(id))?;
    let body = input.body.trim();
    if body.is_empty() {
        return Err(ApiError::bad_request("body must not be empty"));
    }
    let message = state.db.try_with(|c| {
        let msg = load_visible(c, &agent, id)?;
        if msg.sender_id != agent.id {
            return Err(ApiError::forbidden_msg(
                "only the sender can edit a message",
            ));
        }
        c.execute(
            "UPDATE messages SET body = ?1, edited_at = ?2 WHERE id = ?3",
            params![body, now_iso(), id],
        )?;
        reload(c, id)
    })?;
    Ok(Json(serde_json::json!({ "message": message })))
}

pub async fn set_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<SetTaskStatus>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = extract_id(Ok(id))?;
    let status = match input
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) if TASK_STATUSES.contains(&s) => Some(s),
        Some(_) => {
            return Err(ApiError::bad_request(
                "status must be one of claimed, blocked, done",
            ))
        }
        None => None,
    };
    let message = state.db.try_with(|c| {
        let msg = load_visible(c, &agent, id)?;
        let is_admin = msg.channel_role.as_deref() == Some("admin");
        if msg.sender_id != agent.id && !is_admin {
            return Err(ApiError::forbidden_msg(
                "only the sender or a channel admin can set the status",
            ));
        }
        c.execute(
            "UPDATE messages SET task_status = ?1 WHERE id = ?2",
            params![status, id],
        )?;
        reload(c, id)
    })?;
    Ok(Json(serde_json::json!({ "message": message })))
}
