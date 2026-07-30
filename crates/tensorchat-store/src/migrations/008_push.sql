-- Migration 008 — Web Push subscriptions, and a place to keep the VAPID keypair.
--
-- Until now a notification needed an open tab, which for a chat application is
-- most of the point. A push subscription is the browser's promise to wake our
-- service worker even when the site is closed.
--
-- Deliberately *not* storing the subscription's `p256dh` and `auth` keys. Those
-- exist to encrypt a payload, and this server sends none: the push is an empty
-- VAPID-signed poke, and the service worker fetches the actual content from
-- this origin with the session cookie. Message bodies therefore never traverse
-- Google's or Mozilla's infrastructure, and there is one fewer secret at rest.

CREATE TABLE push_subscriptions (
    -- The push service's URL for this browser. Opaque, unique, and the only
    -- thing needed to deliver a payload-less push.
    endpoint   TEXT    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    -- Consecutive delivery failures. A push service that has forgotten a
    -- subscription answers 404 or 410, which prunes the row outright; this
    -- counts the softer failures so a permanently broken endpoint eventually
    -- stops being retried.
    failures   INTEGER NOT NULL DEFAULT 0
) STRICT, WITHOUT ROWID;

-- The only read is "every endpoint for this user", on the delivery path.
CREATE INDEX push_by_user ON push_subscriptions (user_id);

-- Server-wide state that has to survive a restart but is not domain data.
--
-- Currently one row: the VAPID keypair. It must be stable, because the public
-- half is baked into every subscription a browser has already created —
-- regenerating it on each boot would silently invalidate all of them.
CREATE TABLE settings (
    key   TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
) STRICT, WITHOUT ROWID;
