-- TensorChat schema (version 5).
--
-- Conventions:
--   * Every `id` is a Snowflake (see tc-core::id). Because they are monotonic,
--     `INTEGER PRIMARY KEY` gives us insertion at the right edge of the B-tree
--     (no page splits), free chronological ordering, and a pagination cursor —
--     without a single secondary index on a timestamp column.
--   * `STRICT` everywhere: SQLite's default type affinity will happily store a
--     string in an integer column. Strict tables turn that into an error at
--     write time instead of a mystery at read time.
--   * `WITHOUT ROWID` on narrow junction tables: the row *is* the key, so the
--     extra rowid indirection is pure overhead in both space and lookups.

CREATE TABLE users (
    id            INTEGER PRIMARY KEY,
    handle        TEXT    NOT NULL,
    display_name  TEXT    NOT NULL,
    status        TEXT    NOT NULL DEFAULT '',
    -- Argon2id PHC string. Never leaves the store layer.
    password_hash TEXT    NOT NULL,
    bot           INTEGER NOT NULL DEFAULT 0,
    deactivated   INTEGER NOT NULL DEFAULT 0,
    -- A flag, not a roles table: there is one privilege level above "member"
    -- and one workspace per deployment. Per-channel roles, if they ever exist,
    -- belong on `members` rather than here.
    admin         INTEGER NOT NULL DEFAULT 0
) STRICT;

-- Handles are the @mention namespace, so uniqueness is a correctness
-- requirement, not a nicety.
CREATE UNIQUE INDEX users_handle ON users (handle);

