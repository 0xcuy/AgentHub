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
    /// Max memories owned by one agent (private + channel combined);
    /// AGHUB_MEMORY_CAP_PER_AGENT, default 500.
    pub memory_cap: usize,
    /// Embedding provider for the optional vector layer. Noop (disabled)
    /// unless a key is configured via the environment.
    pub embeddings: Arc<dyn crate::embeddings::EmbeddingProvider>,
}

pub const DEFAULT_MEMORY_CAP: usize = 500;

pub fn parse_memory_cap(raw: &str) -> Result<usize, String> {
    let n: usize = raw
        .trim()
        .parse()
        .map_err(|_| format!("invalid AGHUB_MEMORY_CAP_PER_AGENT: {raw:?}"))?;
    if n == 0 {
        return Err("AGHUB_MEMORY_CAP_PER_AGENT must be >= 1".into());
    }
    Ok(n)
}

pub fn memory_cap_from_env() -> usize {
    std::env::var("AGHUB_MEMORY_CAP_PER_AGENT")
        .ok()
        .and_then(|raw| match parse_memory_cap(&raw) {
            Ok(n) => Some(n),
            Err(e) => {
                tracing::warn!(error = %e, "falling back to default memory cap");
                None
            }
        })
        .unwrap_or(DEFAULT_MEMORY_CAP)
}

impl AppState {
    pub fn new(db: crate::db::Db, http: reqwest::Client) -> Self {
        let cap = memory_cap_from_env();
        let embeddings = crate::embeddings::from_env(http.clone());
        tracing::info!(cap, provider = embeddings.name(), "memory enabled");
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
            memory_cap: cap,
            embeddings,
        }
    }
}
