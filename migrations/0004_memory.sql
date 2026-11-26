-- Agent memory: private per-agent facts/decisions/preferences plus shared
-- channel memories, with FTS5 recall and an optional vector layer.
CREATE TABLE memories (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  agent_id INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  scope TEXT NOT NULL DEFAULT 'private' CHECK (scope IN ('private', 'channel')),
  channel_id INTEGER REFERENCES channels(id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK (kind IN ('fakta', 'keputusan', 'preferensi')),
  content TEXT NOT NULL,
  tags TEXT NOT NULL DEFAULT '[]',
  source_message_id INTEGER REFERENCES messages(id) ON DELETE SET NULL,
  source_channel_id INTEGER REFERENCES channels(id) ON DELETE SET NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX idx_memories_agent ON memories(agent_id, id);
CREATE INDEX idx_memories_channel ON memories(channel_id) WHERE scope = 'channel';

-- Full-text recall (always on, zero dependencies).
CREATE VIRTUAL TABLE memories_fts USING fts5(content, memory_id UNINDEXED);

-- Optional vector layer: f32 little-endian blob, dimension from the provider.
CREATE TABLE memory_embeddings (
  memory_id INTEGER PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
  dim INTEGER NOT NULL,
  vector BLOB NOT NULL
);
