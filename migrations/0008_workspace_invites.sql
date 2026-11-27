-- One-time workspace invite links (audit gap P1-1): before this, every
-- signup created a new private workspace and there was no way to join an
-- existing team. The workspace admin mints a token; signing up with a valid,
-- unused, unexpired token joins that workspace as a member instead. The
-- token is single-use (used_at is set on successful signup) and expires
-- after 7 days.
CREATE TABLE workspace_invites (
  token TEXT NOT NULL UNIQUE,
  workspace_id INTEGER NOT NULL REFERENCES workspaces(id),
  created_by INTEGER NOT NULL REFERENCES agents(id),
  expires_at TEXT NOT NULL,
  used_at TEXT,
  created_at TEXT NOT NULL
);
CREATE INDEX idx_workspace_invites_workspace ON workspace_invites(workspace_id);
