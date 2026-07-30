-- Migration 004 — muted channels.
--
-- A column on `members` rather than its own table: mute is 1:1 with
-- membership, exactly like `last_read` above it, so co-locating keeps "my
-- channels and how I have configured each" a single index scan.

ALTER TABLE members ADD COLUMN muted INTEGER NOT NULL DEFAULT 0;
