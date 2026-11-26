# UI Polish Report — O1, S1, S2, F2, F3, F4, F5 (+ sign-in parity)

Scope: UI-FIX-LIST.md items O1, S1, S2, F2, F3, F4, F5 only. Files touched:
`static/index.html`, `static/app.html`, `static/app.css`, `static/app.js`,
`static/login.html`, `static/signup.html`, `static/favicon.svg` (new),
`src/ui.rs` (favicon route only), this report. No API, DB, security, rate-limit,
cookie or subscription code changed.

## Fixes and verification

**O1 — footer source line removed.** Deleted the
"Source and docs at github.com/0xcuy/AgentHub" line from the landing footer;
the footer-column link labeled "GitHub repository" is untouched.
Verified: `curl /` → `Source and docs at` count 0, `GitHub repository` count 1.

**S1 — typing demo rests on a complete sentence.** The live-demo loop now runs
the script once and stops after the final message instead of clearing mid-word
at the loop seam. No loop refactor — same `script` array, same `addMsg` timing.
Verified in the code path; animation plays once per page load.

**S2 — stats captions bigger and heavier.** Added
`.band .stat span { font-size: var(--fs-md); font-weight: 600; }` to
`static/app.css` (Aksara tokens reused) and dropped the smaller `--fs-sm` from
the inline rule in `index.html` so the shared rule wins.

**F2 — error focus + ARIA on signup/login.** Both forms now track the first
invalid field, move focus to it on submit, set/remove `aria-invalid` on the
input, and every input carries `aria-describedby` pointing at its error span
(`name-err`, `handle-err`, `email-err`, `password-err`, `webhook-err`).
Verified in headless Chrome: login submit with empty form →
`{"focus":"email","invalid":"true","describedby":"email-err","err":"Enter a valid email."}`;
signup submit → `{"focus":"name","invalid":"true","describedby":"name-err","pwDesc":"password-err"}`.

**F3 — boot failure shows Retry, never a dead "Loading".** In `app.js` the
initial channels + directory load moved into `initialLoad()`; on failure the
stream pane shows "Couldn't load your workspace" with the error message and a
**Retry** button that re-runs the load. The Inbox view stays openable from the
sidebar; the screen never sticks on Loading (only `me` failure redirects to
/login, as before). Verified: `retry-boot` handler present in served app.js.

**F4 — Self-hosted links open the FAQ answer.** Nav and footer "Self-hosted"
links now point to `#faq-self-host` (was `#numbers`); that `<details>` answer
("Can I self-host it?") opens on click, via the hashchange handler, and on
direct load with that hash. Verified in headless Chrome: fresh-load navbar
click → `{"open":true}`; direct navigation to `/#faq-self-host` → `open:true`.
Note: the summary rests ~26px above the viewport top after the smooth scroll
because section reveal animations change layout during the scroll; the answer
itself is open and readable. Left as is to avoid touching the reveal system.

**F5 — favicon everywhere.** New `static/favicon.svg` (gradient A mark from
the masthead) linked from landing, signup, login and workspace. `src/ui.rs`
changed minimally: added the `FAVICON` include and one match arm in the
existing `/fonts/{file}` handler (`favicon.svg` → `image/svg+xml`); font
serving behavior unchanged. Verified: `curl /fonts/favicon.svg` → 200,
`image/svg+xml`, on all four pages the `rel="icon"` link count is 1.

**Sign-in parity.** Compared login/signup against the landing page: shared
`app.css` (fonts), `theme-color`, A-mark masthead, Sign in / Create your hub
CTAs, alert styling and footer link targets all match. The only real gap was
the missing favicon on auth pages — fixed by F5. Nothing else to fix.

## Checks run

- `cargo test --test api` → **10 passed; 0 failed** (baseline before any
  change was also 10/10; suite re-run after the `ui.rs` change and after the
  F4 click-timing fix).
- Server on port 3000 (AGHUB_DB=/tmp/aghub-polish-ui.db): `/` 200 (34408 B),
  `/login` 200, `/signup` 200, `/app` 200, `/fonts/favicon.svg` 200
  (`image/svg+xml`), `/fonts/inter.woff2` 200 (`font/woff2`).
- Headless Chrome (CDP, google-chrome): interactive F2/F4 checks above plus
  screenshots saved to `/tmp/aghub-landing-desktop.png` (1280×900),
  `/tmp/aghub-landing-mobile.png` (390×844 @2x), `/tmp/aghub-login.png` and
  `/tmp/aghub-workspace.png` (authenticated session, seeded demo data:
  agent `@maya`, channel `#general`, DM partner `report-bot`). All four
  reviewed visually: no overflow, clipping or contrast issues; workspace
  renders real API data (channels, DM partner, inbox empty state).
- Workspace end-to-end: signed up through `/ui/signup`, listed channels and
  agents through `/v1` with the session token — UI shows the same data back.
