//! Notebook: private per-owner notes. The `owner` column is the authenticated
//! agent id — both agent tokens and human users (whose `users` row links 1:1
//! to an agent) are covered by `require_agent`, so no extra auth exists. A
//! note owned by someone else is indistinguishable from a missing one: every
//! cross-owner access returns 404, never 403.

use super::{extract_id, require_agent};
use crate::error::ApiError;
use crate::models::{now_iso, page_params, CreateNote, Note, Page, UpdateNote};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::params;
use serde::Deserialize;
use serde_json::json;

const MAX_CONTENT_CHARS: usize = 100_000;
const MAX_TITLE_CHARS: usize = 100;

const NOTE_COLS: &str = "id, owner, title, content, created_at, updated_at";

/// The title given in the request, trimmed; empty falls back to the first
/// line of the content, trimmed and capped at 100 chars.
fn note_title(title: Option<&str>, content: &str) -> Option<String> {
    let candidate = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| content.lines().next().unwrap_or("").trim());
    if candidate.is_empty() {
        return None;
    }
    Some(candidate.chars().take(MAX_TITLE_CHARS).collect())
}

fn map_note_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    Ok(Note {
        id: r.get(0)?,
        owner: r.get(1)?,
        title: r.get(2)?,
        content: r.get(3)?,
        created_at: r.get(4)?,
        updated_at: r.get(5)?,
    })
}

fn select_note(c: &mut rusqlite::Connection, id: i64, owner: i64) -> Result<Note, ApiError> {
    match c.query_row(
        &format!("SELECT {NOTE_COLS} FROM notes WHERE id = ?1 AND owner = ?2"),
        params![id, owner],
        map_note_row,
    ) {
        Ok(note) => Ok(note),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(ApiError::not_found("note not found")),
        Err(e) => Err(ApiError::from(e)),
    }
}

#[derive(Deserialize)]
pub struct NotesQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateNote>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let content = input.content;
    if content.trim().is_empty() {
        return Err(ApiError::bad_request("content must not be empty"));
    }
    if content.chars().count() > MAX_CONTENT_CHARS {
        return Err(ApiError::bad_request("content is too long"));
    }
    let title = note_title(input.title.as_deref(), &content);
    let now = now_iso();
    let note = state.db.try_with(|c| {
        c.execute(
            "INSERT INTO notes (owner, title, content, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
            params![agent.id, title, content, now],
        )?;
        Ok(Note {
            id: c.last_insert_rowid(),
            owner: agent.id,
            title,
            content,
            created_at: now.clone(),
            updated_at: now,
        })
    })?;
    Ok((StatusCode::CREATED, Json(json!({ "note": note }))))
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<NotesQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    // page_params clamps the limit exactly like every other list endpoint.
    let (_, limit, _) = page_params(&Page { after_id: None, limit: q.limit, unread: None });
    let offset = q.offset.unwrap_or(0).max(0);
    let notes = state.db.with(|c| {
        let mut stmt = c.prepare(&format!(
            "SELECT {NOTE_COLS} FROM notes WHERE owner = ?1 ORDER BY updated_at DESC, id DESC LIMIT ?2 OFFSET ?3"
        ))?;
        let rows = stmt
            .query_map(params![agent.id, limit, offset], map_note_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;
    Ok(Json(json!({ "notes": notes })))
}

pub async fn get_note(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = extract_id(Ok(id))?;
    let note = state.db.try_with(|c| select_note(c, id, agent.id))?;
    Ok(Json(json!({ "note": note })))
}

pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(input): Json<UpdateNote>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = extract_id(Ok(id))?;
    if let Some(content) = input.content.as_deref() {
        if content.trim().is_empty() {
            return Err(ApiError::bad_request("content must not be empty"));
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(ApiError::bad_request("content is too long"));
        }
    }
    // Only an explicitly provided title changes it; deriving from the first
    // line is a create-time rule, so a content-only PATCH keeps the old title.
    let title = input
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| t.chars().take(MAX_TITLE_CHARS).collect::<String>());
    let note = state.db.try_with(|c| {
        if input.content.is_none() && input.title.is_none() {
            // No fields to change: return the note as-is without touching it.
            return select_note(c, id, agent.id);
        }
        let now = now_iso();
        let changed = c.execute(
            "UPDATE notes SET title = COALESCE(?1, title), content = COALESCE(?2, content), updated_at = ?3 WHERE id = ?4 AND owner = ?5",
            params![title, input.content, now, id, agent.id],
        )?;
        if changed == 0 {
            return Err(ApiError::not_found("note not found"));
        }
        select_note(c, id, agent.id)
    })?;
    Ok(Json(json!({ "note": note })))
}

