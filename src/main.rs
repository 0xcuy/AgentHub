use aghub::{api, db};

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::net::SocketAddr;

const TOS: &str = include_str!("../static/tos.html");
const PRIVACY: &str = include_str!("../static/privacy.html");

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .json()
        .init();

    let db_path = std::env::var("AGHUB_DB").unwrap_or_else(|_| "aghub.db".to_string());
    let db = db::Db::open(&db_path).expect("open sqlite database");
    db.migrate().expect("run migrations");

    let state = aghub::state::AppState::new(
        db,
        reqwest::Client::builder()
            .build()
            .expect("build http client"),
    );

    // Durable webhook outbox: retries queued runs forever until delivered
    // or failed, with jittered backoff persisted in SQLite.
    tokio::spawn(aghub::outbox::run_dispatcher(state.clone()));

    let app = build_app(state);
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind port");
    tracing::info!(port, "aghub listening");
    axum::serve(
        listener,
        // ConnectInfo feeds the per-IP rate limiters with the socket address
        // when no X-Forwarded-For header is present.
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("server error");
}

/// The full application: the library router plus the trust pages, the data
/// export route and the session-cookie bridge. Kept as a function so tests
/// exercise exactly what `main` serves.
fn build_app(state: aghub::state::AppState) -> Router {
    let export = Router::new()
        .route("/v1/me/export", get(api::agents::export))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            aghub::ratelimit::middleware,
        ))
        .with_state(state.clone());
    let pages = Router::new()
        .route("/tos.html", get(tos_page))
        .route("/privacy.html", get(privacy_page));
    api::build_router(state.clone())
        .merge(export)
        .merge(pages)
        .layer(middleware::from_fn_with_state(state, session_cookie_auth))
}

/// The workspace UI no longer receives the agent's bearer token from
/// /ui/api/me — it authenticates with the HttpOnly session cookie instead.
/// For /v1 requests whose bearer token does not resolve to an agent, the
/// session cookie's agent is injected as the bearer, so handlers, the
/// per-token limiter and error paths see the same authenticated identity.
/// Requests with a valid agent bearer are never touched.
async fn session_cookie_auth(
    State(state): State<aghub::state::AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    if req.uri().path().starts_with("/v1/") {
        let bearer_authorises = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|token| !token.is_empty())
            .map(|token| {
                state
                    .db
                    .with(|c| {
                        c.query_row("SELECT 1 FROM agents WHERE token = ?1", [token], |_| Ok(()))
                    })
                    .is_ok()
            })
            .unwrap_or(false);
        if !bearer_authorises {
            if let Ok(Some(session)) = api::ui::current_session(&state, req.headers()) {
                let token = state.db.with(|c| {
                    c.query_row(
                        "SELECT token FROM agents WHERE id = ?1",
                        [session.agent.id],
                        |r| r.get::<_, String>(0),
                    )
                });
                if let Ok(token) = token {
                    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) {
                        req.headers_mut().insert(header::AUTHORIZATION, value);
                    }
                }
            }
        }
    }
    next.run(req).await
}

async fn tos_page() -> Response {
    trust_page(TOS)
}

async fn privacy_page() -> Response {
    trust_page(PRIVACY)
}

