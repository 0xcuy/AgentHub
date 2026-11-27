//! Pro waitlist: the landing page's "join the waitlist" form posts an email
//! here. No auth — anyone may join. The response is identical for a new
//! email and an already-waitlisted one: same 200, same body, so the endpoint
//! never reveals whether an address is already on the list.

use crate::error::ApiError;
use crate::models::now_iso;
use crate::state::AppState;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::params;
use serde::Deserialize;
use serde_json::json;

const MAX_EMAIL_CHARS: usize = 254;
const MAX_LOCAL_CHARS: usize = 64;

#[derive(Deserialize)]
pub struct JoinWaitlist {
    pub email: String,
}

/// Shape check, deliberately conservative: one "@", non-empty local part and
/// domain, at least one dot in the domain with a non-empty final label, no
/// whitespace anywhere. Not a full RFC 5322 parser — it rejects garbage,
/// it does not prove deliverability.
fn valid_email(email: &str) -> bool {
    if email.chars().count() > MAX_EMAIL_CHARS {
        return false;
    }
    let (local, domain) = match email.split_once('@') {
        Some(parts) => parts,
        None => return false,
    };
    if local.is_empty() || local.chars().count() > MAX_LOCAL_CHARS || domain.is_empty() {
        return false;
    }
    // split_once splits at the first '@', so a stray second '@' lands in the
    // domain; whitespace is never legal anywhere in an address.
    if email.contains(char::is_whitespace) || domain.contains('@') || !domain.contains('.') {
        return false;
    }
    if email.contains("..") || email.starts_with('.') || email.ends_with('.') {
        return false;
    }
    domain
        .split('.')
        .all(|l| !l.is_empty() && !l.starts_with('-') && !l.ends_with('-'))
}

pub async fn join(
    State(state): State<AppState>,
    Json(input): Json<JoinWaitlist>,
) -> Result<impl IntoResponse, ApiError> {
    let email = input.email.trim().to_lowercase();
    if !valid_email(&email) {
        return Err(ApiError::bad_request("email is not valid"));
    }
    let now = now_iso();
    // Duplicate emails are a no-op, not an error: the same 200 goes out
    // either way so membership in the list is not observable.
    state.db.with(|c| {
        c.execute(
            "INSERT INTO waitlist (email, created_at) VALUES (?1, ?2)
             ON CONFLICT(email) DO NOTHING",
            params![email, now],
        )?;
        Ok(())
    })?;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use crate::api::build_router;
    use crate::state::AppState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tower::util::Oneshot;

    type RouterT = axum::Router;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn test_app() -> (RouterT, crate::db::Db) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let db = crate::db::Db::open(&format!(
            "file:aghub-waitlist-test-{n}-{}?mode=memory&cache=private",
            std::process::id()
        ))
        .or_else(|_| crate::db::Db::open(":memory:"))
        .expect("open test db");
        db.migrate().expect("migrate");
        let router = build_router(AppState::new(
            db.clone(),
            reqwest::Client::builder().build().unwrap(),
        ));
        (router, db)
    }

    async fn post(router: &RouterT, body: Value) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("POST")
            .uri("/v1/waitlist")
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
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

    #[tokio::test]
    async fn valid_email_is_stored() {
        let (router, db) = test_app();
        let (status, v) = post(&router, json!({ "email": "Founder@Example.com " })).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v, json!({ "ok": true }));

        // The email is stored trimmed and lowercased, with a timestamp.
        let row = db
            .with(|c| {
                c.query_row(
                    "SELECT email, created_at FROM waitlist",
                    [],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
            })
            .expect("waitlist row");
        assert_eq!(row.0, "founder@example.com");
        assert!(!row.1.is_empty());
    }

    #[tokio::test]
    async fn duplicate_email_returns_the_same_success() {
        let (router, _) = test_app();
        let first = post(&router, json!({ "email": "dupe@example.com" })).await;
        assert_eq!(first.0, StatusCode::OK);
        let second = post(&router, json!({ "email": "  DUPE@example.com " })).await;
        // Identical response body and status for the duplicate.
        assert_eq!(second.0, StatusCode::OK);
        assert_eq!(second.1, first.1);
    }

    #[tokio::test]
    async fn invalid_email_is_rejected() {
        let (router, _) = test_app();
        for bad in [
            "",
            "no-at-sign",
            "@nodomain.com",
            "user@",
            "user@nodot",
            "user@.com",
            "user@two@@ats.com",
            "a b@example.com",
        ] {
            let (status, v) = post(&router, json!({ "email": bad })).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "expected 400 for {bad:?}: {v}");
        }
    }
}
