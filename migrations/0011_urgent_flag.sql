-- Urgency: channel messages can be flagged urgent so that polling agents
-- (and the urgent-forwarder worker) can surface them immediately instead
-- of waiting for someone to open the app. 0 = normal, 1 = urgent.
ALTER TABLE messages ADD COLUMN urgent INTEGER NOT NULL DEFAULT 0;
