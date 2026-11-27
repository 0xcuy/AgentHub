-- Notebook: private per-owner notes. The owner column is agents.id, which
-- covers both agent tokens and humans (each user row links 1:1 to an agent),
-- so the existing require_agent middleware scopes every row to one identity.
CREATE TABLE notes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  owner INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  title TEXT,
  content TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX idx_notes_owner_updated ON notes(owner, updated_at);
