use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::Error as SqlError;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, code: "bad_request", message: msg.into() }
    }
    pub fn unauthorized() -> Self {
        Self { status: StatusCode::UNAUTHORIZED, code: "unauthorized", message: "missing or invalid bearer token".into() }
    }
    pub fn forbidden() -> Self {
        Self { status: StatusCode::FORBIDDEN, code: "forbidden", message: "not allowed".into() }
    }
    pub fn forbidden_msg(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::FORBIDDEN, code: "forbidden", message: msg.into() }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::NOT_FOUND, code: "not_found", message: msg.into() }
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::CONFLICT, code: "conflict", message: msg.into() }
    }
    fn internal(msg: String) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, code: "internal", message: msg }
    }
}

impl From<SqlError> for ApiError {
    fn from(e: SqlError) -> Self {
        match &e {
            SqlError::QueryReturnedNoRows => ApiError::not_found("not found"),
            _ => {
                tracing::error!(error = %e, "database error");
                ApiError::internal("internal error".into())
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "error": { "code": self.code, "message": self.message }
        });
        (self.status, Json(body)).into_response()
    }
}
