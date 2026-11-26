use axum::extract::ConnectInfo;
use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{Extensions, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Per-IP limit for the unauthenticated POST endpoints (agent registration,
/// signup, logout). Applies in addition to the bearer-token limiter.
pub const IP_RATE_LIMIT: usize = 20;
pub const IP_RATE_WINDOW: Duration = Duration::from_secs(60);

/// Brute-force guard for /ui/login: after this many failed logins (401
/// responses) from one IP inside the window, further attempts get a 429
/// until the window rolls over. A successful login clears the slate.
pub const LOGIN_MAX_FAILURES: usize = 5;
pub const LOGIN_FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);

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

/// Counts failed logins per client IP in a fixed window and blocks the IP
/// once the failure count reaches the maximum. Only 401 responses count as
/// failures; a successful login wipes the counter for that IP.
#[derive(Clone)]
pub struct LoginThrottle {
    inner: std::sync::Arc<std::sync::Mutex<HashMap<String, Window>>>,
    max_failures: usize,
    window: Duration,
}

impl LoginThrottle {
    pub fn new(max_failures: usize, window: Duration) -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            max_failures,
            window,
        }
    }

    /// How much longer this IP is locked out, or None when it may try again.
    pub fn blocked_for(&self, ip: &str) -> Option<Duration> {
        let map = self.inner.lock().unwrap();
        let now = Instant::now();
        match map.get(ip) {
            Some(w) if w.count >= self.max_failures as u64 && now < w.reset_at => {
                Some(w.reset_at - now)
            }
            _ => None,
        }
    }

    /// Record a failed login (401) for this IP. The first failure starts a
    /// fresh window; an expired window restarts the count.
    pub fn record_failure(&self, ip: &str) {
        let mut map = self.inner.lock().unwrap();
        let now = Instant::now();
        let entry = map.entry(ip.to_string()).or_insert(Window {
            count: 0,
            reset_at: now + self.window,
        });
        if now >= entry.reset_at {
            entry.count = 0;
            entry.reset_at = now + self.window;
        }
        entry.count += 1;
    }

    /// A successful login resets the failure counter for this IP.
    pub fn reset(&self, ip: &str) {
        self.inner.lock().unwrap().remove(ip);
    }
}

fn bearer_token(headers: &HeaderMap) -> &str {
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("anonymous")
}

/// Client IP for per-IP rate limiting. Behind Traefik the socket peer is the
/// proxy, so trust the first X-Forwarded-For entry (the client the proxy
/// saw) and fall back to the socket address, then to "unknown" when neither
/// is available (in-process tests).
pub fn client_ip(headers: &HeaderMap, extensions: &Extensions) -> String {
    if let Some(first) = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return first.to_string();
    }
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn rate_limited(retry_after: Duration) -> Response {
    let secs = retry_after.as_secs().max(1);
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(axum::http::header::RETRY_AFTER, secs.to_string())],
        axum::Json(serde_json::json!({
            "error": {
                "code": "rate_limited",
                "message": format!("too many requests, retry after {secs}s"),
            }
        })),
    )
        .into_response()
}

pub async fn middleware(
    State(state): State<crate::state::AppState>,
    req: Request,
    next: Next,
) -> Response {
    match state.limiter.check(bearer_token(req.headers())) {
        Ok(()) => next.run(req).await,
        Err(retry_after) => rate_limited(retry_after),
    }
}

/// POST endpoints reachable without any credentials, so they need their own
/// per-IP budget: registering an agent, creating an account, logging out.
fn is_unauthenticated_post(method: &Method, path: &str) -> bool {
    method == Method::POST && matches!(path, "/v1/agents" | "/ui/signup" | "/ui/logout")
}

/// Per-IP limiter for unauthenticated POST endpoints. Keyed by client IP
/// (X-Forwarded-For first entry, socket address fallback) so one host cannot
/// mass-register agents or churn signups. /ui/login is handled separately by
/// the login throttle, which counts failures only.
pub async fn unauth_post_middleware(
    State(state): State<crate::state::AppState>,
    req: Request,
    next: Next,
) -> Response {
    if is_unauthenticated_post(req.method(), req.uri().path()) {
        let ip = client_ip(req.headers(), req.extensions());
        if let Err(retry_after) = state.ip_limiter.check(&ip) {
            return rate_limited(retry_after);
        }
    }
    next.run(req).await
}

/// Brute-force guard for POST /ui/login. Counts only failed attempts (401
/// responses) per IP; once the count reaches the maximum inside the window,
/// every further attempt is answered with 429 + Retry-After until the
/// window rolls over. A successful login clears the counter for that IP.
pub async fn login_throttle_middleware(
    State(state): State<crate::state::AppState>,
    req: Request,
    next: Next,
) -> Response {
    if req.method() != Method::POST || req.uri().path() != "/ui/login" {
        return next.run(req).await;
    }
    let ip = client_ip(req.headers(), req.extensions());
    if let Some(retry_after) = state.login_throttle.blocked_for(&ip) {
        return rate_limited(retry_after);
    }
    let res = next.run(req).await;
    if res.status() == StatusCode::UNAUTHORIZED {
        state.login_throttle.record_failure(&ip);
    } else if res.status().is_success() {
        state.login_throttle.reset(&ip);
    }
    res
}
