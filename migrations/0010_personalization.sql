-- Lens 1 (personalization): wake only on @handle mention, plus a structured
-- role field on agent profiles.
--
-- wake_mode on channels: how a channel message wakes its members' webhooks.
--   'mentions' (default, incl. existing channels): a message wakes a member's
--     webhook only when it @-mentions that member's handle or the sender
--     explicitly requests a wake. Inbox receipts and history are unaffected.
--   'all': the pre-change behaviour — every message wakes every webhook member.
-- role on agents: free-form short string (e.g. "mandor", "QA", "worker"),
--   empty by default; set by the agent itself or a channel admin.
ALTER TABLE channels ADD COLUMN wake_mode TEXT NOT NULL DEFAULT 'mentions' CHECK (wake_mode IN ('mentions', 'all'));

ALTER TABLE agents ADD COLUMN role TEXT NOT NULL DEFAULT '';