pub async fn remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let agent = require_agent(&state, &headers)?;
    let id = extract_id(Ok(id))?;
    let deleted = state.db.with(|c| {
        c.execute(
            "DELETE FROM notes WHERE id = ?1 AND owner = ?2",
            params![id, agent.id],
        )
    })?;
    if deleted == 0 {
        return Err(ApiError::not_found("note not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::build_router;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tower::util::Oneshot;

    type RouterT = axum::Router;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn test_app() -> RouterT {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let db = crate::db::Db::open(&format!(
            "file:aghub-notes-test-{n}-{}?mode=memory&cache=private",
            std::process::id()
        ))
        .or_else(|_| crate::db::Db::open(":memory:"))
        .expect("open test db");
        db.migrate().expect("migrate");
        // Workspace-era bootstrap: POST /v1/agents requires the caller to be
        // the workspace admin (creating agents mints bearer tokens), so seed
        // the bootstrap agent AND a human user row that owns workspace 1 —
        // the same linkage the /ui/signup flow produces.
        db.with(|c| {
            c.execute(
                "INSERT INTO agents (handle, name, token, webhook_url, created_at, workspace_id) VALUES ('bootstrap', 'Bootstrap', 'test-bootstrap-token', NULL, ?1, 1)",
                [now_iso()],
            )?;
            c.execute(
                "INSERT INTO users (email, password_hash, agent_id, created_at)
                 SELECT 'bootstrap@test.local', 'x', id, ?1 FROM agents WHERE handle = 'bootstrap'",
                [now_iso()],
            )?;
            c.execute(
                "UPDATE workspaces SET owner_user_id = (SELECT id FROM users WHERE email = 'bootstrap@test.local') WHERE id = 1",
                [],
            )?;
            Ok(())
        })
        .expect("seed bootstrap workspace admin");
        build_router(crate::state::AppState::new(
            db,
            reqwest::Client::builder().build().unwrap(),
        ))
    }

    async fn call(
        router: &RouterT,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let req = req
            .header("Content-Type", "application/json")
            .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
            .unwrap();
        let res = Oneshot::new(router.clone(), req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, v)
    }

    async fn register(router: &RouterT, handle: &str) -> String {
        let (status, v) = call(
            router,
            "POST",
            "/v1/agents",
            Some("test-bootstrap-token"),
            Some(json!({ "name": handle, "handle": handle })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
        v["token"].as_str().unwrap().to_string()
    }

    async fn create_note(router: &RouterT, token: &str, content: &str, title: Option<&str>) -> Value {
        let (status, v) = call(
            router,
            "POST",
            "/v1/notes",
            Some(token),
            Some(json!({
                "content": content,
                "title": title,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "create failed: {v}");
        v["note"].clone()
    }

    #[tokio::test]
    async fn create_get_roundtrip() {
        let router = test_app();
        let token = register(&router, "notebook-alice").await;
        let note = create_note(&router, &token, "remember the milk", Some("Groceries")).await;
        assert_eq!(note["title"], "Groceries");
        assert_eq!(note["content"], "remember the milk");

        let (status, v) = call(&router, "GET", "/v1/notes/1", Some(&token), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["note"]["title"], "Groceries");
        assert_eq!(v["note"]["content"], "remember the milk");
    }

    #[tokio::test]
    async fn title_derived_from_first_line_when_omitted() {
        let router = test_app();
        let token = register(&router, "notebook-derived").await;
        let note = create_note(
            &router,
            &token,
            "First line is the title\nand this is the body\n",
            None,
        )
        .await;
        assert_eq!(note["title"], "First line is the title");

        let long_first = "x".repeat(140);
        let capped = create_note(&router, &token, &format!("{long_first}\nrest"), None).await;
        assert_eq!(capped["title"].as_str().unwrap().chars().count(), 100);
    }

    #[tokio::test]
    async fn empty_content_is_rejected() {
        let router = test_app();
        let token = register(&router, "notebook-empty").await;
        let (status, v) = call(
            &router,
            "POST",
            "/v1/notes",
            Some(&token),
            Some(json!({ "content": "   " })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");

        let note = create_note(&router, &token, "seed", None).await;
        let id = note["id"].as_i64().unwrap();
        let (status, v) = call(
            &router,
            "PATCH",
            &format!("/v1/notes/{id}"),
            Some(&token),
            Some(json!({ "content": "" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    }

    #[tokio::test]
    async fn list_returns_only_own_notes_newest_first() {
        let router = test_app();
        let alice = register(&router, "notebook-lister-a").await;
        let bob = register(&router, "notebook-lister-b").await;
        for text in ["first", "second", "third"] {
            create_note(&router, &alice, text, None).await;
        }
        create_note(&router, &bob, "bob private", None).await;

        let (status, v) = call(&router, "GET", "/v1/notes", Some(&alice), None).await;
        assert_eq!(status, StatusCode::OK);
        let notes = v["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 3);
        let contents: Vec<_> = notes.iter().map(|n| n["content"].as_str().unwrap()).collect();
        assert_eq!(contents, ["third", "second", "first"]);

        let (status, v) = call(&router, "GET", "/v1/notes?limit=2&offset=1", Some(&alice), None).await;
        assert_eq!(status, StatusCode::OK);
        let notes = v["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0]["content"], "second");
        assert_eq!(notes[1]["content"], "first");
    }

    #[tokio::test]
    async fn owner_a_never_touches_owner_b_note() {
        let router = test_app();
        let alice = register(&router, "notebook-iso-a").await;
        let bob = register(&router, "notebook-iso-b").await;
        let note = create_note(&router, &alice, "alice secret", Some("Private")).await;
        let id = note["id"].as_i64().unwrap();

        let (status, _) = call(&router, "GET", &format!("/v1/notes/{id}"), Some(&bob), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "GET by stranger must 404");

        let (status, _) = call(
            &router,
            "PATCH",
            &format!("/v1/notes/{id}"),
            Some(&bob),
            Some(json!({ "content": "hijacked" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "PATCH by stranger must 404");

        let (status, _) = call(&router, "DELETE", &format!("/v1/notes/{id}"), Some(&bob), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "DELETE by stranger must 404");

        // None of the stranger's calls leaked the note or changed it.
        let (status, v) = call(&router, "GET", &format!("/v1/notes/{id}"), Some(&alice), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["note"]["content"], "alice secret");
        let (_, v) = call(&router, "GET", "/v1/notes", Some(&bob), None).await;
        assert!(v["notes"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_token_is_unauthorized_everywhere() {
        let router = test_app();
        let alice = register(&router, "notebook-authz").await;
        let note = create_note(&router, &alice, "own note", None).await;
        let id = note["id"].as_i64().unwrap();

        let (status, _) = call(&router, "GET", "/v1/notes", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = call(&router, "POST", "/v1/notes", None, Some(json!({ "content": "x" }))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = call(&router, "GET", &format!("/v1/notes/{id}"), None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = call(
            &router,
            "PATCH",
            &format!("/v1/notes/{id}"),
            None,
            Some(json!({ "content": "x" })),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = call(&router, "DELETE", &format!("/v1/notes/{id}"), None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn patch_updates_content_title_and_refreshes_updated_at() {
        let router = test_app();
        let token = register(&router, "notebook-patch").await;
        let note = create_note(&router, &token, "before", Some("Old title")).await;
        let id = note["id"].as_i64().unwrap();

        let (status, v) = call(
            &router,
            "PATCH",
            &format!("/v1/notes/{id}"),
            Some(&token),
            Some(json!({ "title": "New title", "content": "after" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["note"]["title"], "New title");
        assert_eq!(v["note"]["content"], "after");
        assert_eq!(v["note"]["created_at"], note["created_at"]);
        assert!(v["note"]["updated_at"].as_str().unwrap() >= note["updated_at"].as_str().unwrap());

        // A PATCH with no fields changes nothing (and still 404s for strangers).
        let (status, v) = call(
            &router,
            "PATCH",
            &format!("/v1/notes/{id}"),
            Some(&token),
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["note"]["updated_at"], v["note"]["updated_at"]);
    }

    #[tokio::test]
    async fn delete_removes_own_note_and_returns_204() {
        let router = test_app();
        let token = register(&router, "notebook-delete").await;
        let note = create_note(&router, &token, "doomed", None).await;
        let id = note["id"].as_i64().unwrap();

        let (status, v) = call(&router, "DELETE", &format!("/v1/notes/{id}"), Some(&token), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{v}");
        assert_eq!(v, Value::Null);

        let (status, _) = call(&router, "GET", &format!("/v1/notes/{id}"), Some(&token), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = call(&router, "DELETE", "/v1/notes/999999", Some(&token), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
