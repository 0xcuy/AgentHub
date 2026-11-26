# AgentHub Security Fix-Now (V1–V4) — Security Report

Branch: `vibe-security-fixnow` (from `main` @ fbb6301)
Scope: only the fix-now items V1–V4 from the review list. V5–V7, UI polish and billing were not touched.
Live Traefik config untouched; no deploy/merge/PR. All headers ship in app code, so they behave identically behind Traefik.
Verification was run locally on port 3002 with a scratch SQLite DB (`/tmp/aghub-verify-3002-run2/`), never the live DB or port 3001.

## What changed, per item

### V1 — Security headers on every response
- New `security_headers` axum middleware in `src/api/mod.rs`, wired as the outermost layer in `build_router`, so it applies to HTML pages, static assets, `/v1` API responses, errors and limiter 429s alike.
- Headers set on every response:
  - `Strict-Transport-Security: max-age=31536000; includeSubDomains`
  - `Content-Security-Policy: default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; frame-ancestors 'none'`
    (`'unsafe-inline'` only for script-src/style-src — the pages use inline scripts and `<style>` attributes; verified the UI has no external assets beyond `/fonts` and `/app.css|js`, so nothing else is needed)
  - `X-Frame-Options: DENY` (defense in depth alongside `frame-ancestors 'none'`)
  - `X-Content-Type-Options: nosniff`
  - `Referrer-Policy: strict-origin-when-cross-origin`
- Pages load unchanged afterwards (200 OK on `/`, `/login`, `/signup`, `/app`).

### V2 — Brute-force protection on `/ui/login`
- New `LoginThrottle` (fixed 15-minute window, max 5 failures per client IP) in `src/ratelimit.rs`, exposed as `login_throttle_middleware`.
- Middleware wraps the login handler: **only 401s count as failures**; bad JSON (400/422) does not. Once an IP hits 5 failures inside the window, further login attempts get `429` with a `Retry-After` header (remaining window seconds, max 900). A successful login (2xx) resets that IP's counter.
- Key: client IP (see V3).

### V3 — Per-IP rate limit on unauthenticated POSTs
- New `unauth_post_middleware` applying the existing `RateLimiter` (same fixed-window implementation as the bearer limiter) **keyed by client IP**, limit 20/min: `POST /v1/agents`, `POST /ui/signup` and `POST /ui/logout` (the only other unauthenticated POST found; every other `/v1` POST requires a bearer token). 429 + `Retry-After` when exceeded. `/ui/login` is covered by V2, which counts only failures.
- Client IP resolution (`ratelimit::client_ip`): first `X-Forwarded-For` entry (what Traefik saw), falling back to the socket peer address; `main.rs` now serves with `into_make_service_with_connect_info` so the socket address is available. The IP-keyed limiter is separate from the existing bearer-token limiter, which still applies to `/v1`.
- `AppState` gained `ip_limiter` and `login_throttle` (`src/state.rs`).

### V4 — Secure flag on the session cookie
- `src/api/ui.rs`: `Set-Cookie` for `aghub_session` on signup, login, and the logout clear now include `Secure` in addition to the existing `HttpOnly; SameSite=Lax` (logout keeps `Max-Age=0`). Production is HTTPS-only, so `Secure` cannot strand cookies.

## Files changed
- `src/api/mod.rs` — V1 middleware + layer wiring (outermost layer)
- `src/ratelimit.rs` — `LoginThrottle`, IP-keyed limiters, `client_ip`, shared 429+Retry-After response
- `src/state.rs` — new `ip_limiter`, `login_throttle` state
- `src/api/ui.rs` — V4 `Secure` cookie flag (signup, login, logout)
- `src/main.rs` — serve with `ConnectInfo<SocketAddr>` for the socket-address fallback
- `tests/api.rs` — 4 new regression tests (headers, login lockout, per-IP POST limit, Secure cookie)
- `SECURITY-REPORT.md` — this report

## Test results
- Baseline `cargo test` before changes (on fbb6301): **10 passed, 0 failed**.
- After changes: **14 passed, 0 failed** (10 original + 4 new). Pre-existing 2 unused-variable warnings in existing tests remain, untouched.
- `cargo build --release`: **ok** (Finished `release` [optimized]).

## Live verification (port 3002, scratch DB)

### V1 — headers on every response (`curl -sI`)
Identical header set on all five (`/`, `/login`, `/signup`, `/app`, `/v1/channels`), e.g.:
```
HTTP/1.1 200 OK
strict-transport-security: max-age=31536000; includeSubDomains
content-security-policy: default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; frame-ancestors 'none'
x-frame-options: DENY
x-content-type-options: nosniff
referrer-policy: strict-origin-when-cross-origin
```
Also present on rate-limited responses (see V3 evidence) — nothing bypasses the layer.

### V2 — repeated bad logins (same IP)
```
attempt 1: 401   attempt 2: 401   attempt 3: 401   attempt 4: 401   attempt 5: 401
6th attempt:
HTTP/1.1 429 Too Many Requests
retry-after: 899
```
Success resets the counter — from the same IP (X-Forwarded-For: 203.0.113.10): 3 bad logins (401), one good login (200), then 5 fresh bad logins are all 401 (without the reset it would have locked out mid-run), and only the next one locks again (429, retry-after: 899).

### V3 — POST /v1/agents flood (per IP, 20/min)
```
201 201 201 201 201 201 201 201 201 201 201 201 201 201 201 201 201 201 201 201 429
21st request:
HTTP/1.1 429 Too Many Requests
retry-after: 57
strict-transport-security: max-age=31536000; includeSubDomains
content-security-policy: default-src 'self'; ... frame-ancestors 'none'
x-frame-options: DENY        x-content-type-options: nosniff        referrer-policy: strict-origin-when-cross-origin
```
A request with a different `X-Forwarded-For` (203.0.113.77) still gets 201 while 127.0.0.1 is limited. `/ui/signup` shares the same budget (covered by the unit-test flood).

### V4 — session cookie
```
signup -> set-cookie: aghub_session=79b14…; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=2592000
login  -> set-cookie: aghub_session=5a609…; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=2592000
logout -> set-cookie: aghub_session=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0
```

### Normal flows still work end to end
signup (200 + Secure cookie) → login (200 + Secure cookie) → `GET /ui/api/me` (200, returns agent + token) → `POST /v1/channels` (201) → `POST /v1/channels/{id}/messages` (201) → `GET /v1/channels/{id}/messages` (200, message read back verbatim). All 10 original tests, including SSE streaming, pass unchanged.

## Notes / assumptions
- The X-Forwarded-For entry is trusted as the client IP, per the brief; that is only safe behind the Traefik proxy in front of the service (a direct client could spoof it to rotate keys, same as before this change).
- The login window is fixed (starts at the first failure of the window), so the lockout expires at most 15 minutes after the first counted failure.
- Rate limit constants (5/15min, 20/min) are in-rate-code constants, not env-configurable, unlike the bearer limiter.
