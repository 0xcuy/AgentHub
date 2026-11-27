use super::{extract_id, is_unique_violation, require_agent};
use crate::error::ApiError;
use crate::events::LiveEvent;
use crate::models::{
    now_iso, valid_post_policy, Channel, CreateChannel, CreateMessage, Message, Page, UpdateChannel,
    page_params,
};
use crate::state::AppState;
use crate::triggers;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::params;
use rusqlite::OptionalExtension;

const CHANNEL_COLS: &str = "id, name, topic, created_at, post_policy";

fn map_channel_row(r: &rusqlite::Row) -> rusqlite::Result<Channel> {
    Ok(Channel {
        id: r.get(0)?,
        name: r.get(1)?,
        topic: r.get(2)?,
        created_at: r.get(3)?,
        post_policy: r.get(4)?,
    })
}

fn map_message_row(r: &rusqlite::Row) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        channel_id: r.get(1)?,
        sender_id: r.get(2)?,
        sender_handle: r.get(3)?,
        recipient_id: r.get(4)?,
        body: r.get(5)?,
        created_at: r.get(6)?,
    })
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateChannel>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    if input.name.trim().is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }
    let post_policy = input.post_policy.clone().unwrap_or_else(|| "members".into());
    if !valid_post_policy(&post_policy) {
        return Err(ApiError::bad_request("post_policy must be 'members' or 'admins'"));
    }
    let now = now_iso();
    let channel = state
        .db
        .with(|c| {
            c.query_row(
                &format!(
                    "INSERT INTO channels (workspace_id, name, topic, post_policy, created_at) VALUES (?1, ?2, ?3, ?4, ?5) RETURNING {CHANNEL_COLS}"
                ),
                params![agent.workspace_id, input.name.trim(), input.topic, post_policy, now],
                map_channel_row,
            )
        })
        .map_err(|e| {
            if is_unique_violation(&e) {
                ApiError::conflict("channel name already taken")
            } else {
                e.into()
            }
        })?;
    state.db.try_with(|c| {
        let mut members = vec![agent.id];
        members.extend(input.member_ids.iter().copied());
        members.sort();
        members.dedup();
        for member_id in members {
            if member_id != agent.id {
                // Only agents of the caller's workspace may be added: a
                // foreign agent id must not reveal the channel's existence.
                let same_ws: i64 = c.query_row(
                    "SELECT COUNT(*) FROM agents WHERE id = ?1 AND workspace_id = ?2",
                    params![member_id, agent.workspace_id],
                    |r| r.get(0),
                )?;
                if same_ws == 0 {
                    return Err(ApiError::not_found("agent not found"));
                }
            }
            let role = if member_id == agent.id { "admin" } else { "member" };
            c.execute(
                "INSERT INTO channel_members (channel_id, agent_id, joined_at, role) VALUES (?1, ?2, ?3, ?4)",
                params![channel.id, member_id, now, role],
            )?;
        }
        Ok(())
    })?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "channel": channel }))))
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let channels = state.db.with(|c| {
        let mut stmt = c.prepare(&format!(
            "SELECT {CHANNEL_COLS} FROM channels WHERE workspace_id = ?1 ORDER BY id"
        ))?;
        stmt.query_map([agent.workspace_id], map_channel_row)
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    })?;
    Ok(Json(serde_json::json!({ "channels": channels })))
}

pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<UpdateChannel>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = extract_id(Ok(id))?;
    if let Some(p) = &input.post_policy {
        if !valid_post_policy(p) {
            return Err(ApiError::bad_request("post_policy must be 'members' or 'admins'"));
        }
    }
    state.db.try_with(|c| {
        let tx = c.transaction()?;
        // Workspace scoping: a channel from another workspace reads as
        // nonexistent (404), like every other channel surface. Only then is
        // the admin check applied (403 for same-workspace non-admins).
        let in_workspace: i64 = tx.query_row(
            "SELECT COUNT(*) FROM channels WHERE id = ?1 AND workspace_id = ?2",
            params![id, agent.workspace_id],
            |r| r.get(0),
        )?;
        if in_workspace == 0 {
            return Err(ApiError::not_found("channel not found"));
        }
        let role: Option<String> = tx
            .query_row(
                "SELECT cm.role FROM channel_members cm
                 WHERE cm.channel_id = ?1 AND cm.agent_id = ?2",
                params![id, agent.id],
                |r| r.get(0),
            )
            .optional()?;
        if role.as_deref() != Some("admin") {
            return Err(ApiError::forbidden_msg("only channel admins can update the channel"));
        }
        if input.topic.is_some() {
            tx.execute(
                "UPDATE channels SET topic = ?1 WHERE id = ?2",
                params![input.topic, id],
            )?;
        }
        if let Some(p) = &input.post_policy {
            tx.execute(
                "UPDATE channels SET post_policy = ?1 WHERE id = ?2",
                params![p, id],
            )?;
        }
        tx.commit()?;
        Ok(())
    })?;
    let channel = state.db.with(|c| {
        c.query_row(
            &format!("SELECT {CHANNEL_COLS} FROM channels WHERE id = ?1"),
            [id],
            map_channel_row,
        )
    })?;
    Ok(Json(serde_json::json!({ "channel": channel })))
}

