-- Workspaces: 1 email = 1 workspace. Every agent, channel and memory belongs
-- to exactly one workspace and nothing may leak across workspaces. Legacy
-- rows (created before human user rows existed) are backfilled into
-- workspace 1 'SIERA Labs' with no owner user.
CREATE TABLE workspaces (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  -- NULL for the legacy workspace: its data predates human user rows. Signups
  -- set it to the owning user's id; deleting that user keeps the workspace.
  owner_user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
  -- URL-safe identity: lowercase name, non-alphanumeric runs become '-',
  -- trimmed, collisions get -2, -3, ... Reserved for a future
  -- per-workspace notebook URL (/<slug>/notebook/) behind a PIN.
  slug TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  created_at TEXT NOT NULL
);

-- Fixed id so the backfills below can point every existing row at it.
INSERT INTO workspaces (id, owner_user_id, slug, name, created_at)
VALUES (1, NULL, 'siera-labs', 'SIERA Labs', strftime('%Y-%m-%dT%H:%M:%SZ', 'now'));

-- Agents and memories: SQLite's ALTER TABLE cannot add a NOT NULL column
-- together with a REFERENCES clause (the default would have to be NULL), so
-- for these two columns the foreign key is enforced in the application layer;
-- DEFAULT 1 performs the backfill.
ALTER TABLE agents ADD COLUMN workspace_id INTEGER NOT NULL DEFAULT 1;
ALTER TABLE memories ADD COLUMN workspace_id INTEGER NOT NULL DEFAULT 1;

-- channels: name uniqueness moves from global to per-workspace
-- UNIQUE(workspace_id, name) so two workspaces may each have a #general.
-- SQLite cannot drop a UNIQUE constraint with ALTER TABLE, so the table is
-- rebuilt, preserving every id.
PRAGMA foreign_keys = OFF;

CREATE TABLE channels_new (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  workspace_id INTEGER NOT NULL REFERENCES workspaces(id),
  name TEXT NOT NULL,
  topic TEXT,
  post_policy TEXT NOT NULL DEFAULT 'members',
  created_at TEXT NOT NULL,
  UNIQUE (workspace_id, name)
);

INSERT INTO channels_new (id, workspace_id, name, topic, post_policy, created_at)
SELECT id, 1, name, topic, post_policy, created_at FROM channels;

DROP TABLE channels;
ALTER TABLE channels_new RENAME TO channels;

PRAGMA foreign_keys = ON;
