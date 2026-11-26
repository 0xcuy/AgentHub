use super::{extract_id, require_agent};
use crate::embeddings::{blob_to_vec, cosine, vec_to_blob};
use crate::error::ApiError;
use crate::models::{
    now_iso, valid_memory_kind, valid_memory_scope, CreateMemory, Memory, MemoryQuery,
};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::types::Value as SqlValue;
use rusqlite::OptionalExtension;

const MAX_CONTENT_CHARS: usize = 100_000;
const MAX_TAGS: usize = 32;
const MAX_TAG_CHARS: usize = 128;
const FTS_CANDIDATES: i64 = 200;

const MEMORY_COLS: &str = "m.id, m.agent_id, a.handle, m.scope, m.channel_id, m.kind, m.content, m.tags, m.source_message_id, m.source_channel_id, m.created_at";

/// Memories the caller may see: their own private ones plus channel
/// memories of channels they are a member of. Private memories of other
/// agents are never visible through any filter combination.
fn visibility_where() -> &'static str {
    "(m.scope = 'private' AND m.agent_id = ?1 \
     OR m.scope = 'channel' AND m.channel_id IN \
     (SELECT channel_id FROM channel_members WHERE agent_id = ?1))"
}

fn map_memory_row(r: &rusqlite::Row) -> rusqlite::Result<Memory> {
    let tags_raw: String = r.get(7)?;
    let tags: Vec<String> =
        serde_json::from_str(&tags_raw).unwrap_or_default();
    Ok(Memory {
        id: r.get(0)?,
        agent_id: r.get(1)?,
        agent_handle: r.get(2)?,
        scope: r.get(3)?,
        channel_id: r.get(4)?,
        kind: r.get(5)?,
        content: r.get(6)?,
        tags,
        source_message_id: r.get(8)?,
        source_channel_id: r.get(9)?,
        created_at: r.get(10)?,
    })
}

