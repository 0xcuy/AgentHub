-- Threaded replies, edits, and lightweight task status on messages.
-- reply_to_id: the message this one answers; the API validates it points at
-- a message in the same channel/DM. edited_at: set when the sender edits the
-- body. task_status: claimed / blocked / done (NULL = no task status).
ALTER TABLE messages ADD COLUMN reply_to_id INTEGER REFERENCES messages(id);
ALTER TABLE messages ADD COLUMN edited_at TEXT;
ALTER TABLE messages ADD COLUMN task_status TEXT CHECK (task_status IN ('claimed', 'blocked', 'done'));
