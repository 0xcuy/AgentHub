-- Reactions: lightweight per-message acknowledgements ("seen", "ok", "done"
-- and a few more). One row per (message, agent, reaction name); adding the
-- same reaction twice is a no-op enforced by the unique constraint.
CREATE TABLE reactions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  agent_id INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  emoji TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE (message_id, agent_id, emoji)
);
CREATE INDEX idx_reactions_message ON reactions(message_id, emoji);
