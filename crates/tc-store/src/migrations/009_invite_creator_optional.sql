-- Migration 009 — an invite may have no creator.
--
-- `created_by` was NOT NULL, which assumed every invite is minted by an
-- administrator through the API. The one case that matters most cannot satisfy
-- that: a fresh workspace with registration closed has no accounts at all, so
-- the invite that lets the first person in is minted from the operator console
-- by nobody.
--
-- The alternatives were worse. Pointing it at a synthetic "system" account puts
-- a fake row in the user directory that every mention search and member list has
-- to know to hide. Pointing it at id 0 violates the foreign key, which is the
-- database correctly refusing to store a lie. So the column becomes nullable and
-- means what it says: the administrator who minted this, when one existed.
--
-- SQLite cannot relax NOT NULL in place, so this is the standard rebuild. The
-- table is small — invites are pruned by age — and nothing references it, so
-- there are no dependent foreign keys to reason about.

CREATE TABLE invites_new (
    token_hash BLOB    PRIMARY KEY,
    id         INTEGER NOT NULL,
    label      TEXT    NOT NULL DEFAULT '',
    -- NULL when minted from the operator console on a workspace that had no
    -- administrator to attribute it to.
    created_by INTEGER REFERENCES users (id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    max_uses   INTEGER NOT NULL DEFAULT 0,
    uses       INTEGER NOT NULL DEFAULT 0
) STRICT, WITHOUT ROWID;

INSERT INTO invites_new (token_hash, id, label, created_by, created_at, expires_at, max_uses, uses)
SELECT token_hash, id, label, created_by, created_at, expires_at, max_uses, uses FROM invites;

DROP TABLE invites;
ALTER TABLE invites_new RENAME TO invites;

CREATE UNIQUE INDEX invites_id ON invites (id);
