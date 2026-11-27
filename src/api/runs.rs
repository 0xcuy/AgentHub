use super::{extract_id, require_agent};
use crate::error::ApiError;
use crate::models::{now_iso, AgentRun, CompleteRun, CreateRun, Page, page_params};
use crate::state::AppState;
use crate::triggers;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::params;

const RUN_COLS: &str =
    "id, agent_id, message_id, trigger_type, status, attempts, last_error, result, created_at, updated_at";

fn map_run_row(r: &rusqlite::Row) -> rusqlite::Result<AgentRun> {
    Ok(AgentRun {
        id: r.get(0)?,
        agent_id: r.get(1)?,
        message_id: r.get(2)?,
        trigger_type: r.get(3)?,
        status: r.get(4)?,
        attempts: r.get(5)?,
        last_error: r.get(6)?,
        result: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
    })
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateRun>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let run_id = state.db.try_with(|c| {
        let tx = c.transaction()?;
        // Visibility gate: a run may only reference a message the caller can
        // already read — a channel message in a channel they are a member of
        // (in their own workspace), or a DM they sent or received. Anything
        // else answers 404, so another workspace's message ids cannot be
        // probed or delivered to this caller's webhook.
        let visible: i64 = tx.query_row(
            "SELECT COUNT(*) FROM messages m
             WHERE m.id = ?1
               AND (
                 (m.channel_id IS NOT NULL AND EXISTS (
                    SELECT 1 FROM channel_members cm
                    JOIN channels ch ON ch.id = cm.channel_id
                    WHERE cm.channel_id = m.channel_id
                      AND cm.agent_id = ?2
                      AND ch.workspace_id = ?3))
                 OR (m.channel_id IS NULL AND m.sender_id = ?2)
                 OR (m.channel_id IS NULL AND m.recipient_id = ?2)
               )",
            params![input.message_id, agent.id, agent.workspace_id],
            |r| r.get(0),
        )?;
        if visible == 0 {
            return Err(ApiError::not_found("message not found"));
        }
        let run_id: i64 = tx.query_row(
            &format!(
                "INSERT INTO agent_runs (agent_id, message_id, trigger_type, created_at, updated_at)
                 VALUES (?1, ?2, 'manual', ?3, ?3) RETURNING id"
            ),
            params![agent.id, input.message_id, now_iso()],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(run_id)
    })?;
    let run = state
        .db
        .try_with(|c| {
            c.query_row(&format!("SELECT {RUN_COLS} FROM agent_runs WHERE id = ?1"), [run_id], map_run_row)
                .map_err(ApiError::from)
        })?;
    let db = state.db.clone();
    let http = state.http.clone();
    tokio::spawn(triggers::deliver_run(db, http, run_id));
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "run": run }))))
}

pub async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = extract_id(Ok(id))?;
    let run = state.db.try_with(|c| {
        c.query_row(
            &format!("SELECT {RUN_COLS} FROM agent_runs WHERE id = ?1 AND agent_id = ?2"),
            params![id, agent.id],
            map_run_row,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => ApiError::not_found("run not found"),
            other => other.into(),
        })
    })?;
    Ok(Json(serde_json::json!({ "run": run })))
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let (after_id, limit, _) = page_params(&page);
    let runs = state.db.with(|c| {
        let mut stmt = c.prepare(&format!(
            "SELECT {RUN_COLS} FROM agent_runs WHERE agent_id = ?1 AND (?2 = 0 OR id < ?2) ORDER BY id DESC LIMIT ?3"
        ))?;
        stmt.query_map(params![agent.id, after_id, limit], map_run_row)
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
    })?;
    Ok(Json(serde_json::json!({ "runs": runs })))
}

pub async fn complete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<CompleteRun>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = extract_id(Ok(id))?;
    if !matches!(input.status.as_str(), "succeeded" | "failed") {
        return Err(ApiError::bad_request("status must be 'succeeded' or 'failed'"));
    }
    let run = state.db.try_with(|c| {
        c.query_row(
            &format!(
                "UPDATE agent_runs SET status = ?1, result = ?2, updated_at = ?3
                 WHERE id = ?4 AND agent_id = ?5 AND status IN ('queued', 'delivered')
                 RETURNING {RUN_COLS}"
            ),
            params![input.status, input.result, now_iso(), id, agent.id],
            map_run_row,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                ApiError::not_found("run not found or already finished")
            }
            other => other.into(),
        })
    })?;
    Ok(Json(serde_json::json!({ "run": run })))
}