/// The trust pages are attached outside `api::build_router`, so they carry
/// their own copy of the security headers (same policy as
/// `api::security_headers`).
fn trust_page(html: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (
                header::STRICT_TRANSPORT_SECURITY,
                "max-age=31536000; includeSubDomains",
            ),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
                 img-src 'self' data:; font-src 'self'; connect-src 'self'; frame-ancestors 'none'",
            ),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "strict-origin-when-cross-origin"),
        ],
        html,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;
    use http_body_util::BodyExt;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tower::util::Oneshot;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn test_app(name: &str) -> Router {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let db = db::Db::open(&format!(
            "/tmp/aghub-main-{}-{}-{}.db",
            name,
            n,
            std::process::id()
        ))
        .expect("open test db");
        db.migrate().expect("migrate");
        let state = aghub::state::AppState::new(db, reqwest::Client::builder().build().unwrap());
        build_app(state)
    }

    #[tokio::test]
    async fn session_cookie_authorises_v1_without_bearer_token() {
        let app = test_app("cookieBridge");
        let req = Request::builder()
            .method("POST")
            .uri("/ui/signup")
            .header("Content-Type", "application/json")
            .body(Body::from(
                json!({
                    "email": "bridge@example.com",
                    "password": "password123",
                    "handle": "bridge-agent",
                    "name": "Bridge Agent",
                    "webhook_url": null,
                })
                .to_string(),
            ))
            .unwrap();
        let res = Oneshot::new(app.clone(), req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "signup must succeed");
        let cookie = res
            .headers()
            .get(header::SET_COOKIE)
            .expect("signup sets a session cookie")
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        // Cookie only — no Authorization header, exactly what the read-only
        // workspace UI sends now that /ui/api/me hides the bearer token.
        let req = Request::builder()
            .uri("/v1/channels")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        let res = Oneshot::new(app.clone(), req).await.unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "a valid session cookie must authorise /v1"
        );

        // Without either credential the same call is still 401.
        let req = Request::builder()
            .uri("/v1/channels")
            .body(Body::empty())
            .unwrap();
        let res = Oneshot::new(app, req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn trust_pages_are_served_with_security_headers() {
        let app = test_app("trustPages");
        for (path, needle) in [
            ("/tos.html", "Terms of Service"),
            ("/privacy.html", "Privacy Policy"),
        ] {
            let req = Request::builder().uri(path).body(Body::empty()).unwrap();
            let res = Oneshot::new(app.clone(), req).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{path} must be served");
            assert!(
                res.headers().contains_key(header::CONTENT_SECURITY_POLICY),
                "{path} must carry the security headers"
            );
            let body = res.into_body().collect().await.unwrap().to_bytes();
            let text = String::from_utf8_lossy(&body);
            assert!(text.contains(needle), "{path} must mention {needle}");
        }
    }

    async fn json_call(
        app: &Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let req = req
            .header("Content-Type", "application/json")
            .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
            .unwrap();
        let res = Oneshot::new(app.clone(), req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn signup(app: &Router, handle: &str) -> (i64, String) {
        let (status, v) = json_call(
            app,
            "POST",
            "/ui/signup",
            None,
            Some(json!({
                "email": format!("{handle}@example.com"),
                "password": "password123",
                "handle": handle,
                "name": handle,
                "webhook_url": null,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "signup {handle}: {v}");
        (
            v["agent"]["id"].as_i64().unwrap(),
            v["agent"]["token"].as_str().unwrap().to_string(),
        )
    }

    /// GET /v1/me/export carries the caller's own data and nothing from
    /// another workspace — and never the bearer token itself.
    #[tokio::test]
    async fn export_returns_only_the_callers_own_data() {
        let app = test_app("export");
        let (alice_id, alice) = signup(&app, "export-alice").await;
        let (_bob_id, bob) = signup(&app, "export-bob").await;

        let (status, v) = json_call(
            &app,
            "POST",
            "/v1/channels",
            Some(&alice),
            Some(json!({ "name": "alice-chan" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "alice channel: {v}");
        let chan = v["channel"]["id"].as_i64().unwrap();
        let (status, _) = json_call(
            &app,
            "POST",
            &format!("/v1/channels/{chan}/messages"),
            Some(&alice),
            Some(json!({ "body": "alice secret msg" })),
        )
        .await;
        assert!(status.is_success(), "alice message: {status}");
        let (status, _) = json_call(
            &app,
            "POST",
            "/v1/notes",
            Some(&alice),
            Some(json!({ "content": "alice private note" })),
        )
        .await;
        assert!(status.is_success(), "alice note: {status}");
        let (status, _) = json_call(
            &app,
            "POST",
            "/v1/memory",
            Some(&alice),
            Some(json!({ "content": "alice memory fact", "kind": "fakta" })),
        )
        .await;
        assert!(status.is_success(), "alice memory: {status}");

        let (status, v) = json_call(
            &app,
            "POST",
            "/v1/channels",
            Some(&bob),
            Some(json!({ "name": "bob-chan" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "bob channel: {v}");
        let bchan = v["channel"]["id"].as_i64().unwrap();
        let (status, _) = json_call(
            &app,
            "POST",
            &format!("/v1/channels/{bchan}/messages"),
            Some(&bob),
            Some(json!({ "body": "bob secret msg" })),
        )
        .await;
        assert!(status.is_success(), "bob message: {status}");

        let (status, _) = json_call(&app, "GET", "/v1/me/export", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "export requires auth");

        let (status, v) = json_call(&app, "GET", "/v1/me/export", Some(&alice), None).await;
        assert_eq!(status, StatusCode::OK, "export: {v}");
        assert_eq!(v["profile"]["id"].as_i64(), Some(alice_id));
        assert!(
            v["profile"].get("token").is_none(),
            "exports never carry the bearer token"
        );
        let raw = v.to_string();
        assert!(
            raw.contains("alice secret msg")
                && raw.contains("alice private note")
                && raw.contains("alice memory fact"),
            "the caller's own data must be present: {raw}"
        );
        assert!(
            !raw.contains("bob"),
            "another workspace's data must never appear: {raw}"
        );
        assert_eq!(v["workspaces"].as_array().unwrap().len(), 1);
        let names: Vec<&str> = v["channels"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert!(
            names.contains(&"alice-chan") && names.contains(&"general"),
            "{names:?}"
        );
        let messages = v["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            1,
            "only the caller's messages: {messages:?}"
        );
        assert_eq!(messages[0]["body"], "alice secret msg");
        assert_eq!(v["notes"].as_array().unwrap().len(), 1);
        assert_eq!(v["memory"].as_array().unwrap().len(), 1);

        let (status, v) = json_call(&app, "GET", "/v1/me/export", Some(&bob), None).await;
        assert_eq!(status, StatusCode::OK);
        let raw = v.to_string();
        assert!(
            raw.contains("bob secret msg") && !raw.contains("alice"),
            "{raw}"
        );
    }
}
