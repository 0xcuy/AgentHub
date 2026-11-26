pub mod agents;
pub mod channels;
pub mod dms;
pub mod inbox;
pub mod mail;
pub mod runs;
pub mod ui;

use crate::error::ApiError;
use crate::models::Agent;
use crate::state::AppState;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(crate::ui::index))
        .route("/login", get(crate::ui::login_page))
        .route("/signup", get(crate::ui::signup_page))
        .route("/app", get(crate::ui::app_page))
        .route("/static/app.css", get(crate::ui::stylesheet))
        .route("/static/app.js", get(crate::ui::script))
        .route("/static/fonts/{file}", get(crate::ui::font))
        .route("/app.css", get(crate::ui::stylesheet))
        .route("/app.js", get(crate::ui::script))
        .route("/fonts/{file}", get(crate::ui::font))
        .route("/healthz", get(healthz))
        .route("/ui/signup", post(ui::signup))
        .route("/ui/login", post(ui::login))
        .route("/ui/logout", post(ui::logout))
        .route("/ui/api/me", get(ui::me))
        .route("/v1/agents", post(agents::create).get(agents::list))
        .route(
            "/v1/agents/me",
            get(agents::me).patch(agents::update_me),
        )
        .route("/v1/agents/me/token", post(agents::rotate_token))
        .route("/v1/channels", post(channels::create).get(channels::list))
        .route("/v1/channels/{id}", patch(channels::update))
        .route("/v1/channels/{id}/members", post(channels::add_members))
        .route("/v1/channels/{id}/members/{member_id}", delete(channels::remove_member))
        .route(
            "/v1/channels/{id}/messages",
            post(channels::send_message).get(channels::list_messages),
        )
        .route("/v1/dms", get(dms::list_partners))
        .route("/v1/dms/{agent_id}/messages", post(dms::send).get(dms::list))
        .route("/v1/inbox", get(inbox::list))
        .route("/v1/inbox/read", post(inbox::mark_read))
        .route("/v1/inbox/stream", get(inbox::stream))
        .route("/v1/mail", post(mail::send).get(mail::list))
        .route("/v1/mail/{id}/read", post(mail::mark_read))
        .route("/v1/runs", post(runs::create).get(runs::list))
        .route("/v1/runs/{id}", get(runs::get))
        .route("/v1/runs/{id}/complete", post(runs::complete))
        .layer(axum::middleware::from_fn(log_requests))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::ratelimit::middleware,
        ))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

pub async fn log_requests(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let start = std::time::Instant::now();
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let res = next.run(req).await;
    tracing::info!(
        method = %method,
        path = %path,
        status = res.status().as_u16(),
        latency_ms = start.elapsed().as_millis() as u64,
        "request"
    );
    res
}

pub fn require_agent(state: &AppState, headers: &HeaderMap) -> Result<Agent, ApiError> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(ApiError::unauthorized)?;
    agent_by_token(state, token)
}

/// Same as `require_agent` but also accepts the token as a `?token=` query
/// parameter — needed by browsers' EventSource, which cannot set headers.
/// Only for the SSE stream endpoint; every other route stays header-only.
pub fn require_agent_stream(
    state: &AppState,
    headers: &HeaderMap,
    query_token: Option<String>,
) -> Result<Agent, ApiError> {
    let header_token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    let token = header_token.or(query_token).ok_or_else(ApiError::unauthorized)?;
    agent_by_token(state, &token)
}

fn agent_by_token(state: &AppState, token: &str) -> Result<Agent, ApiError> {
    state.db.try_with(|c| {
        c.query_row(
            "SELECT id, handle, name, webhook_url, created_at FROM agents WHERE token = ?1",
            [token],
            |r| {
                Ok(Agent {
                    id: r.get(0)?,
                    handle: r.get(1)?,
                    name: r.get(2)?,
                    webhook_url: r.get(3)?,
                    created_at: r.get(4)?,
                })
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => ApiError::unauthorized(),
            other => other.into(),
        })
    })
}

pub fn is_unique_violation(e: &rusqlite::Error) -> bool {
    matches!(e.sqlite_error_code(), Some(rusqlite::ErrorCode::ConstraintViolation))
}

pub fn extract_id(id: Result<i64, axum::extract::rejection::PathRejection>) -> Result<i64, ApiError> {
    id.map_err(|_| ApiError::bad_request("invalid id in path"))
}
