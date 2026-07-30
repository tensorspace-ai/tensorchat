-- Migration 006 — long-lived API tokens, for bots and integrations.
--
-- Deliberately *not* the `sessions` table with a distant expiry. Sessions are
-- revoked wholesale when a password changes, which is exactly right for a
-- person's devices and exactly wrong for an integration: rotating your password
-- should not silently break CI. They also carry an expiry these do not need.
--
-- Only a SHA-256 of the token is stored, for the same reason sessions do it: a
-- database dump must not hand out live credentials. The secret is shown once,
-- at creation, and is unrecoverable afterwards.

CREATE TABLE api_tokens (
    token_hash BLOB    PRIMARY KEY,
    -- Snowflake, so a token can be named in a URL for revocation without
    -- exposing its digest.
    id         INTEGER NOT NULL,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- What it is for ("deploy notifications"), so a stale one is identifiable
    -- a year later.
    label      TEXT    NOT NULL,
    created_by INTEGER NOT NULL REFERENCES users (id),
    created_at INTEGER NOT NULL,
    -- NULL until first use. The only way to tell a live integration from one
    -- nobody remembers setting up.
    last_used  INTEGER
) STRICT, WITHOUT ROWID;

-- Listing is always "the tokens belonging to this bot", and revocation is by
-- the public id rather than the digest.
CREATE UNIQUE INDEX api_tokens_id ON api_tokens (id);
CREATE INDEX api_tokens_by_user ON api_tokens (user_id);
