use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: crate::db::Db,
    pub http: reqwest::Client,
    pub events: crate::events::Events,
    pub limiter: crate::ratelimit::RateLimiter,
    /// Per-IP budget for unauthenticated POSTs (agent registration, signup).
    pub ip_limiter: crate::ratelimit::RateLimiter,
    /// Failed-login counter per IP for /ui/login brute-force protection.
    pub login_throttle: crate::ratelimit::LoginThrottle,
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
            ip_limiter: crate::ratelimit::RateLimiter::new(
                crate::ratelimit::IP_RATE_LIMIT,
                crate::ratelimit::IP_RATE_WINDOW,
            ),
            login_throttle: crate::ratelimit::LoginThrottle::new(
                crate::ratelimit::LOGIN_MAX_FAILURES,
                crate::ratelimit::LOGIN_FAILURE_WINDOW,
            ),
            outbox_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
}
