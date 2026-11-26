# AgentHub UI polish report

Scope: only `static/index.html`, `static/app.html`, `static/app.js`, `static/app.css`,
`static/login.html`, `static/signup.html` were edited. `src/`, `migrations/`,
`Cargo.toml`, `tests/` and `README.md` are untouched; every `/v1` endpoint and all
backend behavior is unchanged. Design reference: https://aksara.sieralabs.com
(near-black navy canvas, lavender→sky→mint gradient, Space Grotesk + Inter +
Instrument Serif italic accents, glass hairline borders) — the existing Aksara
restyle was refined, not rebuilt.

## What changed

**Landing page (`static/index.html`)**
- Hub diagram cleanup (the main defect): the two floating status chips were
  absolutely positioned at the left/right corners, where they covered the
  `data-sync` / `watchdog` node labels ("posts to #ops" and "watchdog" were
  clipped). The chips are now centered in the empty bands above and below the
  constellation (`top: 1.5%` / `bottom: -1%`, centered), so they never overlap a
  node label at any viewport width; the `@media (max-width: 1100px)` corner
  nudges were removed with them.
- No black blobs: the radial glow behind the core was shrunk (`inset: 26%`) and
  dimmed, the core's hard 50px purple shadow and the pulsing 80px mint glow were
  replaced with soft 26–28px ambient shadows; edges thinned to 1.4px at 38%
  opacity and straight `<line>` spokes became gentle quadratic curves, so the
  constellation reads as small clean nodes with hairline links.
- Hero remains fully visible on load (it renders above the fold, verified in the
  1280×813 screenshot; headline, CTA buttons and all four node labels legible).

**Workspace (`static/app.html`)** — already Slack-like (sidebar with VIEWS /
CHANNELS / DIRECT MESSAGES, header, composer, message stream). One refinement:
the active nav item now has an inset 2px accent bar on its left edge, matching
Slack's active-channel treatment. No structural changes; `app.js` behavior and
all `/ui/*` and `/v1` calls are untouched.

**Login / signup (`static/login.html`, `static/signup.html`)**
- Removed the duplicated in-card brand row: the fixed top-left `agenthub` brand
  plus an identical brand inside the card showed the logo twice on both pages.
- Removed the now-unused `.auth-brand` block. Everything else (Aurora background,
  gridlines, glass card, gradient button, serif-italic headline accents) already
  matched the landing page and was kept, so both pages read as the same product.

## Checks run

- `cargo test`: **10 passed; 0 failed; 0 ignored** (`test result: ok. 10 passed; 0 failed`), plus doctest binary 0 failed.
- `cargo build` after edits (static files are embedded via `include_str!`), server restarted with `AGHUB_DB=/tmp/aghub-ui-verify.db PORT=3000`.
- `curl` route checks on `127.0.0.1:3000`: `/` 200, `/login` 200, `/signup` 200, `/app` 200, `/app.css` 200.
- Headless Chrome (`google-chrome --headless=new`) screenshots at 1280×900: landing, signup, login, workspace, login-error state, plus a 390×844 mobile landing shot. Inspected each: no overlap, no clipped text, no unreadable contrast; landing horizontal overflow at 1280px: 0px.

## End-to-end signup / login verification (real browser, Chrome DevTools Protocol)

1. **Signup:** opened `/signup` in headless Chrome, filled Agent name
   `QA Report Bot`, handle `qa-report-bot`, email `qa+<timestamp>@agenthub.test`,
   password, clicked **Create your hub** → redirected to `/app`. The workspace
   rendered with the identity card showing `QA Report Bot` / `@qa-report-bot`
   and `GET /ui/api/me` returned 200 with the session cookie.
2. **Session persistence:** cleared cookies, then logged in at `/login` with the
   same email/password → redirected to `/app`, identity card shows the same
   agent; `/ui/api/me` 200.
3. **Negative test:** wrong password on `/login` shows the inline alert
   "Invalid email or password" and stays on `/login` (screenshot taken).
4. **Write persistence through the UI:** signed up a second account
   (`Persist Bot`), created channel `ops-alerts` via the New-channel dialog,
   posted "Hello from the QA run — persisted message" through the composer,
   then did a full page reload of `/app`: the session survived (identity card
   still `Persist Bot`), the channel was still in the sidebar, and the message
   was still in the stream after reload — data comes back from SQLite, not
   client state.
5. No agent login flow was built (agents use bearer tokens on `/v1`, as before).

## Known gaps

- None outstanding. The built-in Playwright browser tool was unavailable in this
  build, so local `google-chrome --headless=new` + raw CDP was used instead
  (same assertions: screenshots, navigation, form fills, fetch checks).

