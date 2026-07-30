-- Migration 007 — invite links, so a closed workspace has a way to grow.
--
-- Before this, `TC_OPEN_REGISTRATION` was the whole story: either anyone with
-- the URL could create an account, or nobody could and an operator had to write
-- rows by hand. Neither is what a small team wants. An invite is the middle
-- setting — registration stays closed, and an administrator hands out a link
-- that admits exactly the people they meant to admit.
--
-- Only a SHA-256 of the token is stored, for the same reason sessions and API
-- tokens do it: a database dump must not yield credentials that still work. The
-- link is shown once, at creation, and cannot be recovered afterwards.

CREATE TABLE invites (
    token_hash BLOB    PRIMARY KEY,
    -- Snowflake, so an invite can be named in a URL for revocation without
    -- exposing the digest of a link that is still live.
    id         INTEGER NOT NULL,
    -- What it is for ("design contractors"), so a stale one is identifiable
    -- months later. Empty is allowed; a link made in thirty seconds should not
    -- require prose.
    label      TEXT    NOT NULL DEFAULT '',
    created_by INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    -- NULL never expires. An expiring link is the safer default the API picks,
    -- but a permanent one is legitimate for a long-running onboarding flow.
    expires_at INTEGER,
    -- 0 is unlimited. Anything else is a hard ceiling enforced inside the same
    -- transaction that creates the account, so two people redeeming the last
    -- seat of a single-use link cannot both win.
    max_uses   INTEGER NOT NULL DEFAULT 0,
    uses       INTEGER NOT NULL DEFAULT 0
) STRICT, WITHOUT ROWID;

-- Listing is "every invite, newest first" for the administration screen, and
-- revocation is by the public id rather than the digest.
CREATE UNIQUE INDEX invites_id ON invites (id);
