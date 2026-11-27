use super::{extract_id, is_unique_violation, require_agent};
use crate::error::ApiError;
use crate::models::{now_iso, Agent, Message};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Reaction names accepted from agents and the UI. The UI renders each name
/// as a glyph; the API stores the stable name so clients never send emoji.
pub const ALLOWED: &[&str] = &["seen", "ok", "done", "up", "down", "heart"];

fn validate(emoji: &str) -> Result<(), ApiError> {
    if !ALLOWED.contains(&emoji) {
        return Err(ApiError::bad_request(
            "emoji must be one of: seen, ok, done, up, down, heart",
        ));
    }
    Ok(())
}

/// True when `agent` may see (and so react to) the message: a channel member
/// for channel messages, a participant for DMs. Foreign-workspace messages
/// read as nonexistent.
fn can_see(c: &Connection, msg_id: i64, agent: &Agent) -> Result<bool, ApiError> {
    let row = c.query_row(
        "SELECT channel_id, sender_id, recipient_id FROM messages WHERE id = ?1",
        params![msg_id],
        |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        },
    );
    let (channel_id, sender_id, recipient_id) = match row {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if let Some(ch) = channel_id {
        let member: i64 = c.query_row(
            "SELECT COUNT(*) FROM channel_members cm
             JOIN channels ch ON ch.id = cm.channel_id
             WHERE cm.channel_id = ?1 AND cm.agent_id = ?2 AND ch.workspace_id = ?3",
            params![ch, agent.id, agent.workspace_id],
            |r| r.get(0),
        )?;
        return Ok(member > 0);
    }
    if sender_id != Some(agent.id) && recipient_id != Some(agent.id) {
        return Ok(false);
    }
    let other_id = if sender_id == Some(agent.id) {
        recipient_id
    } else {
        sender_id
    };
    match other_id {
        Some(id) => {
            let same_workspace: i64 = c.query_row(
                "SELECT COUNT(*) FROM agents WHERE id = ?1 AND workspace_id = ?2",
                params![id, agent.workspace_id],
                |r| r.get(0),
            )?;
            Ok(same_workspace > 0)
        }
        None => Ok(false),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddReaction {
    pub emoji: String,
}

/// POST /v1/messages/{id}/reactions — add the caller's reaction. Idempotent:
/// a duplicate returns 200 with the existing reaction instead of an error.
pub async fn add(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<AddReaction>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = extract_id(Ok(id))?;
    let emoji = input.emoji.trim().to_lowercase();
    validate(&emoji)?;
    let now = now_iso();
    let (created, created_at) = state.db.try_with(|c| {
        if !can_see(c, id, &agent)? {
            return Err(ApiError::not_found("message not found"));
        }
        match c.execute(
            "INSERT INTO reactions (message_id, agent_id, emoji, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, agent.id, emoji, now],
        ) {
            Ok(_) => Ok((true, now.clone())),
            Err(e) if is_unique_violation(&e) => {
                let created_at: String = c.query_row(
                    "SELECT created_at FROM reactions
                     WHERE message_id = ?1 AND agent_id = ?2 AND emoji = ?3",
                    params![id, agent.id, emoji],
                    |r| r.get(0),
                )?;
                Ok((false, created_at))
            }
            Err(e) => Err(e.into()),
        }
    })?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(json!({
            "reaction": {
                "message_id": id,
                "agent_id": agent.id,
                "emoji": emoji,
                "created_at": created_at,
            }
        })),
    ))
}

/// DELETE /v1/messages/{id}/reactions/{emoji} — remove the caller's reaction.
pub async fn remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, emoji)): Path<(i64, String)>,
) -> Result<StatusCode, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = extract_id(Ok(id))?;
    let emoji = emoji.trim().to_lowercase();
    validate(&emoji)?;
    let deleted = state.db.try_with(|c| {
        if !can_see(c, id, &agent)? {
            return Err(ApiError::not_found("message not found"));
        }
        c.execute(
            "DELETE FROM reactions WHERE message_id = ?1 AND agent_id = ?2 AND emoji = ?3",
            params![id, agent.id, emoji],
        )
        .map_err(ApiError::from)
    })?;
    if deleted == 0 {
        return Err(ApiError::not_found("reaction not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Aggregate reaction counts for a page of messages into
/// `{"emoji": "ok", "count": 2, "mine": true}` entries per message id.
pub fn attach_reactions(
    c: &Connection,
    ids: &[i64],
    viewer: i64,
    out: &mut HashMap<i64, Vec<Value>>,
) -> rusqlite::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let mut stmt = c.prepare(&format!(
        "SELECT message_id, emoji, COUNT(*), MAX(agent_id = {viewer})
         FROM reactions WHERE message_id IN ({list})
         GROUP BY message_id, emoji ORDER BY MIN(id)"
    ))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let msg_id: i64 = row.get(0)?;
        out.entry(msg_id).or_default().push(json!({
            "emoji": row.get::<_, String>(1)?,
            "count": row.get::<_, i64>(2)?,
            "mine": row.get::<_, i64>(3)? != 0,
        }));
    }
    Ok(())
}

/// Read-by view for DM messages: the recipient's receipt carries `read_at`
/// once they mark it read via POST /v1/inbox/read. Channel messages have no
/// per-recipient receipts, so this only adds entries where receipts exist.
pub fn attach_read_by(
    c: &Connection,
    ids: &[i64],
    out: &mut HashMap<i64, Vec<Value>>,
) -> rusqlite::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let mut stmt = c.prepare(&format!(
        "SELECT r.message_id, a.id, a.handle, r.read_at
         FROM receipts r JOIN agents a ON a.id = r.agent_id
         WHERE r.message_id IN ({list}) AND r.read_at IS NOT NULL
         ORDER BY r.read_at"
    ))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let msg_id: i64 = row.get(0)?;
        out.entry(msg_id).or_default().push(json!({
            "agent_id": row.get::<_, i64>(1)?,
            "handle": row.get::<_, String>(2)?,
            "read_at": row.get::<_, String>(3)?,
        }));
    }
    Ok(())
}

/// Serialize messages with `reactions` (and, when given, `read_by`) attached.
pub fn decorate(
    messages: Vec<Message>,
    mut reactions: HashMap<i64, Vec<Value>>,
    mut read_by: Option<HashMap<i64, Vec<Value>>>,
) -> Vec<Value> {
    messages
        .into_iter()
        .map(|m| {
            let mut v = serde_json::to_value(&m).unwrap_or_else(|_| json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "reactions".to_string(),
                    Value::Array(reactions.remove(&m.id).unwrap_or_default()),
                );
                if let Some(by_id) = read_by.as_mut() {
                    obj.insert(
                        "read_by".to_string(),
                        Value::Array(by_id.remove(&m.id).unwrap_or_default()),
                    );
                }
            }
            v
        })
        .collect()
}
