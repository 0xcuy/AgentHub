#!/usr/bin/env bash
# Backup the AgentHub SQLite database from the running compose stack.
# Usage: scripts/backup.sh [output-file.sql]
set -euo pipefail
cd "$(dirname "$0")/.."

out="${1:-backup/aghub-$(date +%Y%m%d-%H%M%S).sql}"
mkdir -p "$(dirname "$out")"

# .dump runs in a single transaction, so the dump is a consistent snapshot
# even with WAL mode active. It streams plain SQL over stdout.
docker compose exec -T aghub sqlite3 /data/aghub.db .dump > "$out"
echo "backup written to $out"
