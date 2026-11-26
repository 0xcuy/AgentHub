use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tower::util::Oneshot;

static COUNTER: AtomicU32 = AtomicU32::new(0);

type RouterT = axum::Router;

fn test_app(name: &str) -> RouterT {
    test_app_with_state(name).0
}

/// Router plus its AppState, for tests that need the DB or the outbox.
fn test_app_with_state(name: &str) -> (RouterT, aghub::state::AppState) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let db = aghub::db::Db::open(&format!(
        "/tmp/aghub-test-{}-{}-{}.db",
        name,
        n,
        std::process::id()
    ))
    .expect("open test db");
    db.migrate().expect("migrate");
    let state = aghub::state::AppState::new(
        db,
        reqwest::Client::builder().build().unwrap(),
    );
    // Run the durable outbox dispatcher like production main() does.
    tokio::spawn(aghub::outbox::run_dispatcher(state.clone()));
    (aghub::api::build_router(state.clone()), state)
}

async fn call(
    router: &RouterT,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, v) = call_raw(
        router,
        method,
        uri,
        token,
        body,
        &[],
    )
    .await;
    (status, v)
}

async fn call_raw(
    router: &RouterT,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
    extra_headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
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
        match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => Value::String(String::from_utf8_lossy(&bytes).into_owned()),
        }
    };
    (status, v)
}

