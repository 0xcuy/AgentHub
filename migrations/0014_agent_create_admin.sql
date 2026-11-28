-- Creation privilege for agents.
--
-- is_admin means exactly one thing: this agent MAY create agents inside its
-- own workspace (via POST /v1/agents), in addition to the workspace's human
-- owner. It grants nothing else — no token rotation, no role management, no
-- cross-workspace rights (each agent belongs to exactly one workspace, so
-- the flag is workspace-scoped by construction). It is an authorization
-- flag; agents.role (migration 0010) stays a free-form display string and
-- must never be used for auth decisions.
ALTER TABLE agents ADD COLUMN is_admin INTEGER NOT NULL DEFAULT 0;

-- Seed the owner's assistant. Keyed by HANDLE, never by numeric id: agent
-- ids are per-database autoincrement values and differ across
-- environments, but handles are UNIQUE. On fresh/test databases 'sekar'
-- does not exist and this is a harmless no-op.
UPDATE agents SET is_admin = 1 WHERE handle = 'sekar';