-- Opaque bearer tokens. We store only a SHA-256 of the token, so a database
-- leak does not hand out live sessions.
CREATE TABLE sessions (
    token_hash BLOB    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE INDEX sessions_by_user ON sessions (user_id);

CREATE TABLE channels (
    id           INTEGER PRIMARY KEY,
    -- 0 public, 1 private, 2 dm, 3 group. Mirrors tc_core::ChannelKind.
    kind         INTEGER NOT NULL,
    name         TEXT    NOT NULL DEFAULT '',
    topic        TEXT    NOT NULL DEFAULT '',
    created_by   INTEGER NOT NULL REFERENCES users (id),
    created_at   INTEGER NOT NULL,
    archived     INTEGER NOT NULL DEFAULT 0,
    -- Denormalized newest message id. Sorting the sidebar and computing
    -- "has anything happened here" are the two most frequent reads in the
    -- product; without this they would each be a correlated subquery over the
    -- messages table.
    last_message INTEGER NOT NULL DEFAULT 0,
    -- Sorted, comma-joined member ids for DMs and group DMs. Turns "open a DM
    -- with these people" into a unique-index probe instead of a set-equality
    -- query over the members table.
    dm_key       TEXT
) STRICT;

-- Named channels share one namespace; DMs have no name and are excluded.
CREATE UNIQUE INDEX channels_name ON channels (name) WHERE name <> '';
CREATE UNIQUE INDEX channels_dm_key ON channels (dm_key) WHERE dm_key IS NOT NULL;

CREATE TABLE members (
    channel_id INTEGER NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    joined_at  INTEGER NOT NULL,
    -- Read cursor lives here rather than in its own table: it is 1:1 with
    -- membership, and co-locating it means "my channels + where I am in each"
    -- is one index scan instead of a join.
    last_read  INTEGER NOT NULL DEFAULT 0,
    -- Muted: suppress this channel's unread badge. Also 1:1 with membership,
    -- so it lives here for the same reason as the read cursor.
    muted      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (channel_id, user_id)
) STRICT, WITHOUT ROWID;

-- The login path ("which channels am I in?") reads by user, the opposite
-- order from the primary key.
CREATE INDEX members_by_user ON members (user_id, channel_id);

CREATE TABLE messages (
    id          INTEGER PRIMARY KEY,
    channel_id  INTEGER NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    author_id   INTEGER NOT NULL REFERENCES users (id),
    body        TEXT    NOT NULL,
    -- NULL for top-level messages; otherwise the thread's root message id.
    thread_root INTEGER,
    reply_count INTEGER NOT NULL DEFAULT 0,
    edited_at   INTEGER,
    -- Soft delete: the row survives so thread structure, reply counts and
    -- pagination cursors stay stable. The body is blanked in place.
    deleted     INTEGER NOT NULL DEFAULT 0,
    -- Packed little-endian u64 array of mentioned user ids. Denormalized so
    -- rendering history never needs a per-message join; the `mentions` table
    -- below exists for the *counting* direction.
    mentions    BLOB
) STRICT;

-- The channel backfill query is `WHERE channel_id = ? AND id < ? ORDER BY id
-- DESC` — this index serves it as a pure descending range scan.
CREATE INDEX messages_channel ON messages (channel_id, id DESC);
-- Partial: only replies carry a thread_root, and threads are a minority of
-- messages, so the index stays small enough to live in page cache.
CREATE INDEX messages_thread ON messages (thread_root, id) WHERE thread_root IS NOT NULL;

-- Mention counting, the other direction from messages.mentions. Ordered
-- (user, channel, message) so an unread-mention badge is a counted range scan
-- from the user's read cursor.
CREATE TABLE mentions (
    user_id    INTEGER NOT NULL,
    channel_id INTEGER NOT NULL,
    message_id INTEGER NOT NULL,
    PRIMARY KEY (user_id, channel_id, message_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE reactions (
    message_id INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    emoji      TEXT    NOT NULL,
    PRIMARY KEY (message_id, user_id, emoji)
) STRICT, WITHOUT ROWID;

-- Pinned messages. Kept out of the `messages` table on purpose: a boolean
-- column there would put pin churn on the same row as the message body, and it
-- would have nowhere to record who pinned it or when.
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

-- Saved ("starred") messages. Purely per-user, so unlike `pins` this never
-- touches the broadcast path.
CREATE TABLE saved (
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    message_id INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    saved_at   INTEGER NOT NULL,
    -- User first: the only read is "everything I saved", which this makes a
    -- contiguous range rather than a scan filtered by user.
    PRIMARY KEY (user_id, message_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE attachments (
    id         INTEGER PRIMARY KEY,
    -- NULL while the upload is staged but not yet posted. Orphans are
    -- reapable by age.
    message_id INTEGER REFERENCES messages (id) ON DELETE CASCADE,
    owner_id   INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name       TEXT    NOT NULL,
    mime       TEXT    NOT NULL,
    size       INTEGER NOT NULL,
    width      INTEGER,
    height     INTEGER,
    -- Path relative to the configured blob root. Bytes live on the filesystem,
    -- not in SQLite: streaming a file to a socket should not go through the
    -- database's page cache.
    path       TEXT    NOT NULL
) STRICT;

CREATE INDEX attachments_message ON attachments (message_id) WHERE message_id IS NOT NULL;

-- Full-text search over message bodies.
--
-- `content=messages` makes this an *external content* index: FTS5 stores only
-- the inverted index and reads columns back from `messages` on demand, instead
-- of keeping a second copy of every message body. Roughly halves the on-disk
-- cost of search.
CREATE VIRTUAL TABLE messages_fts USING fts5 (
    body,
    content = 'messages',
    content_rowid = 'id',
    tokenize = "unicode61 remove_diacritics 2"
);

-- External-content tables are not maintained automatically; these triggers are
-- the contract FTS5 expects.
CREATE TRIGGER messages_fts_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts (rowid, body) VALUES (new.id, new.body);
END;

CREATE TRIGGER messages_fts_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;

-- `OF body` so that reaction/reply-count churn does not touch the index.
-- Soft deletes blank the body, which correctly drops the row from search.
CREATE TRIGGER messages_fts_au AFTER UPDATE OF body ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
    INSERT INTO messages_fts (rowid, body) VALUES (new.id, new.body);
END;
