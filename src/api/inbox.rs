use super::{require_agent, require_agent_stream};
use crate::error::ApiError;
use crate::events::LiveEvent;
use crate::models::{now_iso, InboxItem, Page, ReadInbox, page_params};
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::Json;
use futures_util::Stream;
use rusqlite::params;
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::StreamExt;

#[derive(serde::Deserialize)]
pub struct StreamQuery {
    pub token: Option<String>,
}

fn map_inbox_row(r: &rusqlite::Row) -> rusqlite::Result<InboxItem> {
    Ok(InboxItem {
        receipt_id: r.get(0)?,
        read_at: r.get(1)?,
        message: crate::models::Message {
            id: r.get(2)?,
            channel_id: r.get(3)?,
            sender_id: r.get(4)?,
            sender_handle: r.get(5)?,
            recipient_id: r.get(6)?,
            body: r.get(7)?,
            created_at: r.get(8)?,
            urgent: r.get(9)?,
        },
    })
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let (after_id, limit, unread) = page_params(&page);
    let unread_clause = if unread { " AND r.read_at IS NULL" } else { "" };
    let items = state.db.with(|c| {
        let mut stmt = c.prepare(&format!(
            "SELECT r.id, r.read_at, msg.id, msg.channel_id, msg.sender_id, a.handle,
                    msg.recipient_id, msg.body, msg.created_at, msg.urgent
             FROM receipts r
             JOIN messages msg ON msg.id = r.message_id
             JOIN agents a ON a.id = msg.sender_id
             WHERE r.agent_id = ?1 AND (?2 = 0 OR r.id < ?2){unread_clause}
             ORDER BY r.id DESC LIMIT ?3"
        ))?;
        stmt.query_map(params![agent.id, after_id, limit], map_inbox_row)
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    })?;
    Ok(Json(serde_json::json!({ "items": items })))
}

pub async fn mark_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ReadInbox>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    if input.all.is_none() && input.message_ids.is_none() {
        return Err(ApiError::bad_request("pass message_ids or all: true"));
    }
    let now = now_iso();
    let updated = state.db.with(|c| {
        if input.all == Some(true) {
            c.execute(
                "UPDATE receipts SET read_at = ?1 WHERE agent_id = ?2 AND read_at IS NULL",
                params![now, agent.id],
            )
        } else {
            let ids = input.message_ids.unwrap();
            let mut n = 0;
            for id in ids {
                n += c.execute(
                    "UPDATE receipts SET read_at = ?1 WHERE agent_id = ?2 AND message_id = ?3 AND read_at IS NULL",
                    params![now, agent.id, id],
                )?;
            }
            Ok(n)
        }
    })?;
    Ok(Json(serde_json::json!({ "updated": updated })))
}

/// Live inbox updates over Server-Sent Events. Requires `Last-Event-ID`-free
/// bearer auth like every other endpoint; each event is filtered so an agent
/// only ever sees its own inbox receipts.
pub async fn stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<StreamQuery>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    let agent = require_agent_stream(&state, &headers, q.token)?;
    let rx = state.events.subscribe();
    let agent_id = agent.id;
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx)
        .filter_map(move |ev| match ev {
            Ok(LiveEvent::InboxMessage { agent_id: to, receipt_id, message }) => {
                if to == agent_id {
                    Some(serde_json::json!({
                        "type": "inbox_message",
                        "receipt_id": receipt_id,
                        "message": message,
                    }).to_string())
                } else {
                    None
                }
            }
            Ok(LiveEvent::Mail { agent_id: to, mail }) => {
                if to == agent_id {
                    Some(serde_json::json!({ "type": "mail", "mail": mail }).to_string())
                } else {
                    None
                }
            }
            Err(_) => None, // sender lagged; the poll endpoint covers the gap
        })
        .map(|data| Ok::<_, Infallible>(SseEvent::default().data(data)));
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new().interval(Duration::from_secs(15)).text("keepalive"),
    ))
}
