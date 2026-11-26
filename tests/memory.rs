//! Integration tests for the memory feature: CRUD, FTS5 recall ranking,
//! privacy, per-agent cap, channel visibility, and the optional vector
//! layer via a stub provider.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tower::util::Oneshot;

type RouterT = axum::Router;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn test_state(name: &str) -> aghub::state::AppState {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let db = aghub::db::Db::open(&format!(
        "/tmp/aghub-mem-test-{name}-{n}-{}.db",
        std::process::id()
    ))
    .expect("open test db");
    db.migrate().expect("migrate");
    let mut state = aghub::state::AppState::new(
        db,
        reqwest::Client::builder().build().unwrap(),
    );
    // Hermetic: never let an ambient OG_API_KEY from the developer shell
    // turn the vector layer on inside tests.
    state.embeddings = Arc::new(aghub::embeddings::NoopEmbeddings);
    state
}

fn test_app_with_state(name: &str) -> (RouterT, aghub::state::AppState) {
    let state = test_state(name);
    (aghub::api::build_router(state.clone()), state)
}

fn test_app(name: &str) -> RouterT {
    test_app_with_state(name).0
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
        serde_json::from_slice(&bytes).unwrap_or(Value::String(
            String::from_utf8_lossy(&bytes).into_owned(),
        ))
    };
    (status, v)
}

