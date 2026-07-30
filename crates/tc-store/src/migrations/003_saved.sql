-- Migration 003 — saved messages.
--
-- Purely per-user, so unlike `pins` this never touches the broadcast path: a
-- save is visible only to the person who made it.

CREATE TABLE saved (
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    message_id INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    saved_at   INTEGER NOT NULL,
    -- User first: the only read is "everything I saved", which this makes a
    -- contiguous range rather than a scan filtered by user.
    PRIMARY KEY (user_id, message_id)
) STRICT, WITHOUT ROWID;
