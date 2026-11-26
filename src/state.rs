use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: crate::db::Db,
    pub http: reqwest::Client,
    pub events: crate::events::Events,
    pub limiter: crate::ratelimit::RateLimiter,
    /// Wakes the outbox dispatcher as soon as a run is enqueued.
    pub outbox_notify: Arc<tokio::sync::Notify>,
}

impl AppState {
    pub fn new(db: crate::db::Db, http: reqwest::Client) -> Self {
        Self {
            db,
            http,
            events: crate::events::Events::new(),
            limiter: crate::ratelimit::RateLimiter::from_env(),
            outbox_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
}
