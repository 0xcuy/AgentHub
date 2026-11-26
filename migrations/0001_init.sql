CREATE TABLE agents (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  handle TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  token TEXT NOT NULL UNIQUE,
  webhook_url TEXT,
  created_at TEXT NOT NULL
);

CREATE TABLE channels (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL UNIQUE,
  topic TEXT,
  created_at TEXT NOT NULL
);

CREATE TABLE channel_members (
  channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  agent_id INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  joined_at TEXT NOT NULL,
  UNIQUE (channel_id, agent_id)
);
CREATE INDEX idx_channel_members_agent ON channel_members(agent_id);

CREATE TABLE messages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  channel_id INTEGER REFERENCES channels(id) ON DELETE CASCADE,
  sender_id INTEGER NOT NULL REFERENCES agents(id),
  recipient_id INTEGER REFERENCES agents(id),
  body TEXT NOT NULL,
  idempotency_key TEXT,
  created_at TEXT NOT NULL
);
CREATE INDEX idx_messages_channel ON messages(channel_id, id);
CREATE INDEX idx_messages_dm ON messages(sender_id, recipient_id, id);
CREATE UNIQUE INDEX idx_messages_idem ON messages(sender_id, idempotency_key) WHERE idempotency_key IS NOT NULL;

CREATE TABLE receipts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  agent_id INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  read_at TEXT,
  UNIQUE (message_id, agent_id)
);
CREATE INDEX idx_receipts_agent ON receipts(agent_id, read_at);

CREATE TABLE agent_runs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  agent_id INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  message_id INTEGER REFERENCES messages(id) ON DELETE SET NULL,
  trigger_type TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'queued',
  attempts INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  result TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX idx_runs_agent ON agent_runs(agent_id, id);

CREATE TABLE mailbox_messages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  sender_id INTEGER NOT NULL REFERENCES agents(id),
  recipient_id INTEGER NOT NULL REFERENCES agents(id),
  subject TEXT NOT NULL,
  body TEXT NOT NULL,
  read_at TEXT,
  created_at TEXT NOT NULL
);
CREATE INDEX idx_mail_recipient ON mailbox_messages(recipient_id, id);
CREATE INDEX idx_mail_sender ON mailbox_messages(sender_id, id);
