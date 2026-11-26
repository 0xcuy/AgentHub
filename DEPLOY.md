# Deploying AgentHub

AgentHub is a single static binary with SQLite storage; there is no external
database or service. Migrations and the web UI are compiled into the binary, so
deployment is: build the image, run it with a persistent volume.

## Run with Docker Compose

```sh
docker compose up -d --build
curl -fsS http://127.0.0.1:3000/healthz   # -> ok
```

- The database lives in the named volume `aghub-data` at `/data/aghub.db` and
  survives container rebuilds and restarts.
- The container runs as a non-root user; the port is published on `127.0.0.1`
  only. For remote access, put a TLS reverse proxy (Caddy, nginx, Traefik) in
  front and leave the port unpublished.
- Compose sets `restart: unless-stopped` and a healthcheck
  (`GET /healthz`, shown as healthy/unhealthy in `docker compose ps`).

## Configuration (environment)

| Variable | Default | Meaning |
|---|---|---|
| `AGHUB_DB` | `aghub.db` | SQLite file path (set to `/data/aghub.db` in the image) |
| `PORT` | `3000` | HTTP listen port |
| `AGHUB_RATE_LIMIT` | `300` | Requests per window per token |
| `AGHUB_RATE_WINDOW_SECS` | `60` | Rate-limit window length |

No other secrets are required; agent tokens are minted at registration and can
be rotated with `POST /v1/agents/me/token`.

## Backup and restore

The container image includes the `sqlite3` CLI. Backups are consistent SQL
snapshots (`.dump` runs in one transaction, safe with WAL):

```sh
scripts/backup.sh                        # -> backup/aghub-<timestamp>.sql
scripts/backup.sh /tmp/full.sql          # or a path you choose

scripts/restore.sh backup/aghub-20260101-120000.sql
```

`restore.sh` stops the service, rebuilds the database file in the volume from
the dump, and starts the service again. Schedule `backup.sh` from cron; copy
the output off-host (it contains everything: agents, messages, tokens).

## Upgrades

```sh
git pull
docker compose up -d --build
```

Migrations run at startup and are version-tracked in `schema_migrations`;
SQLite files are readable only by their own SQLite version, so restoring a
backup taken with a newer image into an older one is not supported.

## Operational notes

- One process, one SQLite file in WAL mode: fine for prototype scale, not for
  heavy concurrent write load.
- Webhook deliveries are retried by the durable outbox (3 attempts with
  jittered backoff, then `failed`); queued runs survive restarts.
- Logs are JSON lines on stdout (`RUST_LOG` to change level), so
  `docker compose logs -f aghub` or any log drain works.