async fn register(router: &RouterT, handle: &str, webhook: Option<&str>) -> (i64, String) {
    let (status, v) = call(
        router,
        "POST",
        "/v1/agents",
        None,
        Some(json!({ "name": handle, "handle": handle, "webhook_url": webhook })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
    (v["agent"]["id"].as_i64().unwrap(), v["token"].as_str().unwrap().to_string())
}

/// Start a local axum webhook receiver; returns (base_url, received payloads).
async fn start_webhook_server() -> (String, Arc<Mutex<Vec<Value>>>) {
    let received: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let recv = received.clone();
    let app = axum::Router::new().route(
        "/run",
        post(move |body: String| {
            let recv = recv.clone();
            async move {
                if let Ok(v) = serde_json::from_str::<Value>(&body) {
                    recv.lock().unwrap().push(v);
                }
                axum::http::StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), received)
}

// --- agents ---

#[tokio::test]
async fn register_and_auth() {
    let router = test_app("auth");
    let (_id, token) = register(&router, "alpha", None).await;
    let (status, v) = call(&router, "GET", "/v1/agents/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["agent"]["handle"], "alpha");
    let (status, v) = call(&router, "GET", "/v1/agents/me", Some("nope"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"]["code"], "unauthorized");
    let (status, _) = call(
        &router,
        "POST",
        "/v1/agents",
        None,
        Some(json!({ "name": "x", "handle": "alpha" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(
        &router,
        "POST",
        "/v1/agents",
        None,
        Some(json!({ "name": "x", "handle": "Bad Handle!" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &router,
        "POST",
        "/v1/agents",
        None,
        Some(json!({ "name": "x", "handle": "beta", "evil": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call(&router, "GET", "/healthz", None, None).await;
    assert_eq!(status, StatusCode::OK);
}

// --- channels ---

#[tokio::test]
async fn channel_flow() {
    let router = test_app("chan");
    let (a1, t1) = register(&router, "alpha", None).await;
    let (a2, t2) = register(&router, "beta", None).await;

    let (status, v) = call(
        &router,
        "POST",
        "/v1/channels",
        Some(&t1),
        Some(json!({ "name": "general", "member_ids": [a1] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let chan_id = v["channel"]["id"].as_i64().unwrap();

    let (status, _) = call(
        &router,
        "GET",
        &format!("/v1/channels/{chan_id}/messages"),
        Some(&t2),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan_id}/messages"),
        Some(&t2),
        Some(json!({ "body": "sneak" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan_id}/members"),
        Some(&t1),
        Some(json!({ "agent_ids": [a2] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, v1) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan_id}/messages"),
        Some(&t1),
        Some(json!({ "body": "hello" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let msg_id = v1["message"]["id"].as_i64().unwrap();

    // idempotency: same Idempotency-Key returns the same message
    let (_, v2) = call_raw(
        &router,
        "POST",
        &format!("/v1/channels/{chan_id}/messages"),
        Some(&t1),
        Some(json!({ "body": "hello again" })),
        &[("Idempotency-Key", "k1")],
    )
    .await;
    let idem_id = v2["message"]["id"].as_i64().unwrap();
    let (_, v3) = call_raw(
        &router,
        "POST",
        &format!("/v1/channels/{chan_id}/messages"),
        Some(&t1),
        Some(json!({ "body": "hello again" })),
        &[("Idempotency-Key", "k1")],
    )
    .await;
    assert_eq!(idem_id, v3["message"]["id"].as_i64().unwrap(), "idempotency must dedupe");

    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan_id}/messages"),
        Some(&t1),
        Some(json!({ "body": "  " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // pagination over channel messages
    for i in 0..4 {
        call(
            &router,
            "POST",
            &format!("/v1/channels/{chan_id}/messages"),
            Some(&t2),
            Some(json!({ "body": format!("c{i}") })),
        )
        .await;
    }
    let (status, v) = call(
        &router,
        "GET",
        &format!("/v1/channels/{chan_id}/messages?limit=3"),
        Some(&t2),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["messages"].as_array().unwrap().len(), 3);

    // beta's inbox has alpha's message, unread until marked
    let (status, v) = call(&router, "GET", "/v1/inbox?unread=true", Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK);
    let items = v["items"].as_array().unwrap();
    assert!(items.iter().any(|i| i["message"]["id"].as_i64() == Some(msg_id)));
    let (status, _) = call(&router, "POST", "/v1/inbox/read", Some(&t2), Some(json!({ "all": true }))).await;
    assert_eq!(status, StatusCode::OK);
    let (_, v) = call(&router, "GET", "/v1/inbox?unread=true", Some(&t2), None).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 0);
}

// --- DMs ---

#[tokio::test]
async fn dm_flow_and_pagination() {
    let router = test_app("dm");
    let (a1, t1) = register(&router, "alpha", None).await;
    let (a2, t2) = register(&router, "beta", None).await;
    for i in 0..5 {
        let (status, _) = call(
            &router,
            "POST",
            &format!("/v1/dms/{a2}/messages"),
            Some(&t1),
            Some(json!({ "body": format!("msg {i}") })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, v) = call(
        &router,
        "GET",
        &format!("/v1/dms/{a1}/messages?limit=2"),
        Some(&t2),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = v["messages"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    // messages are newest-first; the oldest of the page is the cursor
    let last_id = items.iter().map(|m| m["id"].as_i64().unwrap()).min().unwrap();
    let (_, v) = call(
        &router,
        "GET",
        &format!("/v1/dms/{a1}/messages?limit=100&after_id={last_id}"),
        Some(&t2),
        None,
    )
    .await;
    assert_eq!(v["messages"].as_array().unwrap().len(), 3);
    let (status, v) = call(&router, "GET", "/v1/dms", Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["partners"][0]["handle"], "alpha");
    let (status, _) = call(
        &router,
        "POST",
        "/v1/dms/9999/messages",
        Some(&t1),
        Some(json!({ "body": "hi" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/dms/{a1}/messages"),
        Some(&t1),
        Some(json!({ "body": "self" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// --- mail ---

#[tokio::test]
async fn mail_flow() {
    let router = test_app("mail");
    let (_a1, t1) = register(&router, "alpha", None).await;
    let (_a2, t2) = register(&router, "beta", None).await;
    let (status, _) = call(
        &router,
        "POST",
        "/v1/mail",
        Some(&t1),
        Some(json!({ "to_handle": "beta", "subject": "Hi", "body": "async hello" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, v) = call(&router, "GET", "/v1/mail", Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK);
    let mail_id = v["mail"][0]["id"].as_i64().unwrap();
    assert_eq!(v["mail"][0]["subject"], "Hi");
    let (_, v) = call(&router, "GET", "/v1/mail?unread=true", Some(&t2), None).await;
    assert_eq!(v["mail"].as_array().unwrap().len(), 1);
    let (status, _) = call(&router, "POST", &format!("/v1/mail/{mail_id}/read"), Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&router, "POST", &format!("/v1/mail/{mail_id}/read"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &router,
        "POST",
        "/v1/mail",
        Some(&t1),
        Some(json!({ "to_handle": "ghost", "subject": "x", "body": "y" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --- runs ---

#[tokio::test]
async fn dm_triggers_webhook_run() {
    let (base, received) = start_webhook_server().await;
    let router = test_app("run");
    let (a1, t1) = register(&router, "alpha", None).await;
    let (a2, t2) = register(&router, "beta", None).await;

    // no webhook yet -> no run for beta
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/dms/{a2}/messages"),
        Some(&t1),
        Some(json!({ "body": "no webhook yet" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, v) = call(&router, "GET", "/v1/runs", Some(&t2), None).await;
    assert_eq!(v["runs"].as_array().unwrap().len(), 0);

    // set webhook
    let (status, _) = call(
        &router,
        "PATCH",
        "/v1/agents/me",
        Some(&t2),
        Some(json!({ "webhook_url": format!("{base}/run") })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // a DM now triggers a run delivered to beta's webhook
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/dms/{a2}/messages"),
        Some(&t1),
        Some(json!({ "body": "trigger me" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let msg_id = v["message"]["id"].as_i64().unwrap();

    let (status, v) = call(&router, "GET", "/v1/runs", Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK);
    let runs = v["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["trigger_type"], "message");
    let run_id = runs[0]["id"].as_i64().unwrap();

    // wait for delivery
    let mut delivered = false;
    for _ in 0..40 {
        if let Some(p) = received.lock().unwrap().first() {
            assert_eq!(p["run_id"], run_id);
            assert_eq!(p["message"]["body"], "trigger me");
            assert_eq!(p["agent_handle"], "beta");
            delivered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(delivered, "webhook never received the run payload");

    // run marked delivered after successful webhook
    let mut status_seen = String::new();
    for _ in 0..40 {
        let (_, v) = call(&router, "GET", &format!("/v1/runs/{run_id}"), Some(&t2), None).await;
        status_seen = v["run"]["status"].as_str().unwrap().to_string();
        if status_seen == "delivered" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert_eq!(status_seen, "delivered");

    // agent completes the run
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/runs/{run_id}/complete"),
        Some(&t2),
        Some(json!({ "status": "succeeded", "result": "handled" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/runs/{run_id}/complete"),
        Some(&t2),
        Some(json!({ "status": "succeeded" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cannot complete twice");
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/runs/{run_id}/complete"),
        Some(&t1),
        Some(json!({ "status": "succeeded" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // manual trigger endpoint
    let (status, v) = call(&router, "POST", "/v1/runs", Some(&t2), Some(json!({ "message_id": msg_id }))).await;
    assert_eq!(status, StatusCode::CREATED);
    let manual_run = v["run"]["id"].as_i64().unwrap();
    assert_eq!(v["run"]["trigger_type"], "manual");
    // alpha cannot see beta's runs
    let (status, _) = call(&router, "GET", &format!("/v1/runs/{manual_run}"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // missing message
    let (status, _) = call(&router, "POST", "/v1/runs", Some(&t2), Some(json!({ "message_id": 9999 }))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --- Phase 2: token rotation ---

#[tokio::test]
async fn token_rotation_revokes_old_token() {
    let router = test_app("rotate");
    let (_id, token) = register(&router, "alpha", None).await;

    let (status, v) = call(&router, "POST", "/v1/agents/me/token", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "rotate failed: {v}");
    let new_token = v["token"].as_str().unwrap().to_string();
    assert_ne!(new_token, token, "rotation must mint a fresh token");

    // old token is revoked immediately, new one works
    let (status, _) = call(&router, "GET", "/v1/agents/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "old token must be rejected");
    let (status, v2) = call(&router, "GET", "/v1/agents/me", Some(&new_token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v2["agent"]["handle"], "alpha");
}

// --- Phase 2: rate limiting ---

#[tokio::test]
async fn rate_limit_per_token() {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let db = aghub::db::Db::open(&format!("/tmp/aghub-test-rl-{n}-{}.db", std::process::id()))
        .expect("open test db");
    db.migrate().expect("migrate");
    let mut state = aghub::state::AppState::new(db, reqwest::Client::builder().build().unwrap());
    state.limiter = aghub::ratelimit::RateLimiter::new(3, std::time::Duration::from_secs(60));
    let router = aghub::api::build_router(state);
    let (_id, t1) = register(&router, "alpha", None).await;
    let (_id2, t2) = register(&router, "beta", None).await;

    let mut saw_429 = false;
    for i in 0..5 {
        let (status, v) = call(&router, "GET", "/v1/inbox", Some(&t1), None).await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            saw_429 = true;
            assert_eq!(v["error"]["code"], "rate_limited");
            assert!(i >= 3, "limit is 3; 429 came at request {}", i + 1);
            break;
        }
        assert_eq!(status, StatusCode::OK);
    }
    assert!(saw_429, "expected a 429 after the limit");

    // a different token has its own budget
    let (status, _) = call(&router, "GET", "/v1/inbox", Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK, "another token must not inherit the limit");
}

// --- Phase 2: per-channel permissions ---

#[tokio::test]
async fn channel_permissions() {
    let router = test_app("perm");
    let (a1, t1) = register(&router, "alpha", None).await;
    let (a2, t2) = register(&router, "beta", None).await;
    let (a3, t3) = register(&router, "gamma", None).await;

    let (status, v) = call(
        &router,
        "POST",
        "/v1/channels",
        Some(&t1),
        Some(json!({ "name": "ops", "member_ids": [a1, a2], "post_policy": "admins" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let chan = v["channel"]["id"].as_i64().unwrap();
    assert_eq!(v["channel"]["post_policy"], "admins");

    // beta is a plain member: may read, may not post under admins policy,
    // and may not add members or change the channel
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t2),
        Some(json!({ "body": "member blocked" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "member must not post in admins-only channel");
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/members"),
        Some(&t2),
        Some(json!({ "agent_ids": [a3] })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "member must not add members");
    let (status, _) = call(
        &router,
        "PATCH",
        &format!("/v1/channels/{chan}"),
        Some(&t2),
        Some(json!({ "topic": "hijack" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "member must not update the channel");

    // admin flips the policy; now members may post
    let (status, _) = call(
        &router,
        "PATCH",
        &format!("/v1/channels/{chan}"),
        Some(&t1),
        Some(json!({ "post_policy": "members", "topic": "ops chat" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t2),
        Some(json!({ "body": "member ok now" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // invalid policy is rejected
    let (status, _) = call(
        &router,
        "PATCH",
        &format!("/v1/channels/{chan}"),
        Some(&t1),
        Some(json!({ "post_policy": "everyone" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // cannot remove the last admin; non-admin cannot remove anyone
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/v1/channels/{chan}/members/{a1}"),
        Some(&t1),
        None,
    )
    .await;
    // removing the only admin is refused as a bad request, not a 403
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/v1/channels/{chan}/members/{a2}"),
        Some(&t2),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // admin removes beta, who then loses access
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/v1/channels/{chan}/members/{a2}"),
        Some(&t1),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &router,
        "GET",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t2),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "removed member must lose access");
}

// --- Phase 2: durable outbox with jittered retries ---

#[tokio::test]
async fn outbox_retries_then_fails_and_survives_restart() {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = format!("/tmp/aghub-test-outbox-{n}-{}.db", std::process::id());
    let db = aghub::db::Db::open(&path).expect("open test db");
    db.migrate().expect("migrate");
    let http = reqwest::Client::builder().build().unwrap();
    let state = aghub::state::AppState::new(db, http.clone());
    let router = aghub::api::build_router(state.clone()); // no dispatcher: we drive it by hand
    let (_a1, t1) = register(&router, "alpha", None).await;
    // webhook pointing at a closed port -> every attempt fails
    let (a2, _t2) = register(&router, "beta", Some("http://127.0.0.1:9/run")).await;

    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/dms/{a2}/messages"),
        Some(&t1),
        Some(json!({ "body": "will fail" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // attempt 1: fails, run stays queued with a future next_attempt_at
    let attempted = aghub::outbox::process_due(&state.db, &http).await;
    assert_eq!(attempted.len(), 1, "one due run should be attempted");
    let (attempts, run_status): (i64, String) = state
        .db
        .with(|c| {
            c.query_row(
                "SELECT attempts, status FROM agent_runs LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap();
    assert_eq!(attempts, 1);
    assert_eq!(run_status, "queued", "failed attempt must stay queued for retry");

    // a second pass must NOT retry before next_attempt_at (jittered backoff)
    let attempted = aghub::outbox::process_due(&state.db, &http).await;
    assert!(attempted.is_empty(), "backoff must gate the retry");

    // simulate the wait by rewinding next_attempt_at, retry until failed
    for expected in [2, 3] {
        state
            .db
            .with(|c| c.execute("UPDATE agent_runs SET next_attempt_at = '2000-01-01'", []))
            .unwrap();
        let attempted = aghub::outbox::process_due(&state.db, &http).await;
        assert_eq!(attempted.len(), 1);
        let (attempts, run_status): (i64, String) = state
            .db
            .with(|c| {
                c.query_row(
                    "SELECT attempts, status FROM agent_runs LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            })
            .unwrap();
        assert_eq!(attempts, expected);
        if expected == 3 {
            assert_eq!(run_status, "failed", "3 failed attempts must mark the run failed");
        }
    }
    let last_error: String = state
        .db
        .with(|c| c.query_row("SELECT last_error FROM agent_runs LIMIT 1", [], |r| r.get(0)))
        .unwrap();
    assert!(last_error.contains("HTTP") || !last_error.is_empty());

    // durability: a "restarted" process opening the same DB file still sees
    // the failed run (the queue lived in SQLite, not in memory)
    let db2 = aghub::db::Db::open(&path).expect("reopen test db");
    let status: String = db2
        .with(|c| c.query_row("SELECT status FROM agent_runs LIMIT 1", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(status, "failed");
}

// --- Phase 2: SSE live inbox ---

#[tokio::test]
async fn sse_stream_pushes_inbox_events_only_for_self() {
    let (router, _state) = test_app_with_state("sse");
    let (a1, t1) = register(&router, "alpha", None).await;
    let (a2, t2) = register(&router, "beta", None).await;

    // subscribe beta to the stream and read frames while alpha sends a DM
    let req = Request::builder()
        .method("GET")
        .uri("/v1/inbox/stream")
        .header("Authorization", format!("Bearer {t2}"))
        .body(Body::empty())
        .unwrap();
    let res = Oneshot::new(router.clone(), req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "text/event-stream",
        "must be an SSE stream"
    );
    let mut body = res.into_body();

    // alpha DMs beta -> beta's stream must push the message; alpha would not see it
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/dms/{a2}/messages"),
        Some(&t1),
        Some(json!({ "body": "live one" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let mut got_event = false;
    for _ in 0..40 {
        match tokio::time::timeout(std::time::Duration::from_millis(250), body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(chunk) = frame.into_data() {
                    let s = String::from_utf8_lossy(&chunk);
                    if s.contains("\"type\":\"inbox_message\"") && s.contains("live one") {
                        got_event = true;
                        break;
                    }
                }
            }
            _ => continue,
        }
    }
    assert!(got_event, "SSE stream must push the DM to the recipient");

    // unauthenticated subscription is rejected
    let (status, _) = call(&router, "GET", "/v1/inbox/stream", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let _ = a1;
}

// --- Security fix-now: V1 security headers ---

#[tokio::test]
async fn security_headers_on_all_responses() {
    let router = test_app("secHeaders");
    for uri in ["/", "/login", "/signup", "/app", "/v1/channels", "/static/app.css", "/healthz"] {
        let req = Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap();
        let res = Oneshot::new(router.clone(), req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "uri: {uri}");
        let h = res.headers();
        assert_eq!(
            h.get("strict-transport-security").unwrap(),
            "max-age=31536000; includeSubDomains",
            "uri: {uri}"
        );
        let csp = h.get("content-security-policy").unwrap().to_str().unwrap();
        for directive in [
            "default-src 'self'",
            "script-src 'self' 'unsafe-inline'",
            "style-src 'self' 'unsafe-inline'",
            "img-src 'self' data:",
            "font-src 'self'",
            "connect-src 'self'",
            "frame-ancestors 'none'",
        ] {
            assert!(csp.contains(directive), "uri: {uri}, csp missing: {directive}");
        }
        assert_eq!(h.get("x-frame-options").unwrap(), "DENY", "uri: {uri}");
        assert_eq!(h.get("x-content-type-options").unwrap(), "nosniff", "uri: {uri}");
        assert_eq!(
            h.get("referrer-policy").unwrap(),
            "strict-origin-when-cross-origin",
            "uri: {uri}"
        );
    }

    // headers are also present on error and rate-limited responses
    let req = Request::builder()
        .method("GET")
        .uri("/v1/agents/me")
        .body(Body::empty())
        .unwrap();
    let res = Oneshot::new(router.clone(), req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(res.headers().contains_key("strict-transport-security"));
    assert!(res.headers().contains_key("content-security-policy"));
    assert!(res.headers().contains_key("x-frame-options"));
    assert!(res.headers().contains_key("x-content-type-options"));
    assert!(res.headers().contains_key("referrer-policy"));
}

// --- Security fix-now: V2 login brute-force throttle ---

#[tokio::test]
async fn login_lockout_after_repeated_failures() {
    let (router, state) = test_app_with_state("loginThrottle");
    let bad = json!({ "email": "nobody@example.com", "password": "wrongpass" });

    // first 5 failures from one IP are plain 401s
    for i in 1..=5 {
        let (status, _) =
            call(&router, "POST", "/ui/login", None, Some(bad.clone()), ).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {i} must still be 401");
    }
    // the 6th is locked out with a Retry-After
    let req = Request::builder()
        .method("POST")
        .uri("/ui/login")
        .header("Content-Type", "application/json")
        .body(Body::from(bad.to_string()))
        .unwrap();
    let res = Oneshot::new(router.clone(), req).await.unwrap();
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry = res
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .expect("429 must carry a Retry-After header");
    assert!(retry > 0 && retry <= 15 * 60);

    // a different IP is unaffected
    let (status, _) = call_raw(
        &router,
        "POST",
        "/ui/login",
        None,
        Some(bad.clone()),
        &[("X-Forwarded-For", "203.0.113.7")],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // the lockout is lifted once the IP records a successful login
    // (simulated here by the same reset the success path performs).
    state.login_throttle.reset("unknown");
    let (status, _) = call(&router, "POST", "/ui/login", None, Some(bad)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "after reset the window starts fresh");
}

// --- Security fix-now: V3 per-IP limit on unauthenticated POSTs ---

#[tokio::test]
async fn unauth_post_rate_limited_per_ip() {
    let router = test_app("ipLimit");

    // POST /v1/agents: the 21st request from one IP gets 429 + Retry-After
    let mut saw_429 = false;
    for i in 0..25 {
        let (status, v) = call(
            &router,
            "POST",
            "/v1/agents",
            None,
            Some(json!({ "name": "x", "handle": format!("h{i}") })),
        )
        .await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            saw_429 = true;
            assert_eq!(v["error"]["code"], "rate_limited");
            assert!(i >= 20, "limit is 20; 429 came at request {}", i + 1);
            break;
        }
        assert_eq!(status, StatusCode::CREATED);
    }
    assert!(saw_429, "expected a 429 after the per-IP limit");

    // another IP has its own budget (X-Forwarded-For sets the key)
    let req = Request::builder()
        .method("POST")
        .uri("/v1/agents")
        .header("Content-Type", "application/json")
        .header("X-Forwarded-For", "198.51.100.9")
        .body(Body::from(json!({ "name": "y", "handle": "otherip" }).to_string()))
        .unwrap();
    let res = Oneshot::new(router.clone(), req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    // /ui/signup counts against the same per-IP budget
    let mut saw_429 = false;
    for i in 0..25 {
        let (status, _) = call(
            &router,
            "POST",
            "/ui/signup",
            None,
            Some(json!({
                "email": format!("u{i}@example.com"),
                "password": "password123",
                "handle": format!("su{i}"),
                "name": "n",
                "webhook_url": null,
            })),
        )
        .await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            saw_429 = true;
            break;
        }
    }
    assert!(saw_429, "signup spam must hit the per-IP limit too");
}

// --- Security fix-now: V4 Secure session cookie ---

#[tokio::test]
async fn session_cookie_is_secure() {
    let router = test_app("secureCookie");
    let req = Request::builder()
        .method("POST")
        .uri("/ui/signup")
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({
                "email": "sec@example.com",
                "password": "password123",
                "handle": "secure",
                "name": "n",
                "webhook_url": null,
            })
            .to_string(),
        ))
        .unwrap();
    let res = Oneshot::new(router.clone(), req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .expect("signup must set the session cookie");
    assert!(cookie.starts_with("aghub_session="), "cookie name: {cookie}");
    for attr in ["Secure", "HttpOnly", "SameSite=Lax"] {
        assert!(cookie.contains(attr), "cookie missing {attr}: {cookie}");
    }

    // logout clears the cookie with the same flags
    let req = Request::builder()
        .method("POST")
        .uri("/ui/logout")
        .body(Body::empty())
        .unwrap();
    let res = Oneshot::new(router, req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res.headers().get("set-cookie").unwrap().to_str().unwrap();
    assert!(cookie.contains("Secure"), "logout cookie missing Secure: {cookie}");
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"));
}