pub async fn add_members(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<crate::models::AddMembers>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = extract_id(Ok(id))?;
    if input.agent_ids.is_empty() {
        return Err(ApiError::bad_request("agent_ids must not be empty"));
    }
    let now = now_iso();
    state.db.try_with(|c| {
        let tx = c.transaction()?;
        // Channel must exist in the caller's workspace, and the caller must be
        // its admin. Cross-workspace channels read as nonexistent (404).
        let channel_exists: i64 = tx.query_row(
            "SELECT COUNT(*) FROM channels WHERE id = ?1 AND workspace_id = ?2",
            params![id, agent.workspace_id],
            |r| r.get(0),
        )?;
        if channel_exists == 0 {
            return Err(ApiError::not_found("channel not found"));
        }
        let role: Option<String> = tx
            .query_row(
                "SELECT role FROM channel_members WHERE channel_id = ?1 AND agent_id = ?2",
                params![id, agent.id],
                |r| r.get(0),
            )
            .optional()?;
        if role.as_deref() != Some("admin") {
            return Err(ApiError::forbidden_msg("only channel admins can add members"));
        }
        for member_id in &input.agent_ids {
            let exists: i64 = tx.query_row(
                "SELECT COUNT(*) FROM agents WHERE id = ?1 AND workspace_id = ?2",
                params![member_id, agent.workspace_id],
                |r| r.get(0),
            )?;
            if exists == 0 {
                // Never leak that an agent of another workspace exists.
                return Err(ApiError::not_found("agent not found"));
            }
        }
        for member_id in &input.agent_ids {
            tx.execute(
                "INSERT OR IGNORE INTO channel_members (channel_id, agent_id, joined_at, role) VALUES (?1, ?2, ?3, 'member')",
                params![id, member_id, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    })?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn remove_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, member_id)): Path<(i64, i64)>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = extract_id(Ok(id))?;
    let member_id = extract_id(Ok(member_id))?;
    state.db.try_with(|c| {
        let tx = c.transaction()?;
        let role: Option<String> = tx
            .query_row(
                "SELECT cm.role FROM channel_members cm
                 JOIN channels ch ON ch.id = cm.channel_id
                 WHERE cm.channel_id = ?1 AND cm.agent_id = ?2 AND ch.workspace_id = ?3",
                params![id, agent.id, agent.workspace_id],
                |r| r.get(0),
            )
            .optional()?;
        if role.as_deref() != Some("admin") {
            return Err(ApiError::forbidden_msg("only channel admins can remove members"));
        }
        let target_role: Option<String> = tx
            .query_row(
                "SELECT role FROM channel_members WHERE channel_id = ?1 AND agent_id = ?2",
                params![id, member_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(target_role) = target_role else {
            return Err(ApiError::not_found("member not found"));
        };
        if target_role == "admin" {
            let admins: i64 = tx.query_row(
                "SELECT COUNT(*) FROM channel_members WHERE channel_id = ?1 AND role = 'admin'",
                [id],
                |r| r.get(0),
            )?;
            if admins <= 1 {
                return Err(ApiError::bad_request("cannot remove the last channel admin"));
            }
        }
        tx.execute(
            "DELETE FROM channel_members WHERE channel_id = ?1 AND agent_id = ?2",
            params![id, member_id],
        )?;
        tx.commit()?;
        Ok(())
    })?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn send_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<CreateMessage>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = extract_id(Ok(id))?;
    if input.body.trim().is_empty() {
        return Err(ApiError::bad_request("body must not be empty"));
    }
    let idem_key: Option<String> = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let (message, run_ids, receipts) = state.db.try_with(|c| {
        let tx = c.transaction()?;
        let row: (String, Option<String>) = match tx.query_row(
            "SELECT post_policy,
                    (SELECT role FROM channel_members WHERE channel_id = ?1 AND agent_id = ?2)
             FROM channels WHERE id = ?1 AND workspace_id = ?3",
            params![id, agent.id, agent.workspace_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ) {
            Ok(v) => v,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Err(ApiError::not_found("channel not found"))
            }
            Err(e) => return Err(e.into()),
        };
        let (post_policy, role) = row;
        let Some(role) = role else {
            return Err(ApiError::forbidden());
        };
        if post_policy == "admins" && role != "admin" {
            return Err(ApiError::forbidden_msg(
                "only channel admins can post in this channel",
            ));
        }
        if let Some(key) = &idem_key {
            if let Ok(existing) = tx.query_row(
                "SELECT msg.id, msg.channel_id, msg.sender_id, a.handle, msg.recipient_id, msg.body, msg.created_at
                 FROM messages msg JOIN agents a ON a.id = msg.sender_id
                 WHERE msg.sender_id = ?1 AND msg.idempotency_key = ?2",
                params![agent.id, key],
                map_message_row,
            ) {
                tx.commit()?;
                return Ok((existing, Vec::new(), Vec::new()));
            }
        }
        let now = now_iso();
        let msg_id: i64 = tx.query_row(
            "INSERT INTO messages (channel_id, sender_id, body, idempotency_key, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
            params![id, agent.id, input.body.trim(), idem_key, now],
            |r| r.get(0),
        )?;
        let message = Message {
            id: msg_id,
            channel_id: Some(id),
            sender_id: agent.id,
            sender_handle: agent.handle.clone(),
            recipient_id: None,
            body: input.body.trim().to_string(),
            created_at: now,
        };
        let recipient_ids: Vec<i64> = {
            let mut stmt = tx.prepare(
                "SELECT agent_id FROM channel_members WHERE channel_id = ?1 AND agent_id != ?2",
            )?;
            stmt.query_map(params![id, agent.id], |r| r.get(0))
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())?
        };
        let mut receipts = Vec::new();
        for rid in &recipient_ids {
            let receipt_id: i64 = tx.query_row(
                "INSERT INTO receipts (message_id, agent_id) VALUES (?1, ?2) RETURNING id",
                params![message.id, rid],
                |r| r.get(0),
            )?;
            receipts.push((*rid, receipt_id));
        }
        let run_ids = triggers::enqueue_runs(&tx, message.id, &recipient_ids, "message")?;
        tx.commit()?;
        Ok((message, run_ids, receipts))
    })?;

    for (agent_id, receipt_id) in &receipts {
        state.events.publish(LiveEvent::InboxMessage {
            agent_id: *agent_id,
            receipt_id: *receipt_id,
            message: message.clone(),
        });
    }
    if !run_ids.is_empty() {
        state.outbox_notify.notify_waiters();
    }
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "message": message }))))
}

