use super::require_agent;
use crate::error::ApiError;
use crate::events::LiveEvent;
use crate::models::{now_iso, CreateMail, MailMessage, Page, page_params};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::params;

const MAIL_COLS: &str =
    "m.id, m.sender_id, a.handle, m.recipient_id, m.subject, m.body, m.read_at, m.created_at";

fn map_mail_row(r: &rusqlite::Row) -> rusqlite::Result<MailMessage> {
    Ok(MailMessage {
        id: r.get(0)?,
        sender_id: r.get(1)?,
        sender_handle: r.get(2)?,
        recipient_id: r.get(3)?,
        subject: r.get(4)?,
        body: r.get(5)?,
        read_at: r.get(6)?,
        created_at: r.get(7)?,
    })
}

pub async fn send(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateMail>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    if input.subject.trim().is_empty() {
        return Err(ApiError::bad_request("subject must not be empty"));
    }
    if input.body.trim().is_empty() {
        return Err(ApiError::bad_request("body must not be empty"));
    }
    let recipient_id = match (input.to_agent_id, &input.to_handle) {
        (Some(id), _) => id,
        (None, Some(handle)) => state.db.try_with(|c| {
            // Handle lookup is scoped to the caller's workspace: a handle
            // from another workspace reads as unknown (404).
            c.query_row(
                "SELECT id FROM agents WHERE handle = ?1 AND workspace_id = ?2",
                params![handle, agent.workspace_id],
                |r| r.get(0),
            )
            .map_err(|_| ApiError::not_found("recipient handle not found"))
        })?,
        _ => return Err(ApiError::bad_request("pass to_agent_id or to_handle")),
    };
    if recipient_id == agent.id {
        return Err(ApiError::bad_request("cannot send mail to yourself"));
    }
    state.db.try_with(|c| {
        let exists: i64 = c.query_row(
            // Same workspace only: mailing into another workspace must fail
            // with 404 so foreign agents' existence never leaks.
            "SELECT COUNT(*) FROM agents WHERE id = ?1 AND workspace_id = ?2",
            params![recipient_id, agent.workspace_id],
            |r| r.get(0),
        )?;
        if exists == 0 {
            Err(ApiError::not_found("recipient not found"))
        } else {
            Ok(())
        }
    })?;
    let mail = state.db.try_with(|c| {
        let id: i64 = c.query_row(
            "INSERT INTO mailbox_messages (sender_id, recipient_id, subject, body, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
            params![agent.id, recipient_id, input.subject.trim(), input.body.trim(), now_iso()],
            |r| r.get::<_, i64>(0),
        )?;
        c.query_row(
            &format!(
                "SELECT {MAIL_COLS} FROM mailbox_messages m JOIN agents a ON a.id = m.sender_id WHERE m.id = ?1"
            ),
            [id],
            map_mail_row,
        )
        .map_err(ApiError::from)
    })?;
    state.events.publish(LiveEvent::Mail { agent_id: recipient_id, mail: mail.clone() });
    Ok((axum::http::StatusCode::CREATED, Json(serde_json::json!({ "mail": mail }))))
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let (after_id, limit, unread) = page_params(&page);
    let unread_clause = if unread { " AND m.read_at IS NULL" } else { "" };
    let mail = state.db.with(|c| {
        let mut stmt = c.prepare(&format!(
            "SELECT {MAIL_COLS} FROM mailbox_messages m JOIN agents a ON a.id = m.sender_id
             WHERE m.recipient_id = ?1 AND (?2 = 0 OR m.id < ?2){unread_clause}
             ORDER BY m.id DESC LIMIT ?3"
        ))?;
        stmt.query_map(params![agent.id, after_id, limit], map_mail_row)
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    })?;
    Ok(Json(serde_json::json!({ "mail": mail })))
}

pub async fn mark_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = super::extract_id(Ok(id))?;
    let mail = state.db.try_with(|c| {
        let updated = c.execute(
            "UPDATE mailbox_messages SET read_at = ?1 WHERE id = ?2 AND recipient_id = ?3",
            params![now_iso(), id, agent.id],
        )?;
        if updated == 0 {
            return Err(ApiError::not_found("mail not found for you"));
        }
        c.query_row(
            &format!(
                "SELECT {MAIL_COLS} FROM mailbox_messages m JOIN agents a ON a.id = m.sender_id WHERE m.id = ?1"
            ),
            [id],
            map_mail_row,
        )
        .map_err(ApiError::from)
    })?;
    Ok(Json(serde_json::json!({ "mail": mail })))
}
