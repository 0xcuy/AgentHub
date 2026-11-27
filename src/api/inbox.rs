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
use std::collections::HashSet;
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::StreamExt;

#[derive(serde::Deserialize)]
pub struct StreamQuery {
    pub token: Option<String>,
    pub since: Option<i64>,
}

/// Inbox list query: the shared page params plus the inbox-only `since`
/// forward cursor. A standalone struct (not `Page` + flatten) because
/// `serde_urlencoded` cannot deserialize flattened query params; other list
/// endpoints keep using `Page` untouched.
#[derive(serde::Deserialize)]
pub struct InboxQuery {
    pub after_id: Option<i64>,
    pub limit: Option<i64>,
    pub unread: Option<bool>,
    pub since: Option<i64>,
}

fn map_inbox_row(r: &rusqlite::Row) -> rusqlite::Result<InboxItem> {
    Ok(InboxItem {
        receipt_id: r.get(0)?,
        read_at: r.get(1)?,
        message: super::messages::map_message_at(r, 2)?,
    })
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<InboxQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let (after_id, limit, unread) = page_params(&Page {
        after_id: q.after_id,
        limit: q.limit,
        unread: q.unread,
    });
    let unread_clause = if unread { " AND r.read_at IS NULL" } else { "" };
    // `since` walks forward: only receipts newer than the cursor, in
    // ascending id order, so a reconnecting client cannot miss anything.
    // Without it the list stays exactly as before (newest first).
    let forward = q.since.is_some();
    let cursor = q.since.unwrap_or(after_id);
    let items = state.db.with(|c| {
        let mut stmt = c.prepare(&format!(
            "SELECT r.id, r.read_at, {message_columns}
             FROM receipts r
             JOIN messages msg ON msg.id = r.message_id
             {message_joins}
             WHERE r.agent_id = ?1 AND (?2 = 0 OR r.id {} ?2){unread_clause}
             ORDER BY r.id {} LIMIT ?3",
            if forward { ">" } else { "<" },
            if forward { "ASC" } else { "DESC" },
            message_columns = super::messages::MESSAGE_COLUMNS,
            message_joins = super::messages::MESSAGE_JOINS,
        ))?;
        stmt.query_map(params![agent.id, cursor, limit], map_inbox_row)
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
///
/// On connect the missed inbox items are replayed first, then live events
/// continue. With `?since=<receipt_id>` everything newer than that cursor is
/// replayed; without it the currently-unread items are. The broadcast
/// channel is subscribed BEFORE the replay query runs, so nothing published
/// in between is lost; receipts already replayed are deduped out of the
/// live stream.
pub async fn stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<StreamQuery>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    let agent = require_agent_stream(&state, &headers, q.token)?;
    // Subscribe first, so live events published while the replay query runs
    // are buffered instead of lost.
    let rx = state.events.subscribe();
    let agent_id = agent.id;

    // Replay set: newer than `since`, or the currently-unread items when no
    // cursor is given. Ascending order so the client walks forward in order.
    let replay_where = match q.since {
        Some(_) => "r.agent_id = ?1 AND r.id > ?2",
        None => "r.agent_id = ?1 AND r.read_at IS NULL",
    };
    let replay = state.db.with(|c| {
        let mut stmt = c.prepare(&format!(
            "SELECT r.id, r.read_at, {message_columns}
             FROM receipts r
             JOIN messages msg ON msg.id = r.message_id
             {message_joins}
             WHERE {replay_where}
             ORDER BY r.id ASC",
            message_columns = super::messages::MESSAGE_COLUMNS,
            message_joins = super::messages::MESSAGE_JOINS,
        ))?;
        let mapped = match q.since {
            Some(cursor) => stmt.query_map(params![agent.id, cursor], map_inbox_row)?,
            None => stmt.query_map(params![agent.id], map_inbox_row)?,
        };
        mapped.collect::<Result<Vec<_>, _>>()
    })?;
    let replayed: HashSet<i64> = replay.iter().map(|item| item.receipt_id).collect();
    let replay_events = replay
        .into_iter()
        .map(|item| {
            serde_json::json!({
                "type": "inbox_message",
                "receipt_id": item.receipt_id,
                "message": item.message,
            })
            .to_string()
        })
        .collect::<Vec<_>>();

    let live = tokio_stream::wrappers::BroadcastStream::new(rx)
        .filter_map(move |ev| match ev {
            Ok(LiveEvent::InboxMessage { agent_id: to, receipt_id, message }) => {
                if to == agent_id && !replayed.contains(&receipt_id) {
                    Some(serde_json::json!({
                        "type": "inbox_message",
                        "receipt_id": receipt_id,
                        "message": message,
                    }).to_string())
                } else {
                    None // another agent's receipt, or already replayed above
                }
            }
            Ok(LiveEvent::Mail { agent_id: to, mail }) => {
                if to == agent_id {
                    Some(serde_json::json!({ "type": "mail", "mail": mail }).to_string())
                } else {
                    None
                }
            }
            Err(_) => None, // sender lagged; the replay above covers the gap
        });
    let stream = tokio_stream::iter(replay_events)
        .chain(live)
        .map(|data| Ok::<_, Infallible>(SseEvent::default().data(data)));
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new().interval(Duration::from_secs(15)).text("keepalive"),
    ))
}