pub async fn list_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(page): Query<Page>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = extract_id(Ok(id))?;
    let (after_id, limit, _) = page_params(&page);
    let messages = state.db.try_with(|c| {
        let is_member: i64 = c.query_row(
            "SELECT COUNT(*) FROM channel_members cm
             JOIN channels ch ON ch.id = cm.channel_id
             WHERE cm.channel_id = ?1 AND cm.agent_id = ?2 AND ch.workspace_id = ?3",
            params![id, agent.id, agent.workspace_id],
            |r| r.get(0),
        )?;
        if is_member == 0 {
            // Another workspace's channel reads as nonexistent (404); a
            // same-workspace non-member gets the historical 403.
            let exists: i64 = c.query_row(
                "SELECT COUNT(*) FROM channels WHERE id = ?1 AND workspace_id = ?2",
                params![id, agent.workspace_id],
                |r| r.get(0),
            )?;
            return Err(if exists == 0 {
                ApiError::not_found("channel not found")
            } else {
                ApiError::forbidden()
            });
        }
        let mut stmt = c.prepare(
            "SELECT msg.id, msg.channel_id, msg.sender_id, a.handle, msg.recipient_id, msg.body, msg.created_at
             FROM messages msg JOIN agents a ON a.id = msg.sender_id
             WHERE msg.channel_id = ?1 AND (?2 = 0 OR msg.id < ?2) ORDER BY msg.id DESC LIMIT ?3",
        )?;
        stmt.query_map(params![id, after_id, limit], map_message_row)
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .map_err(ApiError::from)
    })?;
    Ok(Json(serde_json::json!({ "messages": messages })))
}
