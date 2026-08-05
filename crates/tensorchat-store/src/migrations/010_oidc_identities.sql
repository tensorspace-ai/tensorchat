-- Migration 010 — external identities, for signing in through an OpenID
-- Connect provider.
--
-- Keyed on (issuer, subject), never on an email address. The subject is the one
-- claim OIDC promises is stable and unique within an issuer; an email address
-- is neither. A provider that lets someone set an unverified address, or that
-- releases an address and later hands it to somebody else, would otherwise be a
-- route into an existing account. That is why there is no email column here to
-- match on, rather than a rule saying not to.
--
-- The issuer is half the key because subjects are only unique within one.
-- Numeric subjects starting at 1 are common enough that two providers colliding
-- is a question of when.
--
-- No tokens are stored. The access token is used once, during the callback, to
-- read the subject from the userinfo endpoint and is then dropped; the session
-- that results is an ordinary local session. Keeping a refresh token would mean
-- holding a live credential for somebody else's system in order to offer a
-- feature nothing here asks for.

CREATE TABLE oidc_identities (
    issuer     TEXT    NOT NULL,
    subject    TEXT    NOT NULL,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (issuer, subject)
) STRICT, WITHOUT ROWID;

-- "How does this account sign in?", and the lookup a future unlink would use.
CREATE INDEX oidc_identities_by_user ON oidc_identities (user_id);
