#!/usr/bin/env bash
# Restore the AgentHub SQLite database from a backup made by scripts/backup.sh.
# Usage: scripts/restore.sh <backup-file.sql>
set -euo pipefail
cd "$(dirname "$0")/.."

file="${1:?usage: scripts/restore.sh <backup-file.sql>}"
[[ -f "$file" ]] || { echo "no such file: $file" >&2; exit 1; }
abs="$(realpath "$file")"

docker compose stop aghub
# Recreate the DB file from the SQL dump, wiping any stale WAL sidecars.
docker compose run --rm --entrypoint "" \
    -v "$abs:/backup.sql:ro" aghub sh -c \
    'rm -f /data/aghub.db /data/aghub.db-wal /data/aghub.db-shm && sqlite3 /data/aghub.db < /backup.sql && echo "restore complete"'
docker compose start aghub
docker compose ps