/// Turn a free-text query into a safe FTS5 MATCH expression: quoted tokens
/// with prefix matching, AND-ed together, capped in number.
fn fts_match_expr(q: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    for token in q.split_whitespace() {
        let cleaned: String = token.chars().filter(|c| *c != '"').collect();
        if cleaned.is_empty() {
            continue;
        }
        terms.push(format!("\"{cleaned}\"*"));
        if terms.len() >= 16 {
            break;
        }
    }
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateMemory>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let content = input.content.trim().to_string();
    if content.is_empty() {
        return Err(ApiError::bad_request("content must not be empty"));
    }
    if content.chars().count() > MAX_CONTENT_CHARS {
        return Err(ApiError::bad_request("content is too long"));
    }
    if !valid_memory_kind(&input.kind) {
        return Err(ApiError::bad_request(
            "kind must be one of 'fakta', 'keputusan', 'preferensi'",
        ));
    }
    let scope = input.scope.clone().unwrap_or_else(|| "private".into());
    if !valid_memory_scope(&scope) {
        return Err(ApiError::bad_request("scope must be 'private' or 'channel'"));
    }
    let mut tags: Vec<String> = input
        .tags
        .iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    tags.sort();
    tags.dedup();
    if tags.len() > MAX_TAGS {
        return Err(ApiError::bad_request("too many tags (max 32)"));
    }
    if tags.iter().any(|t| t.chars().count() > MAX_TAG_CHARS) {
        return Err(ApiError::bad_request("tag too long (max 128 chars)"));
    }
    let channel_id = match (scope.as_str(), input.channel_id) {
        ("channel", None) => {
            return Err(ApiError::bad_request("channel_id is required when scope is 'channel'"))
        }
        ("channel", Some(id)) => Some(id),
        ("private", Some(_)) => {
            return Err(ApiError::bad_request("channel_id is only valid when scope is 'channel'"))
        }
        ("private", None) => None,
        _ => unreachable!(),
    };

    let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".into());
    let now = now_iso();
    let cap = state.memory_cap;
    let memory = state.db.try_with(|c| {
        let tx = c.transaction()?;
        let owned: i64 = tx.query_row(
            "SELECT COUNT(*) FROM memories WHERE agent_id = ?1",
            [agent.id],
            |r| r.get(0),
        )?;
        if owned >= cap as i64 {
            return Err(ApiError::with_code(
                StatusCode::FORBIDDEN,
                "memory_cap_reached",
                format!(
                    "memory cap reached ({cap} memories per agent); delete memories before saving more"
                ),
            ));
        }
        if let Some(cid) = channel_id {
            let member: i64 = tx.query_row(
                "SELECT COUNT(*) FROM channel_members WHERE channel_id = ?1 AND agent_id = ?2",
                rusqlite::params![cid, agent.id],
                |r| r.get(0),
            )?;
            if member == 0 {
                // Distinguish 404 (no such channel) from 403 (not a member).
                let exists: i64 = tx
                    .query_row("SELECT COUNT(*) FROM channels WHERE id = ?1", [cid], |r| r.get(0))?;
                if exists == 0 {
                    return Err(ApiError::not_found("channel not found"));
                }
                return Err(ApiError::forbidden_msg(
                    "you are not a member of this channel",
                ));
            }
        }
        if let Some(mid) = input.source_message_id {
            let exists: i64 = tx
                .query_row("SELECT COUNT(*) FROM messages WHERE id = ?1", [mid], |r| r.get(0))?;
            if exists == 0 {
                return Err(ApiError::bad_request("source_message_id does not exist"));
            }
        }
        if let Some(cid) = input.source_channel_id {
            let exists: i64 = tx
                .query_row("SELECT COUNT(*) FROM channels WHERE id = ?1", [cid], |r| r.get(0))?;
            if exists == 0 {
                return Err(ApiError::bad_request("source_channel_id does not exist"));
            }
        }
        let id: i64 = tx.query_row(
            "INSERT INTO memories
               (agent_id, scope, channel_id, kind, content, tags, source_message_id, source_channel_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING id",
            rusqlite::params![
                agent.id, scope, channel_id, input.kind, content, tags_json,
                input.source_message_id, input.source_channel_id, now
            ],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO memories_fts (content, memory_id) VALUES (?1, ?2)",
            rusqlite::params![content, id],
        )?;
        tx.commit()?;
        Ok(Memory {
            id,
            agent_id: agent.id,
            agent_handle: agent.handle.clone(),
            scope,
            channel_id,
            kind: input.kind,
            content,
            tags,
            source_message_id: input.source_message_id,
            source_channel_id: input.source_channel_id,
            created_at: now,
        })
    })?;

    // Vector layer (optional): embed outside the DB lock. Failure to embed is
    // not fatal — the memory remains fully usable via FTS5.
    if state.embeddings.enabled() {
        let text = memory.content.clone();
        match state.embeddings.embed(vec![text]).await {
            Ok(vectors) => {
                if let Some(v) = vectors.into_iter().next() {
                    let dim = v.len() as i64;
                    let blob = vec_to_blob(&v);
                    if let Err(e) = state.db.with(|c| {
                        c.execute(
                            "INSERT OR REPLACE INTO memory_embeddings (memory_id, dim, vector) VALUES (?1, ?2, ?3)",
                            rusqlite::params![memory.id, dim, blob],
                        )
                    }) {
                        tracing::warn!(error = %e, memory_id = memory.id, "failed to store embedding");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, memory_id = memory.id, "embedding failed; fts-only");
            }
        }
    }

    Ok((StatusCode::CREATED, Json(serde_json::json!({ "memory": memory }))))
}

pub async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MemoryQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    if let Some(kind) = &q.kind {
        if !valid_memory_kind(kind) {
            return Err(ApiError::bad_request(
                "kind must be one of 'fakta', 'keputusan', 'preferensi'",
            ));
        }
    }
    if let Some(scope) = &q.scope {
        if !valid_memory_scope(scope) {
            return Err(ApiError::bad_request("scope must be 'private' or 'channel'"));
        }
    }

    // Shared WHERE: visibility plus optional filters. Placeholders are
    // numbered (`?N`) and pushed together with their values so the SQL and
    // the parameter list stay in sync for every filter combination.
    let mut filters = String::new();
    let mut vals: Vec<SqlValue> = vec![agent.id.into()];
    if let Some(kind) = &q.kind {
        vals.push(kind.clone().into());
        filters.push_str(&format!(" AND m.kind = ?{}", vals.len()));
    }
    if let Some(scope) = &q.scope {
        vals.push(scope.clone().into());
        filters.push_str(&format!(" AND m.scope = ?{}", vals.len()));
    }
    if let Some(cid) = q.channel_id {
        vals.push(cid.into());
        filters.push_str(&format!(" AND m.scope = 'channel' AND m.channel_id = ?{}", vals.len()));
    }

    let query = q.query.as_deref().map(str::trim).unwrap_or("");
    let has_query = !query.is_empty();

    if !has_query {
        let mut list_vals = vals.clone();
        list_vals.push(limit.into());
        let memories = state.db.with(|c| {
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM memories m JOIN agents a ON a.id = m.agent_id \
                 WHERE {} {} ORDER BY m.id DESC LIMIT ?{}",
                visibility_where(),
                filters,
                list_vals.len()
            );
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(list_vals.iter()), map_memory_row)
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())?;
            Ok(rows)
        })?;
        let items: Vec<serde_json::Value> = memories
            .iter()
            .map(|m| {
                let mut v = serde_json::to_value(m).unwrap_or(serde_json::Value::Null);
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("score".into(), serde_json::Value::Null);
                }
                v
            })
            .collect();
        return Ok(Json(serde_json::json!({
            "memories": items,
            "query": "",
            "vector_search": false,
        })));
    }

    // --- FTS5 recall (always on) ---
    let fts_expr = fts_match_expr(query)
        .ok_or_else(|| ApiError::bad_request("query must contain at least one search term"))?;
    let mut fts_vals = vals.clone();
    fts_vals.push(fts_expr.clone().into());
    let match_n = fts_vals.len();
    fts_vals.push(FTS_CANDIDATES.into());
    let fts_limit_n = fts_vals.len();
    let fts_rows: Vec<(Memory, f64)> = state.db.with(|c| {
        let sql = format!(
            "SELECT {MEMORY_COLS}, bm25(memories_fts) AS rank \
             FROM memories_fts JOIN memories m ON m.id = memories_fts.memory_id \
             JOIN agents a ON a.id = m.agent_id \
             WHERE memories_fts MATCH ?{match_n} AND {} {} \
             ORDER BY rank LIMIT ?{fts_limit_n}",
            visibility_where(),
            filters
        );
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(fts_vals.iter()), |r| {
                Ok((map_memory_row(r)?, r.get::<_, f64>(11)?))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())?;
        Ok(rows)
    })?;
    // bm25() is negative; lower (more negative) = better rank. Normalize the
    // inverted scores to [0, 1] over the candidate set.
    let max_q = fts_rows
        .iter()
        .map(|(_, rank)| (-rank).max(0.0))
        .fold(0.0f64, f64::max);
    let mut hits: std::collections::HashMap<i64, (Memory, Option<f64>, Option<f64>)> =
        std::collections::HashMap::new();
    for (memory, rank) in fts_rows {
        let q = (-rank).max(0.0);
        let norm = if max_q > 0.0 { q / max_q } else { 1.0 };
        hits.insert(memory.id, (memory, Some(norm), None));
    }

    // --- optional vector layer ---
    let vector_on = state.embeddings.enabled() && has_query;
    if vector_on {
        match state.embeddings.embed(vec![query.to_string()]).await {
            Ok(vectors) => {
                if let Some(qvec) = vectors.into_iter().next() {
                    let candidates: Vec<(Memory, Vec<u8>)> = state.db.with(|c| {
                        let sql = format!(
                            "SELECT {MEMORY_COLS}, e.vector FROM memories m \
                             JOIN agents a ON a.id = m.agent_id \
                             JOIN memory_embeddings e ON e.memory_id = m.id \
                             WHERE {} {}",
                            visibility_where(),
                            filters
                        );
                        let mut stmt = c.prepare(&sql)?;
                        let rows = stmt
                            .query_map(rusqlite::params_from_iter(vals.iter()), |r| {
                                Ok((map_memory_row(r)?, r.get::<_, Vec<u8>>(11)?))
                            })
                            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())?;
                        Ok(rows)
                    })?;
                    for (memory, blob) in candidates {
                        let vec = blob_to_vec(&blob);
                        if let Some(cos) = cosine(&qvec, &vec) {
                            let cos = cos.clamp(0.0, 1.0) as f64;
                            if cos > 0.0 {
                                hits.entry(memory.id)
                                    .and_modify(|e| e.2 = Some(e.2.map_or(cos, |c| c.max(cos))))
                                    .or_insert((memory, None, Some(cos)));
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "query embedding failed; fts-only ranking");
            }
        }
    }

    // --- combine: FTS5 + vector, simple weighted sum, then re-rank ---
    let mut ranked: Vec<(f64, Memory)> = hits
        .into_values()
        .filter_map(|(memory, fts, vec)| {
            let score = match (fts, vec) {
                (Some(f), Some(v)) => 0.5 * f + 0.5 * v,
                (Some(f), None) if vector_on => 0.5 * f,
                (Some(f), None) => f,
                (None, Some(v)) => 0.5 * v,
                (None, None) => return None,
            };
            Some((score, memory))
        })
        .filter(|(score, _)| *score > 0.0)
        .collect();
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap().then(b.1.id.cmp(&a.1.id)));
    ranked.truncate(limit as usize);

    let items: Vec<serde_json::Value> = ranked
        .into_iter()
        .map(|(score, memory)| {
            let mut v = serde_json::to_value(&memory).unwrap_or(serde_json::Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("score".into(), ((score * 10_000.0).round() / 10_000.0).into());
            }
            v
        })
        .collect();

    Ok(Json(serde_json::json!({
        "memories": items,
        "query": query,
        "vector_search": vector_on,
    })))
}

pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = extract_id(Ok(id))?;
    let affected = state.db.try_with(|c| {
        let tx = c.transaction()?;
        let row: Option<(String, i64, Option<i64>)> = tx
            .query_row(
                "SELECT scope, agent_id, channel_id FROM memories WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((scope, owner_id, channel_id)) = row else {
            return Err(ApiError::not_found("memory not found"));
        };
        if scope == "private" {
            if owner_id != agent.id {
                // 404, not 403: do not reveal other agents' private memories.
                return Err(ApiError::not_found("memory not found"));
            }
        } else {
            let member: i64 = tx.query_row(
                "SELECT COUNT(*) FROM channel_members WHERE channel_id = ?1 AND agent_id = ?2",
                rusqlite::params![channel_id, agent.id],
                |r| r.get(0),
            )?;
            if owner_id != agent.id && member == 0 {
                return Err(ApiError::forbidden_msg(
                    "only channel members can delete a channel memory",
                ));
            }
        }
        tx.execute("DELETE FROM memories_fts WHERE memory_id = ?1", [id])?;
        let affected = tx.execute("DELETE FROM memories WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(affected)
    })?;
    if affected == 0 {
        return Err(ApiError::not_found("memory not found"));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}
