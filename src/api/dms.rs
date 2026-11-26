use super::{extract_id, require_agent};
use crate::error::ApiError;
use crate::models::{now_iso, CreateMessage, Message, Page, page_params};
use crate::state::AppState;
use crate::triggers;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::params;

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

pub async fn list_partners(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let partners = state.db.with(|c| {
        let mut stmt = c.prepare(
            "SELECT DISTINCT a.id, a.handle, a.name FROM agents a
             WHERE a.id IN (
               SELECT CASE WHEN sender_id = ?1 THEN recipient_id ELSE sender_id END
               FROM messages WHERE channel_id IS NULL AND (sender_id = ?1 OR recipient_id = ?1)
             ) ORDER BY a.id",
        )?;
        stmt.query_map([agent.id], |r| {
            Ok(serde_json::json!({ "id": r.get::<_, i64>(0)?, "handle": r.get::<_, String>(1)?, "name": r.get::<_, String>(2)? }))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    })?;
    Ok(Json(serde_json::json!({ "partners": partners })))
}

pub async fn send(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(recipient_id): Path<i64>,
    Json(input): Json<CreateMessage>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let recipient_id = extract_id(Ok(recipient_id))?;
    if recipient_id == agent.id {
        return Err(ApiError::bad_request("cannot DM yourself"));
    }
    if input.body.trim().is_empty() {
        return Err(ApiError::bad_request("body must not be empty"));
    }
    let idem_key: Option<String> = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let (message, run_ids, receipt_id) = state.db.with(|c| {
        let tx = c.transaction()?;
        let recipient_exists: i64 = tx.query_row(
            "SELECT COUNT(*) FROM agents WHERE id = ?1",
            [recipient_id],
            |r| r.get(0),
        )?;
        if recipient_exists == 0 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
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
                return Ok((existing, Vec::new(), None));
            }
        }
        let now = now_iso();
        let msg_id: i64 = tx.query_row(
            "INSERT INTO messages (sender_id, recipient_id, body, idempotency_key, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
            params![agent.id, recipient_id, input.body.trim(), idem_key, now],
            |r| r.get(0),
        )?;
        let receipt_id: i64 = tx.query_row(
            "INSERT INTO receipts (message_id, agent_id) VALUES (?1, ?2) RETURNING id",
            params![msg_id, recipient_id],
            |r| r.get(0),
        )?;
        let run_ids = triggers::enqueue_runs(&tx, msg_id, &[recipient_id], "message")?;
        tx.commit()?;
        Ok((
            Message {
                id: msg_id,
                channel_id: None,
                sender_id: agent.id,
                sender_handle: agent.handle.clone(),
                recipient_id: Some(recipient_id),
                body: input.body.trim().to_string(),
                created_at: now,
            },
            run_ids,
            Some(receipt_id),
        ))
    }).map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => ApiError::not_found("recipient not found"),
        other => other.into(),
    })?;

    if let Some(receipt_id) = receipt_id {
        state.events.publish(crate::events::LiveEvent::InboxMessage {
            agent_id: recipient_id,
            receipt_id,
            message: message.clone(),
        });
    }
    if !run_ids.is_empty() {
        // Runs are queued in SQLite; the outbox dispatcher delivers them.
        state.outbox_notify.notify_waiters();
    }
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "message": message }))))
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(other_id): Path<i64>,
    Query(page): Query<Page>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let other_id = extract_id(Ok(other_id))?;
    let (after_id, limit, _) = page_params(&page);
    let messages = state.db.with(|c| {
        let mut stmt = c.prepare(
            "SELECT msg.id, msg.channel_id, msg.sender_id, a.handle, msg.recipient_id, msg.body, msg.created_at
             FROM messages msg JOIN agents a ON a.id = msg.sender_id
             WHERE msg.channel_id IS NULL
               AND ((msg.sender_id = ?1 AND msg.recipient_id = ?2)
                 OR (msg.sender_id = ?2 AND msg.recipient_id = ?1))
               AND (?3 = 0 OR msg.id < ?3)
             ORDER BY msg.id DESC LIMIT ?4",
        )?;
        stmt.query_map(params![agent.id, other_id, after_id, limit], map_message_row)
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    })?;
    Ok(Json(serde_json::json!({ "messages": messages })))
}
