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
    signup_user(router, handle, webhook).await
}

/// Human signup: 1 email = 1 workspace, with a linked agent holding the
/// bearer token. Signup sets the session cookie and returns the new agent's
/// token once (provisioning); /ui/api/me never re-exposes it.
async fn signup_user(router: &RouterT, handle: &str, webhook: Option<&str>) -> (i64, String) {
    let email = format!("{handle}@example.com");
    let req = Request::builder()
        .method("POST")
        .uri("/ui/signup")
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({
                "email": email,
                "password": "password123",
                "handle": handle,
                "name": handle,
                "webhook_url": webhook,
            })
            .to_string(),
        ))
        .unwrap();
    let res = Oneshot::new(router.clone(), req).await.unwrap();
    let status = res.status();
    assert!(
        res.headers().get("set-cookie").is_some(),
        "signup must set the session cookie"
    );
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "signup failed: {v}");
    let agent = v["agent"]["id"].as_i64().unwrap();
    let token = v["agent"]["token"].as_str().unwrap().to_string();
    (agent, token)
}

/// Add another agent to the CALLER's workspace via the authenticated
/// POST /v1/agents. The new agent's workspace is always the caller's.
async fn add_agent(
    router: &RouterT,
    owner_token: &str,
    handle: &str,
    webhook: Option<&str>,
) -> (i64, String) {
    let (status, v) = call(
        router,
        "POST",
        "/v1/agents",
        Some(owner_token),
        Some(json!({ "name": handle, "handle": handle, "webhook_url": webhook })),
    )
    .await;
    // Never embed the response body here: on a botched 2xx it would print
    // the freshly minted token into the test output.
    assert_eq!(status, StatusCode::CREATED, "agent create failed for {handle}");
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
    // Breaking change: open self-registration is gone — creating an agent
    // requires a valid bearer token (workspaces are the tenant boundary).
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(
        &router,
        "POST",
        "/v1/agents",
        Some(&token),
        Some(json!({ "name": "x", "handle": "alpha" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(
        &router,
        "POST",
        "/v1/agents",
        Some(&token),
        Some(json!({ "name": "x", "handle": "Bad Handle!" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &router,
        "POST",
        "/v1/agents",
        Some(&token),
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
    // beta joins alpha's workspace (signups each get their own).
    let (a2, t2) = add_agent(&router, &t1, "beta", None).await;

    let (status, v) = call(
        &router,
        "POST",
        "/v1/channels",
        Some(&t1),
        Some(json!({ "name": "eng", "member_ids": [a1] })),
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
    // The inbox Reply button routes by sender (and channel), so the row payload
    // must carry both, or replying from the inbox has nowhere to go.
    let item = items
        .iter()
        .find(|i| i["message"]["id"].as_i64() == Some(msg_id))
        .expect("inbox carries the message");
    assert_eq!(item["message"]["sender_id"].as_i64(), Some(a1));
    assert_eq!(item["message"]["sender_handle"].as_str(), Some("alpha"));
    assert_eq!(item["message"]["channel_id"].as_i64(), Some(chan_id));
    let (status, _) = call(&router, "POST", "/v1/inbox/read", Some(&t2), Some(json!({ "all": true }))).await;
    assert_eq!(status, StatusCode::OK);
    let (_, v) = call(&router, "GET", "/v1/inbox?unread=true", Some(&t2), None).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 0);
}

// --- Urgency: urgent flag on channel messages ---

#[tokio::test]
async fn urgent_flag_on_channel_messages() {
    let (base, received) = start_webhook_server().await;
    let router = test_app("urgent");
    let (a1, t1) = register(&router, "alpha", None).await;
    // beta has a webhook, so channel messages to beta produce deliveries
    // whose payloads must carry the urgent flag.
    let (a2, t2) = add_agent(&router, &t1, "beta", Some(&format!("{base}/run"))).await;

    let (status, v) = call(
        &router,
        "POST",
        "/v1/channels",
        Some(&t1),
        Some(json!({ "name": "ops", "member_ids": [a1, a2] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let chan = v["channel"]["id"].as_i64().unwrap();

    // urgent message: the flag is stored and echoed back
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t1),
        // wake: true — since lens 1 the channel default is mentions-only wake,
        // so an urgent message with no @-mention would never reach the webhook.
        Some(json!({ "body": "prod is down", "urgent": true, "wake": true })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let urgent_id = v["message"]["id"].as_i64().unwrap();
    assert_eq!(v["message"]["urgent"], true, "urgent must round-trip");

    // explicit non-urgent and omitted flag both default to false
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t1),
        Some(json!({ "body": "all clear", "urgent": false })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let normal_id = v["message"]["id"].as_i64().unwrap();
    assert_eq!(v["message"]["urgent"], false);
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t1),
        Some(json!({ "body": "no flag" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["message"]["urgent"], false, "omitted flag must default to false");

    // GET channel messages returns the flag for both
    let (status, v) = call(
        &router,
        "GET",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t2),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let messages = v["messages"].as_array().unwrap();
    let urgent = messages
        .iter()
        .find(|m| m["id"].as_i64() == Some(urgent_id))
        .expect("urgent message must be listed");
    assert_eq!(urgent["urgent"], true);
    let normal = messages
        .iter()
        .find(|m| m["id"].as_i64() == Some(normal_id))
        .expect("normal message must be listed");
    assert_eq!(normal["urgent"], false);

    // the inbox response carries the flag too
    let (status, v) = call(&router, "GET", "/v1/inbox?unread=true", Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK);
    let items = v["items"].as_array().unwrap();
    let urgent_item = items
        .iter()
        .find(|i| i["message"]["id"].as_i64() == Some(urgent_id))
        .expect("urgent message must be in beta's inbox");
    assert_eq!(urgent_item["message"]["urgent"], true);

    // the webhook payload for the urgent channel message includes the flag
    let mut got_payload = false;
    for _ in 0..40 {
        if let Some(p) = received
            .lock()
            .unwrap()
            .iter()
            .find(|p| p["message"]["body"] == "prod is down")
        {
            assert_eq!(p["message"]["urgent"], true, "webhook payload must carry the flag");
            got_payload = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(got_payload, "webhook never received the urgent message payload");
}

// --- DMs ---

#[tokio::test]
async fn dm_flow_and_pagination() {
    let router = test_app("dm");
    let (a1, t1) = register(&router, "alpha", None).await;
    let (a2, t2) = add_agent(&router, &t1, "beta", None).await;
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
    let (_a2, t2) = add_agent(&router, &t1, "beta", None).await;
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
    let (a2, t2) = add_agent(&router, &t1, "beta", None).await;

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

// --- owner self-service API keys (workspace admin only) ---

/// POST /ui/login and return the session cookie.
async fn login_cookie(router: &RouterT, email: &str) -> String {
    let req = Request::builder()
        .method("POST")
        .uri("/ui/login")
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({ "email": email, "password": "password123" }).to_string(),
        ))
        .unwrap();
    let res = Oneshot::new(router.clone(), req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "login {email} must succeed");
    res.headers()
        .get("set-cookie")
        .expect("login sets the session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

/// GET /ui/api/me with a session cookie: status, parsed body and raw text.
async fn me_with_cookie(router: &RouterT, cookie: &str) -> (StatusCode, Value, String) {
    let req = Request::builder()
        .uri("/ui/api/me")
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap();
    let res = Oneshot::new(router.clone(), req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null), raw)
}

/// POST /v1/agents as the signup owner (a workspace admin): 201, a non-empty
/// token, and that token authenticates as the freshly created agent.
#[tokio::test]
async fn admin_creates_agent_and_its_token_authenticates() {
    let router = test_app("keyCreate");
    let (_owner, owner_token) = register(&router, "kc-owner", None).await;
    // add_agent asserts 201 and a usable token field for "kc-bot".
    let (bot_id, bot_token) = add_agent(&router, &owner_token, "kc-bot", None).await;
    assert!(!bot_token.is_empty(), "the create response must carry a token");

    // The minted token authenticates as the new agent, in the owner's workspace.
    let (status, v) = call(&router, "GET", "/v1/agents/me", Some(&bot_token), None).await;
    assert_eq!(status, StatusCode::OK, "the minted token must authenticate");
    assert_eq!(v["agent"]["id"].as_i64(), Some(bot_id));
    assert_eq!(v["agent"]["handle"], "kc-bot");
    let (status, v) = call(&router, "GET", "/v1/agents", Some(&bot_token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["agents"].as_array().unwrap().len(), 2, "same workspace");
}

/// Creating agents is workspace-admin only now: a plain member gets 403 and
/// mints nothing.
#[tokio::test]
async fn non_admin_member_cannot_create_agents() {
    let router = test_app("keyNoCreate");
    let (_owner, owner_token) = register(&router, "nc-owner", None).await;
    let (_member, member_token) = add_agent(&router, &owner_token, "nc-member", None).await;

    let (status, _) = call(
        &router,
        "POST",
        "/v1/agents",
        Some(&member_token),
        Some(json!({ "name": "nc-peer", "handle": "nc-peer", "webhook_url": null })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "members must not mint tokens");
    // and nothing was created
    let (status, v) = call(&router, "GET", "/v1/agents", Some(&owner_token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["agents"].as_array().unwrap().len(), 2);
}

/// POST /v1/agents/{id}/token as the workspace admin: the new token works,
/// the old one is revoked immediately.
#[tokio::test]
async fn admin_rotates_another_agents_token() {
    let router = test_app("keyRotate");
    let (_owner, owner_token) = register(&router, "rot-owner", None).await;
    let (bot_id, bot_token) = add_agent(&router, &owner_token, "rot-bot", None).await;

    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/agents/{bot_id}/token"),
        Some(&owner_token),
        None,
    )
    .await;
    // Never print the response body here — it carries the fresh token.
    assert_eq!(status, StatusCode::OK, "admin rotate must succeed");
    assert_eq!(v["agent"]["id"].as_i64(), Some(bot_id));
    let new_token = v["token"].as_str().expect("rotate returns a token").to_string();
    assert!(!new_token.is_empty());

    // the new token authenticates as the target agent...
    let (status, v) = call(&router, "GET", "/v1/agents/me", Some(&new_token), None).await;
    assert_eq!(status, StatusCode::OK, "the new token must work");
    assert_eq!(v["agent"]["id"].as_i64(), Some(bot_id));
    // ...and the old one is revoked immediately
    let (status, _) = call(&router, "GET", "/v1/agents/me", Some(&bot_token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the old token must be dead");
}

/// A plain member cannot rotate anyone's key — not a peer's, not even its
/// own through the {id} route (self-rotation stays on /v1/agents/me/token).
#[tokio::test]
async fn non_admin_cannot_rotate_anyones_token() {
    let router = test_app("keyNoRotate");
    let (_owner, owner_token) = register(&router, "nr-owner", None).await;
    let (member_id, member_token) = add_agent(&router, &owner_token, "nr-member", None).await;
    let (peer_id, peer_token) = add_agent(&router, &owner_token, "nr-peer", None).await;

    for target in [peer_id, member_id] {
        let (status, _) = call(
            &router,
            "POST",
            &format!("/v1/agents/{target}/token"),
            Some(&member_token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "member rotate of {target} must be 403");
    }
    // nothing changed: the peer's key still works
    let (status, v) = call(&router, "GET", "/v1/agents/me", Some(&peer_token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["agent"]["id"].as_i64(), Some(peer_id));
    // ...but the self-rotate endpoint keeps working for everyone
    let (status, v) = call(&router, "POST", "/v1/agents/me/token", Some(&member_token), None).await;
    assert_eq!(status, StatusCode::OK, "self-rotate must keep working");
    assert!(v["token"].as_str().map(|t| !t.is_empty()).unwrap_or(false));
    let (status, _) = call(&router, "GET", "/v1/agents/me", Some(&member_token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the old self token is revoked");
}

/// Rotating an agent that lives in ANOTHER workspace is a 404 that never
/// leaks the agent's existence — and does not revoke anything.
#[tokio::test]
async fn rotating_an_agent_in_another_workspace_is_not_found() {
    let router = test_app("keyRotateCross");
    let (_a, a_token) = register(&router, "cross-a", None).await;
    let (b_id, b_token) = register(&router, "cross-b", None).await;

    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/agents/{b_id}/token"),
        Some(&a_token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cross-workspace ids are 404");
    let (status, _) = call(&router, "GET", "/v1/agents/me", Some(&b_token), None).await;
    assert_eq!(status, StatusCode::OK, "the target's key must be untouched");
}

/// The directory listing never carries a token field — not the key itself,
/// not even the word.
#[tokio::test]
async fn agents_list_entries_never_contain_a_token() {
    let router = test_app("keyListLeak");
    let (_owner, owner_token) = register(&router, "ll-owner", None).await;
    let (_bot_id, bot_token) = add_agent(&router, &owner_token, "ll-bot", None).await;

    let (status, v) = call(&router, "GET", "/v1/agents", Some(&owner_token), None).await;
    assert_eq!(status, StatusCode::OK);
    let agents = v["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 2);
    assert!(
        agents.iter().all(|a| a.get("token").is_none()),
        "no entry may carry a token field"
    );
    let raw = v.to_string();
    assert!(!raw.contains(&owner_token) && !raw.contains(&bot_token));
    assert!(!raw.contains("\"token\""), "the listing must not even name the field");
}

/// /ui/api/me tells the UI whether the signed-in human's agent is the
/// workspace admin — and still never exposes any token. An invited member's
/// human session must report is_workspace_admin: false.
#[tokio::test]
async fn ui_me_reports_workspace_admin_and_never_a_token() {
    let router = test_app("keyMeAdmin");
    let (_owner, owner_token) = register(&router, "ma-owner", None).await;

    // The signup owner is the workspace admin.
    let cookie = login_cookie(&router, "ma-owner@example.com").await;
    let (status, v, raw) = me_with_cookie(&router, &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["is_workspace_admin"], true, "the owner is the admin: {raw}");
    assert!(v["agent"].get("token").is_none(), "me must never carry a token");
    assert!(
        !raw.contains(&owner_token),
        "the bearer token must never appear in /ui/api/me"
    );

    // Invite a second human: their agent joins the owner's workspace as a
    // plain member, so their session must NOT be a workspace admin.
    let (status, v) = call(
        &router,
        "POST",
        "/v1/workspaces/invites",
        Some(&owner_token),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "invite creation: {v}");
    let invite = v["invite"]["token"].as_str().unwrap().to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/ui/signup")
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({
                "email": "ma-guest@example.com",
                "password": "password123",
                "handle": "ma-guest",
                "name": "ma-guest",
                "webhook_url": null,
                "invite_token": invite,
            })
            .to_string(),
        ))
        .unwrap();
    let res = Oneshot::new(router.clone(), req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "invited signup must succeed");

    let guest_cookie = login_cookie(&router, "ma-guest@example.com").await;
    let (status, v, raw) = me_with_cookie(&router, &guest_cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["is_workspace_admin"], false, "a member is not the admin: {raw}");
    assert!(v["agent"].get("token").is_none());
    // the guest landed in the owner's workspace
    let (_, owner_me, _) = me_with_cookie(&router, &cookie).await;
    assert_eq!(
        v["workspace"]["id"], owner_me["workspace"]["id"],
        "the invited member shares the owner's workspace"
    );
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
    let (a2, t2) = add_agent(&router, &t1, "beta", None).await;
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
    let (a2, _t2) = add_agent(&router, &t1, "beta", Some("http://127.0.0.1:9/run")).await;

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
    let (a2, t2) = add_agent(&router, &t1, "beta", None).await;

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

#[tokio::test]
async fn inbox_since_returns_only_newer_receipts_ascending() {
    let router = test_app("inboxSince");
    let (_, t1) = register(&router, "alphasince", None).await;
    let (a2, t2) = add_agent(&router, &t1, "betasince", None).await;

    // three DMs, so beta has three receipts in ascending id order
    let mut bodies = Vec::new();
    for body in ["one", "two", "three"] {
        let (status, v) = call(
            &router,
            "POST",
            &format!("/v1/dms/{a2}/messages"),
            Some(&t1),
            Some(json!({ "body": body })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        bodies.push(v["message"]["id"].as_i64().unwrap());
    }

    // without since: newest first, exactly as before
    let (_, v) = call(&router, "GET", "/v1/inbox", Some(&t2), None).await;
    let ids: Vec<i64> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["receipt_id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids.len(), 3);
    assert!(ids[0] > ids[1] && ids[1] > ids[2], "default order is newest first");

    // since=<oldest>: only the two newer receipts, in ASCENDING order
    let (status, v) = call(&router, "GET", &format!("/v1/inbox?since={}", ids[2]), Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK);
    let since_ids: Vec<i64> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["receipt_id"].as_i64().unwrap())
        .collect();
    assert_eq!(since_ids, vec![ids[1], ids[0]], "since must walk forward in ascending order");

    // since past everything: empty
    let (_, v) = call(&router, "GET", &format!("/v1/inbox?since={}", ids[0]), Some(&t2), None).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 0);

    // since composes with unread=true: mark the middle message read
    let (status, _) = call(
        &router,
        "POST",
        "/v1/inbox/read",
        Some(&t2),
        Some(json!({ "message_ids": [bodies[1]] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, v) = call(
        &router,
        "GET",
        &format!("/v1/inbox?since={}&unread=true", ids[2]),
        Some(&t2),
        None,
    )
    .await;
    let unread_ids: Vec<i64> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["receipt_id"].as_i64().unwrap())
        .collect();
    assert_eq!(unread_ids, vec![ids[0]], "since+unread must return only the newer unread receipt");

    // plain unread=true is unchanged: newest first, only the two unread
    let (_, v) = call(&router, "GET", "/v1/inbox?unread=true", Some(&t2), None).await;
    let plain_ids: Vec<i64> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["receipt_id"].as_i64().unwrap())
        .collect();
    assert_eq!(plain_ids, vec![ids[0], ids[2]], "plain unread=true must stay newest-first");
}

/// Helper: subscribe `token` to the SSE stream, returning its body reader.
/// Asserts the SSE handshake succeeded.
async fn sse_connect(router: &RouterT, uri: &str, token: &str) -> Body {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = Oneshot::new(router.clone(), req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "text/event-stream",
        "must be an SSE stream"
    );
    res.into_body()
}

#[tokio::test]
async fn sse_stream_replays_missed_inbox_then_live_without_duplicates() {
    let router = test_app("sseReplay");
    let (_, t1) = register(&router, "alphareplay", None).await;
    let (a2, t2) = add_agent(&router, &t1, "betareplay", None).await;

    // a DM lands BEFORE beta connects: it must be replayed on connect
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/dms/{a2}/messages"),
        Some(&t1),
        Some(json!({ "body": "before connect" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let mut body = sse_connect(&router, "/v1/inbox/stream", &t2).await;

    // replay arrives without any new message being sent
    let mut all = String::new();
    let mut saw_replay = false;
    for _ in 0..40 {
        match tokio::time::timeout(std::time::Duration::from_millis(250), body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(chunk) = frame.into_data() {
                    all.push_str(&String::from_utf8_lossy(&chunk));
                    if all.contains("\"before connect\"") {
                        saw_replay = true;
                        break;
                    }
                }
            }
            _ => continue,
        }
    }
    assert!(saw_replay, "SSE stream must replay the pre-connect DM");

    // a live DM after connect must still be pushed, and only to beta
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/dms/{a2}/messages"),
        Some(&t1),
        Some(json!({ "body": "after connect" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let mut saw_live = false;
    for _ in 0..40 {
        match tokio::time::timeout(std::time::Duration::from_millis(250), body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(chunk) = frame.into_data() {
                    all.push_str(&String::from_utf8_lossy(&chunk));
                    if all.contains("\"after connect\"") {
                        saw_live = true;
                        break;
                    }
                }
            }
            _ => continue,
        }
    }
    assert!(saw_live, "SSE stream must still push live DMs after replay");

    // keep draining a little: the replayed receipt must never be emitted twice
    for _ in 0..8 {
        match tokio::time::timeout(std::time::Duration::from_millis(150), body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(chunk) = frame.into_data() {
                    all.push_str(&String::from_utf8_lossy(&chunk));
                }
            }
            _ => continue,
        }
    }
    assert_eq!(
        all.matches("\"before connect\"").count(),
        1,
        "replayed receipt must be emitted exactly once"
    );

    // alpha, who owns no receipts and receives no DM, gets neither replay nor live
    let mut other = sse_connect(&router, "/v1/inbox/stream", &t1).await;
    let mut other_all = String::new();
    for _ in 0..8 {
        match tokio::time::timeout(std::time::Duration::from_millis(150), other.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(chunk) = frame.into_data() {
                    other_all.push_str(&String::from_utf8_lossy(&chunk));
                }
            }
            _ => continue,
        }
    }
    assert!(
        !other_all.contains("before connect") && !other_all.contains("after connect"),
        "another agent's stream must see neither beta's replay nor live events"
    );
}

#[tokio::test]
async fn sse_stream_since_param_replays_only_newer_receipts() {
    let router = test_app("sseSince");
    let (_, t1) = register(&router, "alphasincex", None).await;
    let (a2, t2) = add_agent(&router, &t1, "betasincex", None).await;

    // two DMs; capture beta's receipt ids from the list endpoint
    for body in ["older", "newer"] {
        let (status, _) = call(
            &router,
            "POST",
            &format!("/v1/dms/{a2}/messages"),
            Some(&t1),
            Some(json!({ "body": body })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (_, v) = call(&router, "GET", "/v1/inbox", Some(&t2), None).await;
    let ids: Vec<i64> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["receipt_id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    let (older_id, _newest) = (ids[1], ids[0]);

    // connect with since=<older receipt>: only the newer DM may be replayed
    let mut body = sse_connect(&router, &format!("/v1/inbox/stream?since={older_id}"), &t2).await;
    let mut all = String::new();
    let mut saw_newer = false;
    for _ in 0..40 {
        match tokio::time::timeout(std::time::Duration::from_millis(250), body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(chunk) = frame.into_data() {
                    all.push_str(&String::from_utf8_lossy(&chunk));
                    if all.contains("\"newer\"") {
                        saw_newer = true;
                        break;
                    }
                }
            }
            _ => continue,
        }
    }
    assert!(saw_newer, "since must replay receipts newer than the cursor");
    for _ in 0..8 {
        match tokio::time::timeout(std::time::Duration::from_millis(150), body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(chunk) = frame.into_data() {
                    all.push_str(&String::from_utf8_lossy(&chunk));
                }
            }
            _ => continue,
        }
    }
    assert!(
        !all.contains("\"older\""),
        "since must not replay receipts at or below the cursor"
    );
}

// --- Security fix-now: V1 security headers ---

#[tokio::test]
async fn security_headers_on_all_responses() {
    let router = test_app("secHeaders");
    for uri in ["/", "/login", "/signup", "/app", "/static/app.css", "/healthz"] {
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

    // /v1 routes are authenticated now (breaking change); the security
    // headers must still ride along on the 401.
    let req = Request::builder().method("GET").uri("/v1/channels").body(Body::empty()).unwrap();
    let res = Oneshot::new(router.clone(), req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "/v1/channels");
    let h = res.headers();
    assert_eq!(
        h.get("strict-transport-security").unwrap(),
        "max-age=31536000; includeSubDomains",
        "uri: /v1/channels"
    );

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

    // a different IP using the SAME email is blocked too — the per-email
    // guard stops a targeted attack from hopping source IPs. IP and email
    // guards share the same fixed 15-minute window.
    let (status, v) = call_raw(
        &router,
        "POST",
        "/ui/login",
        None,
        Some(bad.clone()),
        &[("X-Forwarded-For", "203.0.113.7")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "same email from a new IP must be blocked: {v}"
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("retry after"),
        "429 must carry a retry hint: {v}"
    );

    // a different email from that same fresh IP is unaffected
    let (status, _) = call_raw(
        &router,
        "POST",
        "/ui/login",
        None,
        Some(json!({ "email": "someone-else@example.com", "password": "wrongpass" })),
        &[("X-Forwarded-For", "203.0.113.7")],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // the lockout is temporary: the window rolls over after 15 minutes and a
    // successful login resets both counters (simulated here by the same
    // resets the middleware and handler perform).
    state.login_throttle.reset("unknown");
    state.login_throttle.reset("email:nobody@example.com");
    let (status, _) = call(&router, "POST", "/ui/login", None, Some(bad)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "after reset the window starts fresh"
    );
}

// --- Security fix-now: V3 per-IP limit on unauthenticated POSTs ---

#[tokio::test]
async fn unauth_post_rate_limited_per_ip() {
    let router = test_app("ipLimit");
    let (_id, token) = register(&router, "alpha", None).await;

    // POST /v1/agents: the 21st request from one IP gets 429 + Retry-After
    let mut saw_429 = false;
    for i in 0..25 {
        let (status, v) = call(
            &router,
            "POST",
            "/v1/agents",
            Some(&token),
            Some(json!({ "name": "x", "handle": format!("h{i}") })),
        )
        .await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            saw_429 = true;
            assert_eq!(v["error"]["code"], "rate_limited");
            // Limit is 20/window/IP; the signup above consumed one slot.
            assert!(i >= 19, "limit is 20; 429 came at request {}", i + 1);
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
        .header("Authorization", format!("Bearer {token}"))
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

// --- GitHub integration: channels link repos and browse PRs ---

/// Router wired with a MockGitHub transport, plus the shared mock handle
/// so tests can script responses and assert on the calls made.
fn github_app(name: &str) -> (RouterT, Arc<aghub::github::MockGitHub>) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let db = aghub::db::Db::open(&format!(
        "/tmp/aghub-test-{}-{}-{}.db",
        name,
        n,
        std::process::id()
    ))
    .expect("open test db");
    db.migrate().expect("migrate");
    let mut state = aghub::state::AppState::new(db, reqwest::Client::builder().build().unwrap());
    let mock = Arc::new(aghub::github::MockGitHub::new());
    state.github = mock.clone();
    state.github_cache_ttl_secs = 1; // exercise the refresh path quickly
    (aghub::api::build_router(state), mock)
}

/// alpha + beta in one channel; ghost is a registered outsider (own workspace).
async fn github_setup(router: &RouterT) -> (i64, String, i64, String, String) {
    let (a1, t1) = register(router, "alpha", None).await;
    let (a2, t2) = add_agent(router, &t1, "beta", None).await;
    let (_, tout) = register(router, "ghost", None).await;
    let (status, v) = call(
        router,
        "POST",
        "/v1/channels",
        Some(&t1),
        Some(json!({ "name": "gh-ops", "member_ids": [a1, a2] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let chan = v["channel"]["id"].as_i64().unwrap();
    (chan, t1, a1, t2, tout)
}

#[tokio::test]
async fn github_link_list_unlink_and_visibility() {
    let (router, mock) = github_app("gh-link");
    mock.set_repo(
        "acme",
        "api",
        json!({ "full_name": "acme/api", "default_branch": "develop" }),
    );
    let (chan, t1, a1, t2, tout) = github_setup(&router).await;
    let repos_uri = format!("/v1/channels/{chan}/github/repos");

    // Link: fetches the repo first, persists the default branch it returned.
    let (status, v) = call(
        &router,
        "POST",
        &repos_uri,
        Some(&t1),
        Some(json!({ "owner": "acme", "name": "api" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let repo_id = v["repo"]["id"].as_i64().unwrap();
    assert_eq!(v["repo"]["owner"], "acme");
    assert_eq!(v["repo"]["name"], "api");
    assert_eq!(v["repo"]["default_branch"], "develop");
    assert_eq!(v["repo"]["linked_by"], a1);

    // Re-link by a plain member is now admin-gated.
    let (status, v) = call(
        &router,
        "POST",
        &repos_uri,
        Some(&t2),
        Some(json!({ "owner": "acme", "name": "api" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "members must not link: {v}");
    assert_eq!(
        v["error"]["message"], "only channel admins can manage GitHub repositories"
    );

    // Members see the links; an outsider is 403; unknown channel is 404.
    let (status, v) = call(&router, "GET", &repos_uri, Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["repos"].as_array().unwrap().len(), 1);
    let (status, _) = call(&router, "GET", &repos_uri, Some(&tout), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "outsider (another workspace) must not list links");
    let (status, v) = call(&router, "GET", "/v1/channels/999999/github/repos", Some(&t1), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["error"]["code"], "not_found");

    // Unlink is admin-gated too: the member's attempt is denied, the
    // admin's succeeds, a second unlink is 404.
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("{repos_uri}/{repo_id}"),
        Some(&t2),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "members must not unlink");
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("{repos_uri}/{repo_id}"),
        Some(&t1),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("{repos_uri}/{repo_id}"),
        Some(&t1),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "second unlink by admin is 404");
    let (status, v) = call(&router, "GET", &repos_uri, Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["repos"].as_array().unwrap().len(), 0, "repos after unlink");
}

#[tokio::test]
async fn github_link_validates_slugs_and_upstream_errors() {
    let (router, mock) = github_app("gh-valid");
    let (chan, t1, _, _, _) = github_setup(&router).await;
    let repos_uri = format!("/v1/channels/{chan}/github/repos");

    // Malformed slugs are rejected before any GitHub call.
    for bad in [
        json!({ "owner": "", "name": "api" }),
        json!({ "owner": "acme", "name": "../etc" }),
        json!({ "owner": "-lead", "name": "api" }),
        json!({ "owner": "acme", "name": "a b" }),
    ] {
        let (status, _) = call(&router, "POST", &repos_uri, Some(&t1), Some(bad.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "bad slug must 400: {bad}");
    }
    // Unknown fields are rejected too — by axum's Json extractor (422).
    let (status, _) = call(
        &router,
        "POST",
        &repos_uri,
        Some(&t1),
        Some(json!({ "owner": "acme", "name": "api", "extra": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "unknown field must be rejected");
    assert!(mock.calls().is_empty(), "invalid slugs must not reach GitHub");

    // A repo GitHub cannot see (404) cannot be linked.
    mock.set_repo_error("ghost", "repo", 404, "Not Found");
    let (status, v) = call(
        &router,
        "POST",
        &repos_uri,
        Some(&t1),
        Some(json!({ "owner": "ghost", "name": "repo" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["error"]["code"], "github_not_found");

    // Other upstream failures are a 502 with context, not a crash.
    mock.set_repo_error("broken", "repo", 500, "kaboom");
    let (status, v) = call(
        &router,
        "POST",
        &repos_uri,
        Some(&t1),
        Some(json!({ "owner": "broken", "name": "repo" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{v}");
    assert_eq!(v["error"]["code"], "github_error");
    assert!(v["error"]["message"].as_str().unwrap().contains("500"));
}

#[tokio::test]
async fn github_repo_cap_per_channel() {
    let (router, mock) = github_app("gh-cap");
    let (chan, t1, _, _, _) = github_setup(&router).await;
    let repos_uri = format!("/v1/channels/{chan}/github/repos");
    for i in 0..10 {
        let (owner, name) = (format!("acme{i}"), "repo");
        mock.set_repo(&owner, name, json!({ "default_branch": "main" }));
        let (status, v) = call(
            &router,
            "POST",
            &repos_uri,
            Some(&t1),
            Some(json!({ "owner": owner, "name": name })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "link {i} failed: {v}");
    }
    mock.set_repo("acme11", "repo", json!({ "default_branch": "main" }));
    let (status, v) = call(
        &router,
        "POST",
        &repos_uri,
        Some(&t1),
        Some(json!({ "owner": "acme11", "name": "repo" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"]["code"], "github_repo_cap_reached");

    // Re-linking an existing repo stays under the cap.
    let (status, _) = call(
        &router,
        "POST",
        &repos_uri,
        Some(&t1),
        Some(json!({ "owner": "acme0", "name": "repo" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn github_pulls_cache_filters_and_ttl_refresh() {
    let (router, mock) = github_app("gh-pulls");
    mock.set_repo("acme", "api", json!({ "default_branch": "main" }));
    let (chan, t1, _, t2, tout) = github_setup(&router).await;
    let (_, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/github/repos"),
        Some(&t1),
        Some(json!({ "owner": "acme", "name": "api" })),
    )
    .await;
    let repo_id = v["repo"]["id"].as_i64().unwrap();
    let base = format!("/v1/channels/{chan}/github/repos/{repo_id}/pulls");

    mock.set_pulls(
        "acme",
        "api",
        json!([
            { "number": 2, "title": "Open draft", "state": "open", "draft": true,
              "user": { "login": "octocat" }, "html_url": "https://github.com/acme/api/pull/2" },
            { "number": 1, "title": "Open ready", "state": "open", "draft": false,
              "user": { "login": "hubot" }, "html_url": "https://github.com/acme/api/pull/1" },
            { "number": 7, "title": "Merged", "state": "closed", "draft": false,
              "user": { "login": "octocat" }, "html_url": "https://github.com/acme/api/pull/7" }
        ]),
    );

    // First list fetches from the transport and caches.
    let (status, v) = call(&router, "GET", &format!("{base}?state=open"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let pulls = v["pulls"].as_array().unwrap();
    assert_eq!(pulls.len(), 2, "open only: {v}");
    assert_eq!(pulls[0]["number"], 2, "ordered by number desc");
    assert_eq!(pulls[0]["draft"], true);
    assert_eq!(pulls[0]["author"], "octocat");

    let (status, v) = call(&router, "GET", &format!("{base}?state=closed"), Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["pulls"].as_array().unwrap().len(), 1);
    assert_eq!(v["pulls"][0]["number"], 7);

    let pull_calls = mock.calls().iter().filter(|c| c.contains("/pulls")).count();
    assert_eq!(pull_calls, 1, "one upstream fetch serves three cache reads");

    // Within the TTL a changed upstream is not re-fetched.
    mock.set_pulls(
        "acme",
        "api",
        json!([{ "number": 9, "title": "Fresh", "state": "open", "draft": false,
                 "user": { "login": "new" }, "html_url": "https://github.com/acme/api/pull/9" }]),
    );
    let (_, v) = call(&router, "GET", &base, Some(&t1), None).await;
    assert_eq!(v["pulls"].as_array().unwrap().len(), 3, "stale cache served within TTL");

    // After the TTL the list refreshes and the new upstream wins.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (status, v) = call(&router, "GET", &base, Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    let pulls = v["pulls"].as_array().unwrap();
    assert_eq!(pulls.len(), 1, "refreshed after TTL: {v}");
    assert_eq!(pulls[0]["number"], 9);
    let pull_calls = mock.calls().iter().filter(|c| c.contains("/pulls")).count();
    assert_eq!(pull_calls, 2, "exactly one refresh after the TTL");

    // Validation and access control.
    let (status, _) = call(&router, "GET", &format!("{base}?state=weird"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&router, "GET", &format!("{base}?limit=100000"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK, "limit is clamped, not rejected");
    let (status, _) = call(&router, "GET", &base, Some(&tout), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "outsider (another workspace) must not list pulls");
    let (status, _) = call(
        &router,
        "GET",
        &format!("/v1/channels/{chan}/github/repos/999999/pulls"),
        Some(&t1),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "link of another channel is 404");

    // Unlinking drops the cache with it.
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/v1/channels/{chan}/github/repos/{repo_id}"),
        Some(&t1),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn github_create_pull() {
    let (router, mock) = github_app("gh-create");
    mock.set_repo("acme", "api", json!({ "default_branch": "main" }));
    let (chan, t1, _, t2, tout) = github_setup(&router).await;
    let (_, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/github/repos"),
        Some(&t1),
        Some(json!({ "owner": "acme", "name": "api" })),
    )
    .await;
    let repo_id = v["repo"]["id"].as_i64().unwrap();
    let pulls_uri = format!("/v1/channels/{chan}/github/repos/{repo_id}/pulls");
    let body = json!({ "head": "feature", "base": "main", "title": "Add feature", "body": "desc" });

    // Invalid bodies are rejected locally, without a GitHub call.
    for bad in [
        json!({ "head": "", "base": "main", "title": "T" }),
        json!({ "head": "feature", "base": "", "title": "T" }),
        json!({ "head": "feature", "base": "main", "title": "" }),
        json!({ "head": "feature", "base": "main", "title": "T", "extra": 1 }),
    ] {
        let (status, _) = call(&router, "POST", &pulls_uri, Some(&t1), Some(bad.clone())).await;
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
            "invalid create must be rejected: {bad}"
        );
    }
    assert!(
        !mock.calls().iter().any(|c| c.ends_with("/pulls")),
        "invalid bodies must not reach GitHub"
    );

    // Only channel admins may open PRs: member and outsider are denied.
    let (status, _) = call(&router, "POST", &pulls_uri, Some(&t2), Some(body.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "plain member must not create PRs");
    let (status, _) = call(&router, "POST", &pulls_uri, Some(&tout), Some(body.clone())).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "outsider (another workspace) must not even see the channel");

    // Admin creates: the transport is called and the returned PR is answered.
    mock.push_post_response(
        "/repos/acme/api/pulls",
        Ok(json!({
            "number": 12, "title": "Add feature", "state": "open", "draft": false,
            "html_url": "https://github.com/acme/api/pull/12"
        })),
    );
    let (status, v) = call(&router, "POST", &pulls_uri, Some(&t1), Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["pull"]["number"], 12);
    assert_eq!(v["pull"]["url"], "https://github.com/acme/api/pull/12");
    assert_eq!(v["pull"]["state"], "open");

    // The created PR is cached; the next list shows it without a refresh.
    let (status, v) = call(&router, "GET", &format!("{pulls_uri}?state=open"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let pulls = v["pulls"].as_array().unwrap();
    assert_eq!(pulls.len(), 1, "created PR must be cached: {v}");
    assert_eq!(pulls[0]["number"], 12);
    assert_eq!(pulls[0]["author"], "alpha");

    // An upstream validation failure (422, e.g. branch missing) is passed
    // through as a 400 with GitHub's message; server-side failures stay 502.
    mock.push_post_response(
        "/repos/acme/api/pulls",
        Err(aghub::github::GitHubApiError {
            status: Some(422),
            message: "Validation Failed: A pull request already exists".into(),
        }),
    );
    let (status, v) = call(
        &router,
        "POST",
        &pulls_uri,
        Some(&t1),
        Some(json!({ "head": "feature", "base": "main", "title": "Dup" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["code"], "github_rejected");
    assert!(v["error"]["message"].as_str().unwrap().contains("already exists"));
    mock.push_post_response(
        "/repos/acme/api/pulls",
        Err(aghub::github::GitHubApiError { status: Some(500), message: "kaboom".into() }),
    );
    let (status, v) = call(
        &router,
        "POST",
        &pulls_uri,
        Some(&t1),
        Some(json!({ "head": "feature", "base": "main", "title": "Boom" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{v}");
    assert_eq!(v["error"]["code"], "github_error");

    // Unknown repo link: 404.
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/github/repos/999999/pulls"),
        Some(&t1),
        Some(json!({ "head": "f", "base": "main", "title": "T" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn github_unconfigured_returns_503() {
    // Default AppState ships the disabled transport: linking and listing 503.
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let db = aghub::db::Db::open(&format!(
        "/tmp/aghub-test-gh-noop-{}-{}.db",
        n,
        std::process::id()
    ))
    .unwrap();
    db.migrate().unwrap();
    let mut state = aghub::state::AppState::new(db, reqwest::Client::builder().build().unwrap());
    // Force the disabled transport: the ambient environment may carry a
    // GITHUB_TOKEN, and this test must stay offline either way.
    state.github = Arc::new(aghub::github::NoopGitHub);
    let router = aghub::api::build_router(state);
    let (a1, t1) = register(&router, "alpha", None).await;
    let (status, v) = call(
        &router,
        "POST",
        "/v1/channels",
        Some(&t1),
        Some(json!({ "name": "c", "member_ids": [a1] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let chan = v["channel"]["id"].as_i64().unwrap();

    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/github/repos"),
        Some(&t1),
        Some(json!({ "owner": "acme", "name": "api" })),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(v["error"]["code"], "github_unavailable");

    let (status, v) = call(
        &router,
        "GET",
        &format!("/v1/channels/{chan}/github/repos/1/pulls"),
        Some(&t1),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["error"]["code"], "github_unavailable");

    // Creating a PR is unavailable too.
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/github/repos/1/pulls"),
        Some(&t1),
        Some(json!({ "head": "feature", "base": "main", "title": "T" })),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(v["error"]["code"], "github_unavailable");
}

// --- Workspace per email: multi-tenant isolation ---

#[tokio::test]
async fn slug_derived_from_name_and_deduped() {
    let router = test_app("wsSlug");
    // "Budi Santoso workspace" -> slug 'budi-santoso-workspace'.
    let (_, cookie1) = register_named(&router, "budi", "Budi Santoso").await;
    let (status, v) = call_me_cookie(&router, &cookie1).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["workspace"]["slug"], "budi-santoso-workspace", "{v}");

    // A second user with the same name gets a deduped slug: -2.
    let (_, cookie2) = register_named(&router, "budi2", "Budi Santoso").await;
    let (status, v) = call_me_cookie(&router, &cookie2).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["workspace"]["slug"], "budi-santoso-workspace-2", "{v}");
}

/// Signup with an explicit display name (register() uses the handle);
/// returns the session cookie so /ui/api/me (cookie-only) can be called.
async fn register_named(router: &RouterT, handle: &str, name: &str) -> (i64, String) {
    let email = format!("{handle}@example.com");
    let req = Request::builder()
        .method("POST")
        .uri("/ui/signup")
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({
                "email": email,
                "password": "password123",
                "handle": handle,
                "name": name,
                "webhook_url": null,
            })
            .to_string(),
        ))
        .unwrap();
    let res = Oneshot::new(router.clone(), req).await.unwrap();
    let status = res.status();
    let cookie = res
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .expect("signup must set the session cookie")
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "signup failed: {v}");
    (v["agent"]["id"].as_i64().unwrap(), cookie)
}

/// GET /ui/api/me with the session cookie (the route is cookie-only).
async fn call_me_cookie(router: &RouterT, cookie: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri("/ui/api/me")
        .header("Cookie", cookie)
        .body(Body::empty())
        .unwrap();
    let res = Oneshot::new(router.clone(), req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

#[tokio::test]
async fn signup_creates_private_workspace_with_general() {
    let router = test_app("wsSignup");
    let (_a1, t1) = register(&router, "alpha", None).await;
    let (_a2, t2) = register(&router, "beta", None).await;

    // Each signup lands in its OWN workspace with its own empty #general.
    let (status, v1) = call(&router, "GET", "/v1/channels", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK, "{v1}");
    let channels1 = v1["channels"].as_array().unwrap();
    assert_eq!(channels1.len(), 1, "exactly the signup #general: {v1}");
    assert_eq!(channels1[0]["name"], "general");

    let (status, v2) = call(&router, "GET", "/v1/channels", Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK, "{v2}");
    let channels2 = v2["channels"].as_array().unwrap();
    assert_eq!(channels2.len(), 1, "beta's own #general, not alpha's: {v2}");
    assert_eq!(channels2[0]["name"], "general");
    assert_ne!(
        channels1[0]["id"], channels2[0]["id"],
        "the two signups must not share a channel"
    );

    // alpha's #general is empty and alpha is its admin.
    let chan1 = channels1[0]["id"].as_i64().unwrap();
    let (status, v) = call(&router, "GET", &format!("/v1/channels/{chan1}/messages"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["messages"].as_array().unwrap().len(), 0, "fresh #general is empty");
    let (status, _) = call(
        &router,
        "PATCH",
        &format!("/v1/channels/{chan1}"),
        Some(&t1),
        Some(json!({ "topic": "mine" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the signup user admins their own #general");

    // Agents listing is workspace-scoped too.
    let (status, v) = call(&router, "GET", "/v1/agents", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let ids: Vec<i64> = v["agents"].as_array().unwrap().iter().map(|a| a["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![_a1], "alpha must see only their own workspace's agents");
}

#[tokio::test]
async fn cross_workspace_access_is_denied() {
    let router = test_app("wsIso");
    let (_a1, t1) = register(&router, "alpha", None).await;
    let (a2, t2) = register(&router, "beta", None).await;

    // alpha's second channel + beta's channel, in their own workspaces.
    let (status, v) = call(
        &router,
        "POST",
        "/v1/channels",
        Some(&t1),
        Some(json!({ "name": "eng", "topic": "alpha eng" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let alpha_chan = v["channel"]["id"].as_i64().unwrap();

    // beta cannot list or read alpha's channels: 404, never a leak.
    let (status, _) = call(&router, "GET", &format!("/v1/channels/{alpha_chan}/messages"), Some(&t2), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cross-workspace read must be 404");
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{alpha_chan}/messages"),
        Some(&t2),
        Some(json!({ "body": "intrusion" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
    let (status, _) = call(
        &router,
        "PATCH",
        &format!("/v1/channels/{alpha_chan}"),
        Some(&t2),
        Some(json!({ "topic": "hijacked" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cross-workspace PATCH must be 404");

    // beta cannot add itself to alpha's channel (admin + same-workspace checks).
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/channels/{alpha_chan}/members"),
        Some(&t2),
        Some(json!({ "agent_ids": [a2] })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // beta cannot remove a member from alpha's channel either: 404, not 403.
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/v1/channels/{alpha_chan}/members/{a2}"),
        Some(&t2),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cross-workspace member removal must be 404");

    // alpha cannot add beta (another workspace) as a member: 404, no leak.
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/channels/{alpha_chan}/members"),
        Some(&t1),
        Some(json!({ "agent_ids": [a2] })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "adding a foreign agent must be 404");

    // DMs across workspaces fail in both directions.
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/dms/{a2}/messages"),
        Some(&t1),
        Some(json!({ "body": "hello?" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
    let (status, _) = call(&router, "GET", &format!("/v1/dms/{a2}/messages"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, v) = call(&router, "GET", "/v1/dms", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["partners"].as_array().unwrap().len(), 0, "partners stay inside the workspace");

    // Memories: beta cannot recall alpha's private memories.
    let (status, v) = call(
        &router,
        "POST",
        "/v1/memory",
        Some(&t1),
        Some(json!({ "content": "alpha deployment key is hunter2", "kind": "fakta" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let mem_id = v["memory"]["id"].as_i64().unwrap();
    let (status, v) = call(&router, "GET", "/v1/memory?query=deployment+key", Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["memories"].as_array().unwrap().len(), 0, "FTS recall must not cross workspaces");
    let (status, _) = call(&router, "GET", "/v1/memory?query=deployment+key", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK, "owner still recalls it");
    let (status, _) = call(&router, "DELETE", &format!("/v1/memory/{mem_id}"), Some(&t2), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "deleting a foreign memory must be 404");

    // Mail across workspaces fails with 404.
    let (status, _) = call(
        &router,
        "POST",
        "/v1/mail",
        Some(&t1),
        Some(json!({ "to_agent_id": a2, "subject": "s", "body": "b" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "mail into another workspace must be 404");

    // Same for mail by handle: a foreign handle reads as unknown (404).
    let (status, _) = call(
        &router,
        "POST",
        "/v1/mail",
        Some(&t1),
        Some(json!({ "to_handle": "beta", "subject": "s", "body": "b" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "mail by foreign handle must be 404");
}

#[tokio::test]
async fn cross_workspace_runs_are_denied() {
    let router = test_app("wsRuns");
    let (_a1, t1) = register(&router, "alpha", Some("http://127.0.0.1:9/hook")).await;
    let (_a2, t2) = register(&router, "beta", Some("http://127.0.0.1:9/hook")).await;

    // alpha's channel message and DM that beta must not be able to reach.
    let (status, v) = call(
        &router,
        "POST",
        "/v1/channels",
        Some(&t1),
        Some(json!({ "name": "eng", "topic": "alpha eng" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let alpha_chan = v["channel"]["id"].as_i64().unwrap();
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{alpha_chan}/messages"),
        Some(&t1),
        Some(json!({ "body": "alpha channel secret" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let chan_msg = v["message"]["id"].as_i64().unwrap();
    let (helper_id, _ht) = add_agent(&router, &t1, "helper", None).await;
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/dms/{helper_id}/messages"),
        Some(&t1),
        Some(json!({ "body": "alpha dm secret" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let dm_msg = v["message"]["id"].as_i64().unwrap();

    // beta enumerating alpha's ids gets 404 and no run is created — the
    // message body must never be delivered to beta's webhook.
    for msg_id in [chan_msg, dm_msg] {
        let (status, v) = call(
            &router,
            "POST",
            "/v1/runs",
            Some(&t2),
            Some(json!({ "message_id": msg_id })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "foreign message {msg_id} must read as nonexistent: {v}"
        );
        let (_, v) = call(&router, "GET", "/v1/runs", Some(&t2), None).await;
        assert_eq!(v["runs"].as_array().unwrap().len(), 0, "no run may be created for a foreign message");
    }

    // beta's own channel message is still runnable end to end.
    let (status, v) = call(
        &router,
        "POST",
        "/v1/channels",
        Some(&t2),
        Some(json!({ "name": "eng" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let beta_chan = v["channel"]["id"].as_i64().unwrap();
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{beta_chan}/messages"),
        Some(&t2),
        Some(json!({ "body": "beta own trigger" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let own_msg = v["message"]["id"].as_i64().unwrap();
    let (status, v) = call(
        &router,
        "POST",
        "/v1/runs",
        Some(&t2),
        Some(json!({ "message_id": own_msg })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "own channel message must still trigger: {v}");
    assert_eq!(v["run"]["message_id"].as_i64(), Some(own_msg));
}

#[tokio::test]
async fn agent_create_requires_auth_and_joins_callers_workspace() {
    let router = test_app("wsAgent");

    // Unauthenticated create -> 401 (breaking change: open registration gone).
    let (status, v) = call(
        &router,
        "POST",
        "/v1/agents",
        None,
        Some(json!({ "name": "Siera", "handle": "siera" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{v}");

    // Authenticated: lands in the CALLER's workspace, never a global pool.
    let (_a1, t1) = register(&router, "alpha", None).await;
    let (a2, t2) = add_agent(&router, &t1, "siera", None).await;
    let (status, v) = call(&router, "GET", "/v1/agents", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let ids: Vec<i64> = v["agents"].as_array().unwrap().iter().map(|a| a["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![_a1, a2], "both agents share alpha's workspace");
    let (status, v) = call(&router, "GET", "/v1/agents", Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let ids: Vec<i64> = v["agents"].as_array().unwrap().iter().map(|a| a["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![_a1, a2], "the new agent sees the same workspace");

    // Another signup's workspace stays out of it.
    let (_a3, t3) = register(&router, "beta", None).await;
    let (status, v) = call(&router, "GET", "/v1/agents", Some(&t3), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let ids: Vec<i64> = v["agents"].as_array().unwrap().iter().map(|a| a["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![_a3], "beta's workspace lists only beta's agent");

    // Same channel name per workspace is allowed; a duplicate inside one is not.
    let (status, _) = call(&router, "POST", "/v1/channels", Some(&t1), Some(json!({ "name": "general" }))).await;
    assert_eq!(status, StatusCode::CONFLICT, "duplicate name inside one workspace still conflicts");
    let (status, _) = call(&router, "POST", "/v1/channels", Some(&t3), Some(json!({ "name": "eng" }))).await;
    assert_eq!(status, StatusCode::CREATED, "beta creates their own channel");
    let (status, _) = call(&router, "POST", "/v1/channels", Some(&t1), Some(json!({ "name": "eng" }))).await;
    assert_eq!(status, StatusCode::CREATED, "two workspaces may share a channel name");
}

/// Migration backfill: a pre-migration fixture DB (agents, #general, messages,
/// memories, one linked repo) keeps every row inside legacy workspace 1.
#[tokio::test]
async fn migration_backfills_legacy_rows_into_workspace_1() {
    let db_path = format!(
        "/tmp/aghub-ws-backfill-{}-{}.db",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let db = aghub::db::Db::open(&db_path).expect("open fixture db");

    // Build the pre-migration world: schema 0001..0005 only, then legacy rows.
    db.with(|c| {
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL);",
        )?;
        c.execute_batch(include_str!("../migrations/0001_init.sql"))?;
        c.execute_batch(include_str!("../migrations/0002_hardening.sql"))?;
        c.execute_batch(include_str!("../migrations/0003_ui_auth.sql"))?;
        c.execute_batch(include_str!("../migrations/0004_memory.sql"))?;
        c.execute_batch(include_str!("../migrations/0005_github.sql"))?;
        for (v, name) in [
            (1, "0001_init"),
            (2, "0002_hardening"),
            (3, "0003_ui_auth"),
            (4, "0004_memory"),
            (5, "0005_github"),
        ] {
            c.execute(
                "INSERT INTO schema_migrations (version, name) VALUES (?1, ?2)",
                rusqlite::params![v, name],
            )?;
        }
        // Legacy fixture: 2 agents, #general with 2 messages, 2 memories
        // (one private, one channel), 1 linked repo — no workspaces yet.
        c.execute(
            "INSERT INTO agents (id, handle, name, token, created_at)
             VALUES (1, 'sekar', 'Sekar', 'tok-sekar', '2026-01-01T00:00:00Z')",
            [],
        )?;
        c.execute(
            "INSERT INTO agents (id, handle, name, token, created_at)
             VALUES (2, 'fable', 'Fable', 'tok-fable', '2026-01-01T00:00:00Z')",
            [],
        )?;
        c.execute(
            "INSERT INTO channels (id, name, topic, created_at)
             VALUES (1, 'general', 'lobby', '2026-01-01T00:00:00Z')",
            [],
        )?;
        c.execute(
            "INSERT INTO channel_members (channel_id, agent_id, joined_at, role)
             VALUES (1, 1, '2026-01-01T00:00:00Z', 'admin')",
            [],
        )?;
        c.execute(
            "INSERT INTO messages (id, channel_id, sender_id, body, created_at)
             VALUES (1, 1, 1, 'hello world', '2026-01-01T00:00:01Z')",
            [],
        )?;
        c.execute(
            "INSERT INTO messages (id, channel_id, sender_id, body, created_at)
             VALUES (2, 1, 2, 'halo juga', '2026-01-01T00:00:02Z')",
            [],
        )?;
        c.execute(
            "INSERT INTO memories (id, agent_id, channel_id, scope, kind, content, created_at)
             VALUES (1, 1, NULL, 'private', 'preferensi', 'sekar suka kopi', '2026-01-01T00:00:03Z')",
            [],
        )?;
        c.execute(
            "INSERT INTO memories (id, agent_id, channel_id, scope, kind, content, created_at)
             VALUES (2, 1, 1, 'channel', 'fakta', 'general berjalan', '2026-01-01T00:00:04Z')",
            [],
        )?;
        c.execute(
            "INSERT INTO memories_fts (content, memory_id) VALUES ('sekar suka kopi', 1)",
            [],
        )?;
        c.execute(
            "INSERT INTO memories_fts (content, memory_id) VALUES ('general berjalan', 2)",
            [],
        )?;
        c.execute(
            "INSERT INTO github_repos (id, channel_id, owner, name, default_branch, linked_by, created_at, updated_at)
             VALUES (1, 1, 'acme', 'api', 'main', 1, '2026-01-01T00:00:05Z', '2026-01-01T00:00:05Z')",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    // Run the full migration chain: 0006 must backfill without losing rows.
    db.migrate().expect("migrate to 0006");

    let (ws_count, legacy): (i64, (i64, String, Option<i64>)) = db
        .with(|c| {
            Ok((
                c.query_row("SELECT COUNT(*) FROM workspaces", [], |r| r.get(0))?,
                c.query_row(
                    "SELECT id, name, owner_user_id FROM workspaces WHERE id = 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?,
            ))
        })
        .unwrap();
    assert_eq!(ws_count, 1, "exactly the legacy workspace exists");
    assert_eq!(legacy.0, 1);
    assert_eq!(legacy.1, "SIERA Labs");
    assert_eq!(legacy.2, None, "legacy workspace predates human users");

    let (agents_ws, channels_ws, memories_ws): (Vec<i64>, Vec<i64>, Vec<i64>) = db
        .with(|c| {
            Ok((
                c.prepare("SELECT workspace_id FROM agents ORDER BY id")?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                c.prepare("SELECT workspace_id FROM channels ORDER BY id")?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                c.prepare("SELECT workspace_id FROM memories ORDER BY id")?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            ))
        })
        .unwrap();
    assert_eq!(agents_ws, vec![1, 1], "every legacy agent joins workspace 1");
    assert_eq!(channels_ws, vec![1], "the legacy #general joins workspace 1");
    assert_eq!(memories_ws, vec![1, 1], "every legacy memory joins workspace 1");

    // No data loss: ids, contents and links all survive.
    let (msg_count, repo_count, channel_id): (i64, i64, i64) = db
        .with(|c| {
            Ok((
                c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?,
                c.query_row("SELECT COUNT(*) FROM github_repos", [], |r| r.get(0))?,
                c.query_row("SELECT id FROM channels WHERE name = 'general'", [], |r| r.get(0))?,
            ))
        })
        .unwrap();
    assert_eq!(msg_count, 2, "messages survive the channels rebuild");
    assert_eq!(repo_count, 1, "the linked repo survives");
    assert_eq!(channel_id, 1, "channel ids are preserved by the rebuild");

    // Uniqueness moved from global to per-workspace.
    let dup_in_ws1 = db.with(|c| {
        c.execute(
            "INSERT INTO channels (workspace_id, name, topic, created_at) VALUES (1, 'general', NULL, '2026-01-02T00:00:00Z')",
            [],
        )
    });
    assert!(dup_in_ws1.is_err(), "duplicate #general inside one workspace must still be rejected");
    let ws2 = db.with(|c| {
        c.execute(
            "INSERT INTO workspaces (id, owner_user_id, slug, name, created_at) VALUES (2, NULL, 'other', 'Other', '2026-01-02T00:00:00Z')",
            [],
        )
    });
    assert!(ws2.is_ok(), "a second workspace can exist beside the legacy one");
    let dup_in_ws2 = db.with(|c| {
        c.execute(
            "INSERT INTO channels (workspace_id, name, topic, created_at) VALUES (2, 'general', NULL, '2026-01-02T00:00:00Z')",
            [],
        )
    });
    assert!(dup_in_ws2.is_ok(), "another workspace may have its own #general");

    // And the migrated DB serves the API: the legacy agent is isolated.
    let state = aghub::state::AppState::new(db, reqwest::Client::builder().build().unwrap());
    let router = aghub::api::build_router(state);
    let (status, v) = call(&router, "GET", "/v1/agents", Some("tok-sekar"), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let ids: Vec<i64> = v["agents"].as_array().unwrap().iter().map(|a| a["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![1, 2], "sekar sees both legacy agents in workspace 1");
    let (status, v) = call(&router, "GET", "/v1/channels/1/messages", Some("tok-sekar"), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["messages"].as_array().unwrap().len(), 2, "legacy messages readable in place");
    let (status, v) = call(&router, "GET", "/v1/memory?query=kopi", Some("tok-sekar"), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["memories"].as_array().unwrap().len(), 1, "legacy private memory recalls");
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

// --- lens 1: mentions-only wake mode + agent role ---

/// Poll the webhook receiver until a payload matching `pred` arrives.
async fn wait_for_payload(
    received: &Arc<Mutex<Vec<Value>>>,
    what: &str,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    for _ in 0..50 {
        if let Some(p) = received.lock().unwrap().iter().find(|p| pred(p)) {
            return p.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("webhook never received {what}");
}

#[tokio::test]
async fn channel_wake_mentions_only_by_default() {
    let (base, received) = start_webhook_server().await;
    let hook = format!("{base}/run");
    let router = test_app("wake-mentions");
    let (a1, t1) = register(&router, "walpha", Some(&hook)).await;
    let (a2, t2) = add_agent(&router, &t1, "wbeta", Some(&hook)).await;
    let _ = a1;

    let (status, v) = call(
        &router,
        "POST",
        "/v1/channels",
        Some(&t1),
        Some(json!({ "name": "wake", "member_ids": [a2] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(
        v["channel"]["wake_mode"], "mentions",
        "new channels must default to mentions-only"
    );
    let chan = v["channel"]["id"].as_i64().unwrap();

    // (a) a message without an @-mention wakes no webhook and enqueues no run
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t2),
        Some(json!({ "body": "status: build is green" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    assert!(
        received.lock().unwrap().is_empty(),
        "a non-mentioning message must not wake walpha's webhook"
    );
    let (_, v) = call(&router, "GET", "/v1/runs", Some(&t1), None).await;
    assert!(
        v["runs"].as_array().unwrap().is_empty(),
        "no run may be enqueued for a non-mentioning message"
    );

    // ...but the message still reaches the human inbox (polling unchanged)
    let (status, v) = call(&router, "GET", "/v1/inbox", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["message"]["body"] == "status: build is green"),
        "inbox must still show the message"
    );

    // (b) an @-mention wakes the mentioned member's webhook
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t2),
        Some(json!({ "body": "@walpha please review the build" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let p = wait_for_payload(&received, "the @-mention", |p| {
        p["message"]["body"] == "@walpha please review the build"
    })
    .await;
    assert_eq!(p["agent_handle"], "walpha");

    // an explicit wake request also wakes the member, mention or not
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t2),
        Some(json!({ "body": "deploy freeze until further notice", "wake": true })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    wait_for_payload(&received, "the explicit wake request", |p| {
        p["message"]["body"] == "deploy freeze until further notice"
    })
    .await;
}

#[tokio::test]
async fn channel_wake_all_mode_switched_by_admin() {
    let (base, received) = start_webhook_server().await;
    let hook = format!("{base}/run");
    let router = test_app("wake-all");
    let (a1, t1) = register(&router, "calpha", Some(&hook)).await;
    let (a2, t2) = add_agent(&router, &t1, "cbeta", Some(&hook)).await;
    let _ = a1;

    let (_, v) = call(
        &router,
        "POST",
        "/v1/channels",
        Some(&t1),
        Some(json!({ "name": "eng", "member_ids": [a2] })),
    )
    .await;
    let chan = v["channel"]["id"].as_i64().unwrap();

    // a plain member cannot switch the mode
    let (status, _) = call(
        &router,
        "PATCH",
        &format!("/v1/channels/{chan}"),
        Some(&t2),
        Some(json!({ "wake_mode": "all" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "only channel admins may toggle");

    // the admin switches the channel back to waking on every message
    let (status, v) = call(
        &router,
        "PATCH",
        &format!("/v1/channels/{chan}"),
        Some(&t1),
        Some(json!({ "wake_mode": "all" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["channel"]["wake_mode"], "all");

    // a message without any mention now wakes the webhook
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t2),
        Some(json!({ "body": "plain status update" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let p = wait_for_payload(&received, "the all-messages wake", |p| {
        p["message"]["body"] == "plain status update"
    })
    .await;
    assert_eq!(p["agent_handle"], "calpha");

    // unknown modes are rejected
    let (status, _) = call(
        &router,
        "PATCH",
        &format!("/v1/channels/{chan}"),
        Some(&t1),
        Some(json!({ "wake_mode": "sometimes" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // and the mode round-trips through the channel list
    let (_, v) = call(&router, "GET", "/v1/channels", Some(&t1), None).await;
    let ch = v["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"].as_i64() == Some(chan))
        .unwrap()
        .clone();
    assert_eq!(ch["wake_mode"], "all");
}

#[tokio::test]
async fn agent_role_round_trip() {
    let router = test_app("role");
    let (a1, t1) = register(&router, "ralpha", None).await;
    let (a2, t2) = add_agent(&router, &t1, "rbeta", None).await;

    // role starts empty and is part of agent objects
    let (status, v) = call(&router, "GET", "/v1/agents/me", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["agent"]["role"], "", "role is empty until set");

    // the agent sets its own role
    let (status, v) = call(
        &router,
        "PATCH",
        "/v1/agents/me",
        Some(&t1),
        Some(json!({ "role": "mandor" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["agent"]["role"], "mandor");
    let (_, v) = call(&router, "GET", "/v1/agents/me", Some(&t1), None).await;
    assert_eq!(v["agent"]["role"], "mandor", "role must round-trip");

    // the workspace admin (the human's linked agent) sets another agent's role
    let (status, v) = call(
        &router,
        "PATCH",
        &format!("/v1/agents/{a2}"),
        Some(&t1),
        Some(json!({ "role": "QA" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["agent"]["role"], "QA");
    let (_, v) = call(&router, "GET", "/v1/agents", Some(&t1), None).await;
    let beta = v["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"].as_i64() == Some(a2))
        .unwrap()
        .clone();
    assert_eq!(beta["role"], "QA", "directory must return the role");

    // the agent itself may also set its role through the same endpoint
    let (status, v) = call(
        &router,
        "PATCH",
        &format!("/v1/agents/{a2}"),
        Some(&t2),
        Some(json!({ "role": "worker" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["agent"]["role"], "worker");

    // a plain member cannot set someone else's role
    let (status, _) = call(
        &router,
        "PATCH",
        &format!("/v1/agents/{a1}"),
        Some(&t2),
        Some(json!({ "role": "boss" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // roles are free-form but short
    let (status, _) = call(
        &router,
        "PATCH",
        &format!("/v1/agents/{a1}"),
        Some(&t1),
        Some(json!({ "role": "x".repeat(65) })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // an empty string clears the role
    let (status, v) = call(
        &router,
        "PATCH",
        &format!("/v1/agents/{a2}"),
        Some(&t2),
        Some(json!({ "role": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["agent"]["role"], "");
}



// --- Lens 3: terms/trust — listing auth, webhook leaks, token leak, login throttle ---

#[tokio::test]
async fn listings_require_a_bearer_token() {
    let router = test_app("listAuth");
    for uri in ["/v1/agents", "/v1/channels", "/v1/dms", "/v1/inbox", "/v1/notes"] {
        let (status, v) = call(&router, "GET", uri, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} must not be public");
        assert_eq!(v["error"]["code"], "unauthorized", "{uri}: {v}");
    }
    let (status, _) = call(&router, "GET", "/v1/agents", Some("not-a-real-token"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a bogus token is not auth");
}

#[tokio::test]
async fn list_responses_never_expose_webhook_urls() {
    let router = test_app("webhookLeak");
    let (_owner, token) =
        register(&router, "hook-owner", Some("https://hooks.example.com/owner-secret")).await;
    let (_worker, _worker_token) = add_agent(
        &router,
        &token,
        "hook-worker",
        Some("https://hooks.example.com/worker-secret"),
    )
    .await;

    // The cross-agent directory must not carry webhook configuration.
    let (status, v) = call(&router, "GET", "/v1/agents", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let raw = v.to_string();
    assert!(
        !raw.contains("owner-secret") && !raw.contains("worker-secret"),
        "listing leaks a webhook URL: {raw}"
    );
    assert!(!raw.contains("webhook_url"), "listing must omit the webhook_url field: {raw}");
    let agents = v["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 2);
    assert!(agents.iter().all(|a| a.get("webhook_url").is_none()));

    // ...while the owning agent still sees its own webhook on its record.
    let (status, v) = call(&router, "GET", "/v1/agents/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["agent"]["webhook_url"], "https://hooks.example.com/owner-secret");

    // Channel listings have no webhook fields at all.
    let (status, v) = call(&router, "GET", "/v1/channels", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!v.to_string().contains("webhook_url"));
}

#[tokio::test]
async fn ui_me_never_returns_the_agent_token() {
    let router = test_app("meToken");
    let (_id, token) = register(&router, "me-viewer", None).await;
    let (status, _) = call(
        &router,
        "POST",
        "/ui/login",
        None,
        Some(json!({ "email": "me-viewer@example.com", "password": "password123" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "login must succeed");

    // Login sets the session cookie; /ui/api/me is what the browser reads.
    let req = Request::builder()
        .method("POST")
        .uri("/ui/login")
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({ "email": "me-viewer@example.com", "password": "password123" }).to_string(),
        ))
        .unwrap();
    let res = Oneshot::new(router.clone(), req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res
        .headers()
        .get("set-cookie")
        .expect("login sets the session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let req = Request::builder()
        .uri("/ui/api/me")
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap();
    let res = Oneshot::new(router, req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["agent"]["handle"], "me-viewer");
    assert!(
        v["agent"].get("token").is_none(),
        "me must not expose a token field: {raw}"
    );
    assert!(
        !raw.contains(&token),
        "me must not contain the agent's bearer token: {raw}"
    );
}

#[tokio::test]
async fn login_throttle_blocks_one_email_across_ips() {
    let router = test_app("emailThrottle");
    let email = "lens3-target@example.com";
    let bad = json!({ "email": email, "password": "wrongpass" });

    // Five failures, each from a different IP: the per-IP guard never fires,
    // so only the per-email guard can stop the sixth attempt.
    for i in 0..5 {
        let xff = format!("198.51.100.{}", i + 1);
        let (status, _) = call_raw(
            &router,
            "POST",
            "/ui/login",
            None,
            Some(bad.clone()),
            &[("X-Forwarded-For", xff.as_str())],
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "failure {} from a fresh IP is a plain 401",
            i + 1
        );
    }
    let (status, v) = call_raw(
        &router,
        "POST",
        "/ui/login",
        None,
        Some(bad),
        &[("X-Forwarded-For", "198.51.100.99")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "per-email guard must block: {v}"
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("retry after"),
        "429 must carry a retry hint: {v}"
    );

    // A different email from that fresh IP is unaffected.
    let (status, _) = call_raw(
        &router,
        "POST",
        "/ui/login",
        None,
        Some(json!({ "email": "lens3-other@example.com", "password": "wrongpass" })),
        &[("X-Forwarded-For", "198.51.100.99")],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn login_throttle_window_rolls_over_so_lockout_is_temporary() {
    let throttle = aghub::ratelimit::LoginThrottle::new(5, std::time::Duration::from_millis(60));
    let key = "email:owner@example.com";
    for _ in 0..5 {
        throttle.record_failure(key);
    }
    assert!(throttle.blocked_for(key).is_some(), "five failures block");
    assert!(
        throttle.blocked_for("email:other@example.com").is_none(),
        "other keys unaffected"
    );
    tokio::time::sleep(std::time::Duration::from_millis(90)).await;
    assert!(
        throttle.blocked_for(key).is_none(),
        "the block must lift when the window expires — never a permanent lockout"
    );
    throttle.record_failure(key);
    assert!(
        throttle.blocked_for(key).is_none(),
        "a fresh window starts below the limit"
    );
}

#[tokio::test]
async fn reactions_add_remove_and_counts() {
    let router = test_app("reactions");
    let (a1, t1) = register(&router, "alpha", None).await;
    let (a2, t2) = add_agent(&router, &t1, "beta", None).await;

    let (status, v) = call(
        &router,
        "POST",
        "/v1/channels",
        Some(&t1),
        Some(json!({ "name": "eng", "member_ids": [a1, a2] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "channel create failed: {v}");
    let chan = v["channel"]["id"].as_i64().unwrap();
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t1),
        Some(json!({ "body": "deploy is live" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let msg = v["message"]["id"].as_i64().unwrap();
    let react = format!("/v1/messages/{msg}/reactions");

    // Add: 201 the first time, 200 (idempotent) on a repeat.
    let (status, _) = call(&router, "POST", &react, Some(&t2), Some(json!({ "emoji": "ok" }))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(&router, "POST", &react, Some(&t2), Some(json!({ "emoji": "ok" }))).await;
    assert_eq!(status, StatusCode::OK, "re-adding the same reaction is a no-op");
    let (status, _) = call(&router, "POST", &react, Some(&t1), Some(json!({ "emoji": "ok" }))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(&router, "POST", &react, Some(&t1), Some(json!({ "emoji": "seen" }))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = call(&router, "POST", &react, Some(&t2), Some(json!({ "emoji": "banana" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unknown reaction names are rejected");

    // Aggregate counts: 2x ok, 1x seen; `mine` is per viewer.
    let (status, v) = call(&router, "GET", &format!("/v1/channels/{chan}/messages"), Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK);
    let reactions = v["messages"][0]["reactions"].as_array().unwrap();
    let ok = reactions.iter().find(|r| r["emoji"] == "ok").unwrap();
    assert_eq!(ok["count"], 2);
    assert_eq!(ok["mine"], true);
    let seen = reactions.iter().find(|r| r["emoji"] == "seen").unwrap();
    assert_eq!(seen["count"], 1);
    assert_eq!(seen["mine"], false, "alpha's reaction is not beta's");
    assert!(
        v["messages"][0].get("read_by").is_none(),
        "channel messages carry no per-recipient read state"
    );

    // A workspace member outside the channel cannot react.
    let (_a3, t3) = add_agent(&router, &t1, "gamma", None).await;
    let (status, _) = call(&router, "POST", &react, Some(&t3), Some(json!({ "emoji": "ok" }))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Another workspace's agent cannot even see the message.
    let (_a4, t4) = register(&router, "delta", None).await;
    let (status, _) = call(&router, "POST", &react, Some(&t4), Some(json!({ "emoji": "ok" }))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Remove: 204 once, 404 on a repeat.
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("{react}/ok"),
        Some(&t2),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&router, "DELETE", &format!("{react}/ok"), Some(&t2), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, v) = call(&router, "GET", &format!("/v1/channels/{chan}/messages"), Some(&t2), None).await;
    let ok = v["messages"][0]["reactions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["emoji"] == "ok")
        .unwrap()
        .clone();
    assert_eq!(ok["count"], 1);
    assert_eq!(ok["mine"], false);
}

#[tokio::test]
async fn dm_read_receipts_and_reactions_visible_to_sender() {
    let router = test_app("readby");
    let (a1, t1) = register(&router, "alpha", None).await;
    let (a2, t2) = add_agent(&router, &t1, "beta", None).await;

    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/dms/{a2}/messages"),
        Some(&t1),
        Some(json!({ "body": "laporan sudah dikirim" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let msg = v["message"]["id"].as_i64().unwrap();

    // Before the recipient reads it, the sender sees no read_by entries.
    let (status, v) = call(&router, "GET", &format!("/v1/dms/{a2}/messages"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["messages"][0]["read_by"].as_array().unwrap().len(), 0);
    assert_eq!(v["messages"][0]["reactions"].as_array().unwrap().len(), 0);

    // The recipient marks the inbox item read; that is the existing receipt.
    let (status, _) = call(
        &router,
        "POST",
        "/v1/inbox/read",
        Some(&t2),
        Some(json!({ "message_ids": [msg] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, v) = call(&router, "GET", &format!("/v1/dms/{a2}/messages"), Some(&t1), None).await;
    let read_by = v["messages"][0]["read_by"].as_array().unwrap();
    assert_eq!(read_by.len(), 1);
    assert_eq!(read_by[0]["handle"], "beta");
    assert!(!read_by[0]["read_at"].as_str().unwrap().is_empty());

    // The recipient reacts; both sides get the aggregate, `mine` per viewer.
    let react = format!("/v1/messages/{msg}/reactions");
    let (status, _) = call(&router, "POST", &react, Some(&t2), Some(json!({ "emoji": "done" }))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, v) = call(&router, "GET", &format!("/v1/dms/{a2}/messages"), Some(&t1), None).await;
    let done = &v["messages"][0]["reactions"][0];
    assert_eq!(done["emoji"], "done");
    assert_eq!(done["count"], 1);
    assert_eq!(done["mine"], false, "the sender did not react");
    let (_, v) = call(&router, "GET", &format!("/v1/dms/{a1}/messages"), Some(&t2), None).await;
    assert_eq!(v["messages"][0]["reactions"][0]["mine"], true);
}

// --- Replies, edits, and task status on messages ---

#[tokio::test]
async fn message_reply_and_edit_flow() {
    let router = test_app("msg-reply-edit");
    let (a1, t1) = register(&router, "alpha", None).await;
    let (a2, t2) = add_agent(&router, &t1, "beta", None).await;
    let (a3, t3) = add_agent(&router, &t1, "gamma", None).await;
    let (status, v) = call(
        &router,
        "POST",
        "/v1/channels",
        Some(&t1),
        Some(json!({ "name": "eng", "member_ids": [a2, a3] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "channel create failed: {v}");
    let chan = v["channel"]["id"].as_i64().unwrap();

    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t1),
        Some(json!({ "body": "parent body" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let parent = v["message"]["id"].as_i64().unwrap();
    assert!(
        v["message"]["reply_to"].is_null(),
        "a plain message has no reply context"
    );

    // beta replies in the same channel: the response carries the context.
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t2),
        Some(json!({ "body": "a reply", "reply_to_id": parent })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "reply send failed: {v}");
    let reply_id = v["message"]["id"].as_i64().unwrap();
    assert_eq!(v["message"]["reply_to"]["id"].as_i64(), Some(parent));
    assert_eq!(v["message"]["reply_to"]["sender_handle"], "alpha");
    assert_eq!(v["message"]["reply_to"]["body"], "parent body");
    assert!(v["message"]["edited_at"].is_null());
    assert!(v["message"]["task_status"].is_null());

    // list responses include the replied-to message too.
    let (status, v) = call(
        &router,
        "GET",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t3),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"].as_i64() == Some(reply_id))
        .expect("reply must be listed");
    assert_eq!(listed["reply_to"]["id"].as_i64(), Some(parent));
    assert_eq!(listed["reply_to"]["sender_handle"], "alpha");
    assert_eq!(listed["reply_to"]["body"], "parent body");

    // A reply must target an existing message in the same channel.
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t1),
        Some(json!({ "body": "orphan", "reply_to_id": 999_999 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, v) = call(
        &router,
        "POST",
        "/v1/channels",
        Some(&t1),
        Some(json!({ "name": "other", "member_ids": [a1] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let other_chan = v["channel"]["id"].as_i64().unwrap();
    let (_, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{other_chan}/messages"),
        Some(&t1),
        Some(json!({ "body": "elsewhere" })),
    )
    .await;
    let other_msg = v["message"]["id"].as_i64().unwrap();
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t1),
        Some(json!({ "body": "wrong channel", "reply_to_id": other_msg })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "reply across channels must be rejected"
    );

    // DM replies: a channel message is not a valid target, a DM message is.
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/dms/{a1}/messages"),
        Some(&t2),
        Some(json!({ "body": "dm", "reply_to_id": parent })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "reply across channel/DM must be rejected"
    );
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/dms/{a1}/messages"),
        Some(&t2),
        Some(json!({ "body": "hi alpha" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let dm_parent = v["message"]["id"].as_i64().unwrap();
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/dms/{a2}/messages"),
        Some(&t1),
        Some(json!({ "body": "hi beta", "reply_to_id": dm_parent })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "DM reply send failed: {v}");
    assert_eq!(v["message"]["reply_to"]["id"].as_i64(), Some(dm_parent));
    assert_eq!(v["message"]["reply_to"]["sender_handle"], "beta");
    let (status, v) = call(
        &router,
        "GET",
        &format!("/v1/dms/{a1}/messages"),
        Some(&t2),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["reply_to"]["id"].as_i64() == Some(dm_parent)),
        "DM list must carry reply context"
    );

    // Only the sender may edit; edits set edited_at.
    let (status, v) = call(
        &router,
        "PATCH",
        &format!("/v1/messages/{reply_id}"),
        Some(&t2),
        Some(json!({ "body": "a reply (edited)" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "sender edit failed: {v}");
    assert_eq!(v["message"]["body"], "a reply (edited)");
    assert!(
        v["message"]["edited_at"].is_string(),
        "editing must record edited_at"
    );
    assert_eq!(v["message"]["reply_to"]["id"].as_i64(), Some(parent));

    let (status, _) = call(
        &router,
        "PATCH",
        &format!("/v1/messages/{reply_id}"),
        Some(&t1),
        Some(json!({ "body": "hijacked" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-senders must not edit");

    let (status, _) = call(
        &router,
        "PATCH",
        &format!("/v1/messages/{parent}"),
        Some(&t2),
        Some(json!({ "body": "  " })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an empty edit must be rejected"
    );

    let (status, v) = call(
        &router,
        "GET",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t3),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"].as_i64() == Some(reply_id))
        .unwrap();
    assert_eq!(listed["body"], "a reply (edited)");
    assert!(listed["edited_at"].is_string());
}

#[tokio::test]
async fn message_task_status_transitions() {
    let router = test_app("msg-status");
    let (a1, t1) = register(&router, "alpha", None).await;
    let (a2, t2) = add_agent(&router, &t1, "beta", None).await;
    let (a3, t3) = add_agent(&router, &t1, "gamma", None).await;
    let (status, v) = call(
        &router,
        "POST",
        "/v1/channels",
        Some(&t1),
        Some(json!({ "name": "ops", "member_ids": [a2, a3] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "channel create failed: {v}");
    let chan = v["channel"]["id"].as_i64().unwrap();
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t2),
        Some(json!({ "body": "ship the release" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let msg = v["message"]["id"].as_i64().unwrap();

    // claimed -> blocked -> done, each returning the new status.
    for expected in ["claimed", "blocked", "done"] {
        let (status, v) = call(
            &router,
            "POST",
            &format!("/v1/messages/{msg}/status"),
            Some(&t2),
            Some(json!({ "status": expected })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "status {expected} failed: {v}");
        assert_eq!(v["message"]["task_status"], expected);
    }
    let (status, v) = call(
        &router,
        "GET",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t2),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"].as_i64() == Some(msg))
        .unwrap();
    assert_eq!(
        listed["task_status"], "done",
        "list responses carry the status"
    );

    // null clears the task status.
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/messages/{msg}/status"),
        Some(&t2),
        Some(json!({ "status": null })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        v["message"]["task_status"].is_null(),
        "null must clear the status"
    );

    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/messages/{msg}/status"),
        Some(&t2),
        Some(json!({ "status": "shipping" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "unknown status values are rejected"
    );

    // A member who is neither the sender nor a channel admin may not set it.
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/messages/{msg}/status"),
        Some(&t3),
        Some(json!({ "status": "claimed" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The channel admin may set it on someone else's message.
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/messages/{msg}/status"),
        Some(&t1),
        Some(json!({ "status": "blocked" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "channel admin status failed: {v}");
    assert_eq!(v["message"]["task_status"], "blocked");

    let (status, _) = call(
        &router,
        "POST",
        "/v1/messages/999999/status",
        Some(&t2),
        Some(json!({ "status": "done" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // In a DM only the sender can set the status (there is no channel admin).
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/dms/{a1}/messages"),
        Some(&t2),
        Some(json!({ "body": "dm task" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let dm_msg = v["message"]["id"].as_i64().unwrap();
    let (status, _) = call(
        &router,
        "POST",
        &format!("/v1/messages/{dm_msg}/status"),
        Some(&t1),
        Some(json!({ "status": "claimed" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a DM recipient is not the sender"
    );
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/messages/{dm_msg}/status"),
        Some(&t2),
        Some(json!({ "status": "claimed" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["message"]["task_status"], "claimed");
}
