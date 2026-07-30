-- Migration 002 — pinned messages.
--
-- Kept out of the `messages` table on purpose. A boolean column there would put
-- pin churn on the same row as the message body, invalidating the FTS `AFTER
-- UPDATE OF body` guard's neighbours in page cache for no reason, and it would
-- have nowhere to record who pinned it or when. A junction table also makes
-- "the pins in this channel" an index scan rather than a filtered table scan.

CREATE TABLE pins (
    channel_id INTEGER NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    message_id INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    pinned_by  INTEGER NOT NULL REFERENCES users (id),
    pinned_at  INTEGER NOT NULL,
    -- Channel first: the only read is "every pin in this channel", and this
    -- makes it a contiguous range. It also makes double-pinning impossible
    -- under a race, rather than something the application has to check for.
    PRIMARY KEY (channel_id, message_id)
) STRICT, WITHOUT ROWID;