---

# Round 2 — FAQ, footer, sign-in redesign, mobile pass (2026-11-26)

Scope unchanged: only `static/index.html`, `static/login.html`, `static/signup.html`
(and one nav link) were edited this round. `src/`, `migrations/`, `Cargo.toml`,
`tests/`, `README.md` still untouched; no `/v1` endpoint or backend behavior
changed. All round-1 polish was kept — same Aksara tokens, type scale, glass and
gradient language.

## What changed (round 2)

**Landing page (`static/index.html`)**
- **New FAQ section** (`#faq`, nav link added): five native `<details>` accordions
  with real answers — what AgentHub is, how agents join (human + first agent at
  signup, more agents via bearer-token `/v1`), how webhooks and runs work
  (message → run queued in SQLite → POST with retries/jittered backoff, inbox
  over SSE), self-hosting (docker compose / cargo run, single SQLite file,
  `scripts/backup.sh`), and pricing: **free and self-hosted, no paid plan**
  (no invented price anywhere).
  Styled in-system: glass cards, gradient-tinted `+` marker that rotates to `×`
  when open, open-state reveal animation (`.3s`, transform/opacity,
  `prefers-reduced-motion` disables it).
- **New footer** replacing the single-line `footer.site`: three-column grid —
  brand column (logo, one-line product description, `hub.sieralabs.com`
  mono chip), Product column (Live demo, Features, FAQ, Self-hosted), Get
  started column (Create your hub, Sign in, GitHub repo ↗ →
  https://github.com/0xcuy/AgentHub), plus a small-print bar
  (© 2026 · "Free and self-hosted — there is no paid plan." · source/docs link).
  Collapses to one column under 820px.

**Sign-in page (`static/login.html`) — redesigned, no bare form**
- Split shell like the landing hero: left showcase column (eyebrow "Welcome
  back", display headline with serif-italic gradient accent "Your agents kept
  *talking*", supporting copy, three checkmark points about durable inboxes /
  retries / one Rust binary, mono status strip `hub.sieralabs.com · one binary ·
  one SQLite file`) and right glass card (gradient hairline on the top edge,
  blurred glass, inline validation, gradient submit button, link to signup).
  Shared aurora + gridline backdrop with the landing. Showcase hides under 940px,
  card centers. Signup page was rebuilt on the identical shell so the two match
  exactly (its own copy and extra webhook field kept). Form ids, `fetch` targets
  (`/ui/login`, `/ui/signup`), JSON payload keys and redirect behavior are
  byte-identical to round 1 — verified by diff.

**Mobile pass (390px)**
- Landing: tightened container gutters (`width: calc(100% - 32px)` at ≤480px),
  section/hero padding reduced, CTAs go full-width with 48px min-height, FAQ
  paddings/type step down, footer small-print stacks. Verified in the browser:
  no horizontal overflow, no clipped text, full-width CTAs, footer single-column,
  FAQ open-state readable at 390×844.

## Checks run (round 2)

- `cargo test` after edits: **10 passed; 0 failed** (`test result: ok. 10 passed; 0 failed`), other binaries 0 failed.
- `cargo build` re-embeds the static files; server re-run with `PORT=3000 AGHUB_DB=/tmp/aghub-ui-verify.db`.
- `curl` on `127.0.0.1:3000`: `/` 200, `/login` 200, `/signup` 200, `/app.css` 200; served HTML contains 5 `.faq` items and the new footer.
- Headless Chrome + raw CDP screenshots (the Playwright tool is still unavailable
  in this build): landing desktop 1280×900, landing 390×844, FAQ desktop + mobile
  (with an open item), footer desktop + mobile, sign-in desktop + 390×844,
  workspace 1280×900. Each inspected: no overlap, clipping, overflow or
  unreadable text.

## End-to-end verification (round 2, real HTTP through the redesigned pages)

1. **Signup:** `POST /ui/signup` with the redesigned form's exact payload
   (`{email, password, handle, name, webhook_url}`) → 200, agent
   `Report Bot` / `@polish-bot-2` created (id 1), session cookie set.
2. **Session + workspace:** `GET /app` with the cookie → 200; `GET /ui/api/me`
   → 200 returning the same agent.
3. **Login:** `POST /ui/logout` (200), then `POST /ui/login` with the same
   credentials → 200 and a fresh cookie; `/ui/api/me` → 200, `GET /app` → 200
   (workspace screenshot shows the signed-in hub for `@polish-bot-2`).
4. No agent login flow was built or changed; agents still use bearer tokens on
   `/v1`.
