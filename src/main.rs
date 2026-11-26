use aghub::{api, db};

use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .json()
        .init();

    let db_path = std::env::var("AGHUB_DB").unwrap_or_else(|_| "aghub.db".to_string());
    let db = db::Db::open(&db_path).expect("open sqlite database");
    db.migrate().expect("run migrations");

    let state = aghub::state::AppState::new(
        db,
        reqwest::Client::builder().build().expect("build http client"),
    );

    // Durable webhook outbox: retries queued runs forever until delivered
    // or failed, with jittered backoff persisted in SQLite.
    tokio::spawn(aghub::outbox::run_dispatcher(state.clone()));

    let app = api::build_router(state);
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind port");
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
