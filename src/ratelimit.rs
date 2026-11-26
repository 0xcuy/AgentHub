use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Fixed-window rate limiter keyed by bearer token. Unknown-token and
/// missing-token requests are counted under their own keys so brute-forcing
/// auth is also slowed down.
#[derive(Clone)]
pub struct RateLimiter {
    inner: std::sync::Arc<LimiterInner>,
    pub limit: usize,
    pub window: Duration,
}

struct LimiterInner {
    map: std::sync::Mutex<HashMap<String, Window>>,
}

struct Window {
    count: u64,
    reset_at: Instant,
}

impl RateLimiter {
    pub fn new(limit: usize, window: Duration) -> Self {
        Self {
            inner: std::sync::Arc::new(LimiterInner { map: std::sync::Mutex::new(HashMap::new()) }),
            limit,
            window,
        }
    }

    pub fn from_env() -> Self {
        let limit = std::env::var("AGHUB_RATE_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        let window = Duration::from_secs(
            std::env::var("AGHUB_RATE_WINDOW_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
        );
        Self::new(limit, window)
    }

    /// Ok(()) to allow, or Err(retry_after) when the limit is hit.
    pub fn check(&self, key: &str) -> Result<(), Duration> {
        let mut map = self.inner.map.lock().unwrap();
        let now = Instant::now();
        let entry = map.entry(key.to_string()).or_insert_with(|| Window {
            count: 0,
            reset_at: now + self.window,
        });
        if now >= entry.reset_at {
            entry.count = 0;
            entry.reset_at = now + self.window;
        }
        entry.count += 1;
        if entry.count > self.limit as u64 {
            Err(entry.reset_at - now)
        } else {
            Ok(())
        }
    }
}

fn bearer_token(headers: &axum::http::HeaderMap) -> &str {
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("anonymous")
}

pub async fn middleware(
    State(state): State<crate::state::AppState>,
    req: Request,
    next: Next,
) -> Response {
    match state.limiter.check(bearer_token(req.headers())) {
        Ok(()) => next.run(req).await,
        Err(retry_after) => {
            let secs = retry_after.as_secs().max(1);
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("Retry-After", secs.to_string())],
                axum::Json(serde_json::json!({
                    "error": {
                        "code": "rate_limited",
                        "message": format!("too many requests, retry after {secs}s"),
                    }
                })),
            )
                .into_response()
        }
    }
}