async fn register(router: &RouterT, handle: &str) -> (i64, String) {
    let (status, v) = call(
        router,
        "POST",
        "/v1/agents",
        None,
        Some(json!({ "name": handle, "handle": handle })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
    (v["agent"]["id"].as_i64().unwrap(), v["token"].as_str().unwrap().to_string())
}

async fn make_channel(
    router: &RouterT,
    token: &str,
    name: &str,
    member_ids: Vec<i64>,
) -> i64 {
    let (status, v) = call(
        router,
        "POST",
        "/v1/channels",
        Some(token),
        Some(json!({ "name": name, "member_ids": member_ids })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "channel create failed: {v}");
    v["channel"]["id"].as_i64().unwrap()
}

async fn remember(
    router: &RouterT,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    call(router, "POST", "/v1/memory", Some(token), Some(body)).await
}

// --- CRUD + source fields ---

#[tokio::test]
async fn memory_crud_and_source_fields() {
    let router = test_app("crud");
    let (_a, t1) = register(&router, "alpha").await;
    let chan = make_channel(&router, &t1, "general", vec![]).await;
    let (status, v) = call(
        &router,
        "POST",
        &format!("/v1/channels/{chan}/messages"),
        Some(&t1),
        Some(json!({ "body": "kita pakai rust untuk server" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let msg_id = v["message"]["id"].as_i64().unwrap();

    let (status, v) = remember(
        &router,
        &t1,
        json!({
            "content": "Server AgentHub ditulis dalam Rust",
            "kind": "keputusan",
            "tags": ["tech", "stack"],
            "source_message_id": msg_id,
            "source_channel_id": chan
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let m = &v["memory"];
    assert_eq!(m["kind"], "keputusan");
    assert_eq!(m["scope"], "private");
    assert!(m["channel_id"].is_null());
    assert_eq!(m["source_message_id"], msg_id, "source message must be stored");
    assert_eq!(m["source_channel_id"], chan, "source channel must be stored");
    assert_eq!(m["agent_handle"], "alpha");
    assert_eq!(m["tags"], json!(["stack", "tech"]), "tags are stored sorted");
    let mem_id = m["id"].as_i64().unwrap();

    // visible in a plain listing, newest first
    let (status, v) = call(&router, "GET", "/v1/memory", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["memories"].as_array().unwrap().len(), 1);
    assert_eq!(v["memories"][0]["id"], mem_id);

    let (status, v) = call(&router, "DELETE", &format!("/v1/memory/{mem_id}"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (_, v) = call(&router, "GET", "/v1/memory", Some(&t1), None).await;
    assert_eq!(v["memories"].as_array().unwrap().len(), 0);
    // deleting twice -> 404
    let (status, _) = call(&router, "DELETE", &format!("/v1/memory/{mem_id}"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // validation
    let (status, _) = remember(&router, &t1, json!({ "content": "x", "kind": "salah" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = remember(&router, &t1, json!({ "content": "   ", "kind": "fakta" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = remember(&router, &t1, json!({ "content": "x", "kind": "fakta", "scope": "channel" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "scope=channel without channel_id must fail");
    let (status, _) = remember(&router, &t1, json!({ "content": "x", "kind": "fakta", "channel_id": 1 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "channel_id with scope=private must fail");
    let (status, _) = remember(&router, &t1, json!({ "content": "x", "kind": "fakta", "source_message_id": 99 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unknown source_message_id must fail");
    let (status, _) = call(&router, "GET", "/v1/memory?query=%22%22", Some(&t1), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "query of only quotes must be rejected");
}

// --- privacy: another agent must never see private memories ---

#[tokio::test]
async fn memory_privacy() {
    let router = test_app("privacy");
    let (_a, t1) = register(&router, "alpha").await;
    let (_b, t2) = register(&router, "beta").await;

    let (status, v) = remember(
        &router,
        &t1,
        json!({ "content": "rahasia nenek alpha: resep rendang pineapple surabaya", "kind": "fakta" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let secret_id = v["memory"]["id"].as_i64().unwrap();

    let (status, _) = remember(&router, &t2, json!({ "content": "beta suka pineapple pizza", "kind": "preferensi" })).await;
    assert_eq!(status, StatusCode::CREATED);

    // beta searches for alpha's private memory content: nothing
    let (status, v) = call(&router, "GET", "/v1/memory?query=rendang+pineapple", Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK);
    for m in v["memories"].as_array().unwrap() {
        assert_ne!(m["agent_id"], serde_json::json!(_a), "beta must not see alpha's private memory");
    }
    assert!(!v["memories"].as_array().unwrap().iter().any(|m| m["id"] == secret_id));

    // beta cannot delete alpha's private memory, and gets no hint it exists
    let (status, _) = call(&router, "DELETE", &format!("/v1/memory/{secret_id}"), Some(&t2), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // alpha still sees and deletes their own memory
    let (status, v) = call(&router, "GET", "/v1/memory?query=rendang", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["memories"].as_array().unwrap().len(), 1);
    let (status, _) = call(&router, "DELETE", &format!("/v1/memory/{secret_id}"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
}

// --- channel memories: members only ---

#[tokio::test]
async fn memory_channel_visibility() {
    let router = test_app("channel");
    let (a1, t1) = register(&router, "alpha").await;
    let (a2, t2) = register(&router, "beta").await;
    let (a3, t3) = register(&router, "gamma").await;

    let proj = make_channel(&router, &t1, "proj", vec![a1, a2]).await;
    let other = make_channel(&router, &t2, "other", vec![a2, a3]).await;

    // alpha writes a shared summary into "proj"
    let (status, v) = remember(
        &router,
        &t1,
        json!({
            "content": "Ringkasan channel proj: kami memutuskan migrasi ke SQLite FTS5",
            "kind": "keputusan",
            "scope": "channel",
            "channel_id": proj,
            "source_channel_id": proj
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    // beta writes into "other"
    let (status, v) = remember(
        &router,
        &t2,
        json!({
            "content": "Ringkasan channel other: catatan rahasia tim beta-gamma",
            "kind": "fakta",
            "scope": "channel",
            "channel_id": other,
            "source_channel_id": other
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let other_mem_id = v["memory"]["id"].as_i64().unwrap();

    // beta is a member of both channels and sees both summaries
    let (status, v) = call(&router, "GET", "/v1/memory?query=ringkasan+channel", Some(&t2), None).await;
    assert_eq!(status, StatusCode::OK);
    let items = v["memories"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{v}");

    // alpha sees only "proj", never "other" (not a member)
    let (status, v) = call(&router, "GET", "/v1/memory?query=ringkasan", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    let items = v["memories"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["channel_id"], proj);
    assert!(items.iter().all(|m| m["id"] != other_mem_id));

    // gamma (member of "other") can delete beta's channel memory
    let (status, _) = call(&router, "DELETE", &format!("/v1/memory/{other_mem_id}"), Some(&t3), None).await;
    assert_eq!(status, StatusCode::OK, "channel member must be able to delete a channel memory");

    // alpha cannot POST a channel memory into "other" (not a member)
    let (status, _) = remember(
        &router,
        &t1,
        json!({ "content": "suntik", "kind": "fakta", "scope": "channel", "channel_id": other }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // scope/channel_id filters
    let (status, v) = call(&router, "GET", &format!("/v1/memory?scope=channel&channel_id={proj}"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["memories"].as_array().unwrap().len(), 1);
    let (status, v) = call(&router, "GET", "/v1/memory?scope=private", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["memories"].as_array().unwrap().len(), 0, "alpha owns no private memories here");

    // non-member filtering by a foreign channel_id returns nothing, not 403
    let (status, v) = call(&router, "GET", &format!("/v1/memory?channel_id={other}"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["memories"].as_array().unwrap().len(), 0);

    // invalid channel id on POST: 404
    let (status, _) = remember(
        &router,
        &t1,
        json!({ "content": "x", "kind": "fakta", "scope": "channel", "channel_id": 9999 }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --- FTS5 ranking ---

#[tokio::test]
async fn memory_fts_ranking_and_filters() {
    let router = test_app("fts");
    let (_a, t1) = register(&router, "alpha").await;
    remember(&router, &t1, json!({ "content": "kopi tubruk adalah minuman pagi alpha", "kind": "fakta" })).await;
    remember(&router, &t1, json!({ "content": "kopi kopi kopi kopi kopi pagi", "kind": "preferensi" })).await;
    remember(&router, &t1, json!({ "content": "teh hijau menyehatkan", "kind": "fakta" })).await;

    let (status, v) = call(&router, "GET", "/v1/memory?query=kopi", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    let items = v["memories"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{v}");
    // density: the "kopi kopi kopi" memory must outrank the single mention
    assert!(items[0]["content"].as_str().unwrap().contains("kopi kopi"), "higher term density must rank first: {v}");
    assert_eq!(items[0]["score"].as_f64().unwrap(), 1.0, "top fts hit is normalized to 1.0");
    assert!(items[1]["score"].as_f64().unwrap() < 1.0);
    assert_eq!(v["vector_search"], false, "vector search must be off without a provider key");

    // limit
    let (_, v) = call(&router, "GET", "/v1/memory?query=kopi&limit=1", Some(&t1), None).await;
    assert_eq!(v["memories"].as_array().unwrap().len(), 1);

    // kind filter excludes the preferensi memo
    let (status, v) = call(&router, "GET", "/v1/memory?query=kopi&kind=fakta", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    let items = v["memories"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["kind"], "fakta");

    // prefix matching: "kop" finds kopi
    let (status, v) = call(&router, "GET", "/v1/memory?query=kop", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["memories"].as_array().unwrap().len(), 2, "prefix query must match");

    // no hit
    let (status, v) = call(&router, "GET", "/v1/memory?query=supra", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["memories"].as_array().unwrap().len(), 0);

    // invalid kind filter
    let (status, _) = call(&router, "GET", "/v1/memory?kind=bogus", Some(&t1), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // auth required
    let (status, _) = call(&router, "GET", "/v1/memory?query=kopi", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// --- per-agent cap (AGHUB_MEMORY_CAP_PER_AGENT; default 500) ---

#[tokio::test]
async fn memory_cap_per_agent() {
    let mut state = test_state("cap");
    state.memory_cap = 2;
    let router = aghub::api::build_router(state.clone());

    // env parsing: same helper main() uses for AGHUB_MEMORY_CAP_PER_AGENT
    assert_eq!(aghub::state::parse_memory_cap("7").unwrap(), 7);
    assert_eq!(aghub::state::parse_memory_cap(" 42 ").unwrap(), 42);
    assert!(aghub::state::parse_memory_cap("x").is_err());
    assert!(aghub::state::parse_memory_cap("0").is_err());
    assert_eq!(aghub::state::DEFAULT_MEMORY_CAP, 500);

    let (_a, t1) = register(&router, "alpha").await;
    let (_b, t2) = register(&router, "beta").await;
    let chan = make_channel(&router, &t1, "capped", vec![]).await;

    let (status, _) = remember(&router, &t1, json!({ "content": "m1", "kind": "fakta" })).await;
    assert_eq!(status, StatusCode::CREATED);
    // channel memories count toward the OWNER's cap too
    let (status, _) = remember(
        &router,
        &t1,
        json!({ "content": "m2", "kind": "fakta", "scope": "channel", "channel_id": chan, "source_channel_id": chan }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, v) = remember(&router, &t1, json!({ "content": "m3", "kind": "fakta" })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"]["code"], "memory_cap_reached");
    assert!(v["error"]["message"].as_str().unwrap().contains("2"), "error must mention the cap: {v}");

    // the cap is per agent: beta has its own budget
    let (status, _) = remember(&router, &t2, json!({ "content": "b1", "kind": "fakta" })).await;
    assert_eq!(status, StatusCode::CREATED);

    // deleting frees budget
    let (_, v) = call(&router, "GET", "/v1/memory", Some(&t1), None).await;
    let m2_id = v["memories"].as_array().unwrap().iter()
        .find(|m| m["content"] == "m2").unwrap()["id"].as_i64().unwrap();
    let (status, _) = call(&router, "DELETE", &format!("/v1/memory/{m2_id}"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = remember(&router, &t1, json!({ "content": "m3", "kind": "fakta" })).await;
    assert_eq!(status, StatusCode::CREATED, "deleted memory must free cap budget");
}

// --- vector layer: stub provider, and off-by-default without a key ---

#[tokio::test]
async fn memory_vector_layer_with_stub_and_off_without_key() {
    // default: no env key -> Noop -> vector search must be off
    let (router, state) = test_app_with_state("vec");
    assert!(!state.embeddings.enabled(), "without a key the provider must be disabled");
    assert_eq!(state.embeddings.name(), "noop");

    let (_a, t1) = register(&router, "alpha").await;
    let (status, v) = remember(&router, &t1, json!({ "content": "alpha suka kopi hitam", "kind": "preferensi" })).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let _mem_id = v["memory"]["id"].as_i64().unwrap();
    // no embedding was stored
    let stored: i64 = state
        .db
        .with(|c| c.query_row("SELECT COUNT(*) FROM memory_embeddings", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(stored, 0, "disabled provider must not store embeddings");
    let (_, v) = call(&router, "GET", "/v1/memory?query=kopi", Some(&t1), None).await;
    assert_eq!(v["vector_search"], false);

    // with the stub provider the vector layer turns on
    let mut state2 = aghub::state::AppState::new(
        state.db.clone(),
        reqwest::Client::builder().build().unwrap(),
    );
    state2.embeddings = Arc::new(aghub::embeddings::StubEmbeddings { dim: 64 });
    assert!(state2.embeddings.enabled());
    let router2 = aghub::api::build_router(state2.clone());

    let (status, v) = remember(&router2, &t1, json!({ "content": "alpha suka kopi pagi enak", "kind": "fakta" })).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let vec_id = v["memory"]["id"].as_i64().unwrap();
    // a second embedded memory that shares only SOME query terms: FTS (AND
    // semantics) cannot match it, the vector layer can
    let (status, v) = remember(&router2, &t1, json!({ "content": "alpha suka teh tarik pagi", "kind": "fakta" })).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let stored: i64 = state2
        .db
        .with(|c| c.query_row("SELECT COUNT(*) FROM memory_embeddings", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(stored, 2, "stub provider must store embeddings");
    let (dim,): (i64,) = state2
        .db
        .with(|c| {
            c.query_row(
                "SELECT dim FROM memory_embeddings WHERE memory_id = ?1",
                [vec_id],
                |r| Ok((r.get(0)?,)),
            )
        })
        .unwrap();
    assert_eq!(dim, 64, "dimension is flexible and comes from the provider");

    // "suka kopi pagi": full FTS match for the kopi memory (FTS + vector),
    // no FTS match but nonzero cosine for the teh memory (vector only).
    let (status, v) = call(&router2, "GET", "/v1/memory?query=suka+kopi+pagi", Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["vector_search"], true, "vector layer must be reported active");
    let items = v["memories"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{v}");
    assert!(
        items[0]["content"].as_str().unwrap().contains("kopi"),
        "full FTS match plus vector agreement must rank first: {v}"
    );
    assert!(items[0]["score"].as_f64().unwrap() > 0.8, "{v}");
    assert!(
        items[1]["content"].as_str().unwrap().contains("teh"),
        "a memory with no FTS match must still surface via vector-only similarity: {v}"
    );
    assert!(items[1]["score"].as_f64().unwrap() > 0.0 && items[1]["score"].as_f64().unwrap() < items[0]["score"].as_f64().unwrap());

    // deleting a memory removes its embedding and its FTS row
    let (status, _) = call(&router2, "DELETE", &format!("/v1/memory/{vec_id}"), Some(&t1), None).await;
    assert_eq!(status, StatusCode::OK);
    let stored: i64 = state2
        .db
        .with(|c| c.query_row("SELECT COUNT(*) FROM memory_embeddings", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(stored, 1, "cascade must remove the deleted memory's embedding");
    let fts_left: i64 = state2
        .db
        .with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memory_id = ?1",
                [vec_id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(fts_left, 0, "delete must remove the FTS row too");
}
