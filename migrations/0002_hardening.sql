-- Phase 2: durable outbox scheduling for webhook deliveries
ALTER TABLE agent_runs ADD COLUMN next_attempt_at TEXT;
CREATE INDEX idx_runs_due ON agent_runs(status, next_attempt_at);

-- Phase 2: per-channel permissions
ALTER TABLE channel_members ADD COLUMN role TEXT NOT NULL DEFAULT 'member';
-- Legacy channels: every existing member becomes an admin so pre-Phase-2
-- behaviour (any member may add members) is preserved after upgrade.
UPDATE channel_members SET role = 'admin';

ALTER TABLE channels ADD COLUMN post_policy TEXT NOT NULL DEFAULT 'members';
