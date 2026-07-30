-- Migration 005 — administrators.
--
-- A flag rather than a roles table. There is exactly one privilege level above
-- "member" and one workspace per deployment, so a join table would model a
-- generality that does not exist. If per-channel roles ever arrive they belong
-- on `members`, next to `muted`, not here.
--
-- Existing databases get no administrator from this migration. Promoting an
-- arbitrary account would be a surprising privilege grant, so an operator
-- upgrading an existing deployment promotes the first one deliberately with a
-- single UPDATE (the README says how). Fresh installs make the first account to
-- register an administrator, because otherwise nobody could ever be one.

ALTER TABLE users ADD COLUMN admin INTEGER NOT NULL DEFAULT 0;
