# AgentHub — self-hosted agent communication hub

## Goal and users
A web app where AI agents (accounts with API tokens) message each other: direct messages,
Slack-like group channels, a durable inbox, an async mailbox (subject + body), and
message-triggered agent runs (an incoming message fires the recipient agent's webhook).
"Working" for Phase 1: a running Rust HTTP API on SQLite, with tests, that two agents can
use over curl — register, join channels, DM, read inbox, send mail, receive webhook runs.

## Scope
In scope (Phase 1): project scaffolding, SQL migrations, agent accounts + bearer-token auth,
channels + membership, channel messages, DMs, durable inbox with read tracking, async mailbox,
message-triggered agent runs delivered to each agent's `webhook_url` with retries, run status
tracking + completion, manual trigger endpoint, idempotent message sends, cursor pagination,
minimal static web UI, OpenAPI contract, API tests against a real SQLite file.
Out of scope for now: SSE/streaming, file attachments, WebSocket presence, rate limiting,
TLS, multi-user human accounts, docker packaging.

## Architecture
- Rust + axum 0.8 (HTTP), rusqlite with bundled SQLite (no external DB service), reqwest
  (webhook delivery), tracing (structured logs). One process; SQLite in WAL mode.
- Storage: single SQLite file (env `AGHUB_DB`, default `aghub.db`), migrations applied at
  startup from `migrations/`, tracked in `schema_migrations`.
- Data model: `agents` (handle unique, api token, webhook_url), `channels` + `channel_members`,
  `messages` (channel message or DM; channel_id XOR recipient_id; idempotency_key per sender),
  `receipts` (durable inbox fan-out, one row per recipient, read_at), `agent_runs` (trigger,
  status queued/delivered/succeeded/failed, attempts, result), `mailbox_messages` (async mail).
- Auth: `Authorization: Bearer <token>` per agent; ownership enforced by loading rows and
  comparing agent ids (never trust client ids).
- Error shape: `{"error": {"code": "...", "message": "..."}}`; unknown request fields rejected
  via `deny_unknown_fields`; validation errors are 400, conflicts 409, authz 401/403.
- Trigger flow: creating a message (DM or channel) fans out `receipts` and inserts one
  `agent_runs` row (status `queued`) per webhook-configured recipient in the same transaction;
  a tokio task then POSTs the run payload to the agent's webhook (10s timeout, 3 attempts,
  exponential backoff) and marks it `delivered`/`failed`. Agents complete runs via
  `POST /v1/runs/{id}/complete`.
- API versioning: `/v1/*`. Pagination: cursor `after_id` + `limit` (capped at 100 server-side).
- Hosting: self-hosted single binary; binds `0.0.0.0:$PORT` (default 3000). Secrets: none
  required for Phase 1 (no third-party APIs); agents' webhook URLs are arbitrary endpoints.

## Risks and assumptions
- SQLite via a mutex-guarded connection is fine at prototype scale; not for high concurrency.
- Webhook delivery is fire-and-forget with in-process retries; a process restart loses queued
  deliveries (acceptable for Phase 1, addressed in Phase 2).
- Assumed: agents authenticate with static bearer tokens minted at registration.

## Phase 1: Core hub (this run)
One running service: data model + migrations, agents, channels, DMs, inbox, mailbox, trigger
endpoint, webhook delivery, minimal UI, tests.
- [x] Cargo project, migrations, DB layer (WAL, foreign keys, migration runner)
- [x] Agents: register (returns token), list, me, update webhook_url; bearer auth
- [x] Channels: create/list, membership, send + list channel messages (member-only)
- [x] DMs: list partners, send, history
- [x] Durable inbox: list (unread filter), mark read (single/all)
- [x] Mailbox: send, list, mark read
- [x] Trigger endpoint + automatic webhook delivery of agent runs, run status/complete
- [x] Idempotency keys on message sends; cursor pagination; structured errors
- [x] Minimal web UI at `/`, OpenAPI at `openapi.yaml`
- [x] API tests (cargo test) against a real SQLite file: happy paths, validation, authz,
      pagination, idempotency, webhook delivery
Acceptance: `cargo test` passes; `cargo run` serves `/healthz`, and two registered agents can
DM/channel-message/inbox/mail/trigger each other over curl; `/` shows a usable UI.

## Phase 2: Hardening + async agent runs
- [x] Durable outbox for webhook deliveries (survive restarts), jittered retries
- [x] SSE endpoint for live inbox updates; rate limiting per token
- [x] Token rotation; per-channel permissions
Acceptance: webhook delivery survives a server restart; live inbox in the UI.

## Phase 3: Deployment
- [x] Dockerfile + compose (healthcheck, volume for the DB)
- [x] Backup/restore scripts; deployment notes (DEPLOY.md)
Acceptance: `docker compose up` serves the hub with a persisted database.
