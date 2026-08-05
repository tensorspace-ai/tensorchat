//! Accounts and sessions.

use rusqlite::{OptionalExtension, Row, TransactionBehavior, params};
use tensorchat_core::text::MAX_HANDLE_LEN;
use tensorchat_core::{Id, User};

use crate::{Error, Result, Store, from_sql, to_sql};

/// Columns every `User` mapping selects, in the order [`map_user`] expects.
const USER_COLS: &str = "id, handle, display_name, status, bot, deactivated, admin";
/// The same list, table-qualified, for queries that join.
const USER_COLS_Q: &str = "u.id, u.handle, u.display_name, u.status, u.bot, u.deactivated, u.admin";

/// What [`Store::user_for_oidc_identity`] did.
#[derive(Debug)]
pub enum OidcLogin {
    /// The identity was already linked to this account.
    Existing(User),
    /// First sight of this identity; the account was created for it.
    Created(User),
    /// The identity belongs to an account somebody deactivated.
    ///
    /// Its own variant rather than an `Existing` carrying the flag, and rather
    /// than folding into "not found", because both of those fail quietly. A
    /// caller that forgets the flag signs the person in; a lookup that filtered
    /// deactivated rows out — the way `user_for_login` does — would fall
    /// through to provisioning and hand them a brand new account, which makes
    /// deactivation one sign-in away from being undone.
    Deactivated,
}

/// The first free handle at or after `base`: `base`, then `base2`, `base3`, …
///
/// Two people called `alice` at the provider, or one whose provider name is
/// already somebody else's account here, both have to end up with somewhere to
/// live. Numbering keeps the handle recognizable, which a random suffix would
/// not.
///
/// Runs inside the caller's transaction, so a name found free is still free
/// when the insert lands.
fn free_handle(tx: &rusqlite::Transaction<'_>, base: &str) -> Result<String> {
    let mut taken = tx.prepare_cached("SELECT EXISTS (SELECT 1 FROM users WHERE handle = ?)")?;
    for n in 0..MAX_HANDLE_ATTEMPTS {
        let candidate = numbered_handle(base, n);
        if !taken.query_row([&candidate], |r| r.get::<_, bool>(0))? {
            return Ok(candidate);
        }
    }
    Err(Error::Conflict("that handle"))
}

/// How many numbered variants to try before giving up. Reaching this means a
/// thousand accounts share a stem, which is a stuck provider rather than a
/// coincidence worth looping over.
const MAX_HANDLE_ATTEMPTS: u32 = 1000;

/// `base` with `n` appended, shortened so the result still fits.
///
/// The stem is truncated rather than the number dropped, and trailing
/// separators are trimmed afterwards: `alice-` is not a handle the rules accept,
/// so slicing a long name at exactly the wrong byte would produce a row the
/// application would refuse to have created itself.
fn numbered_handle(base: &str, n: u32) -> String {
    if n == 0 {
        return base.to_string();
    }
    let suffix = (n + 1).to_string();
    let room = MAX_HANDLE_LEN.saturating_sub(suffix.len());
    // Handles are ASCII by `validate_handle`, so this cannot split a character.
    let stem = base[..base.len().min(room)].trim_end_matches(['.', '-', '_']);
    format!("{stem}{suffix}")
}

fn map_user(row: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: from_sql(row.get(0)?),
        handle: row.get(1)?,
        display_name: row.get(2)?,
        status: row.get(3)?,
        bot: row.get(4)?,
        deactivated: row.get(5)?,
        admin: row.get(6)?,
    })
}

impl Store {
    /// Register an account. `password_hash` must already be an Argon2id PHC
    /// string — this layer never sees a plaintext password.
    ///
    /// The first *person* to register becomes the administrator. Somebody has
    /// to be, and every other bootstrapping route is worse: a setup token to
    /// mislay, a config flag to forget, or a workspace nobody can administer.
    ///
    /// Bots are excluded from that count deliberately. They cannot sign in, so
    /// a bot created before the first human would otherwise consume the
    /// bootstrap slot and leave the workspace with no administrator and no way
    /// to appoint one.
    pub fn create_user(
        &self,
        id: Id,
        handle: &str,
        display_name: &str,
        password_hash: &str,
    ) -> Result<User> {
        let mut conn = self.conn()?;
        // IMMEDIATE: "is this the first user?" is a read that decides a write,
        // so two simultaneous registrations must not both see an empty table
        // and both claim the admin flag.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let first: bool = tx
            .prepare_cached("SELECT NOT EXISTS (SELECT 1 FROM users WHERE bot = 0)")?
            .query_row([], |r| r.get(0))?;

        tx.prepare_cached(
            "INSERT INTO users (id, handle, display_name, password_hash, admin) \
             VALUES (?, ?, ?, ?, ?)",
        )?
        .execute(params![
            to_sql(id),
            handle,
            display_name,
            password_hash,
            first
        ])
        .map_err(|e| Error::from_sqlite(e, "that handle"))?;
        tx.commit()?;

        Ok(User {
            id,
            handle: handle.to_string(),
            display_name: display_name.to_string(),
            status: String::new(),
            bot: false,
            deactivated: false,
            admin: first,
        })
    }

    /// Resolve an external identity to an account, creating one the first time
    /// that identity is seen.
    ///
    /// The lookup, the handle deconfliction and both inserts share one
    /// `IMMEDIATE` transaction. Two tabs finishing a first sign-in at the same
    /// moment would otherwise both see no identity, both provision an account,
    /// and the loser would discover the primary key only after its user row was
    /// already written.
    ///
    /// `handle_hint` must already satisfy the handle rules — see
    /// `text::handle_from_external`. This layer only makes it unique.
    pub fn user_for_oidc_identity(
        &self,
        issuer: &str,
        subject: &str,
        new_id: Id,
        handle_hint: &str,
        display_name: &str,
        now_ms: u64,
    ) -> Result<OidcLogin> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let existing = tx
            .prepare_cached(&format!(
                "SELECT {USER_COLS_Q} FROM oidc_identities i JOIN users u ON u.id = i.user_id \
                 WHERE i.issuer = ? AND i.subject = ?"
            ))?
            .query_row(params![issuer, subject], map_user)
            .optional()?;
        if let Some(user) = existing {
            return Ok(if user.deactivated {
                OidcLogin::Deactivated
            } else {
                OidcLogin::Existing(user)
            });
        }

        let handle = free_handle(&tx, handle_hint)?;

        // The same bootstrap rule `create_user` applies: the first person
        // through the door administers the workspace, however they arrived.
        let first: bool = tx
            .prepare_cached("SELECT NOT EXISTS (SELECT 1 FROM users WHERE bot = 0)")?
            .query_row([], |r| r.get(0))?;

        // `'!'` is the sentinel bots use: a value no Argon2 hash can equal, so
        // `verify_password` against it fails for every input. An account that
        // arrived through a provider has no local password by construction
        // rather than because some check remembers to say so.
        tx.prepare_cached(
            "INSERT INTO users (id, handle, display_name, password_hash, admin) \
             VALUES (?, ?, ?, '!', ?)",
        )?
        .execute(params![to_sql(new_id), handle, display_name, first])
        .map_err(|e| Error::from_sqlite(e, "that handle"))?;

        tx.prepare_cached(
            "INSERT INTO oidc_identities (issuer, subject, user_id, created_at) \
             VALUES (?, ?, ?, ?)",
        )?
        .execute(params![issuer, subject, to_sql(new_id), now_ms as i64])
        .map_err(|e| Error::from_sqlite(e, "that identity"))?;
        tx.commit()?;

        Ok(OidcLogin::Created(User {
            id: new_id,
            handle,
            display_name: display_name.to_string(),
            status: String::new(),
            bot: false,
            deactivated: false,
            admin: first,
        }))
    }

    /// How many people (not bots) have accounts.
    ///
    /// Zero is the state a fresh install is in, and the one worth reporting at
    /// startup when registration is closed: nobody can sign in, and nobody can
    /// mint the invite that would let them.
    pub fn human_count(&self) -> Result<u32> {
        let conn = self.conn()?;
        Ok(conn
            .prepare_cached("SELECT count(*) FROM users WHERE bot = 0")?
            .query_row([], |r| r.get::<_, i64>(0))? as u32)
    }

    /// Resolve a handle to its account, *including* deactivated ones.
    ///
    /// Deliberately unlike [`Store::user_for_login`] and
    /// [`Store::ids_for_handles`], which both hide deactivated accounts so that
    /// deactivation is undetectable from outside. This is for the operator
    /// console, where the whole point is to act on an account that may well be
    /// deactivated — and where "no such user" and "that user is switched off"
    /// need to be different answers.
    pub fn user_by_handle(&self, handle: &str) -> Result<Option<User>> {
        let conn = self.conn()?;
        Ok(conn
            .prepare_cached(&format!("SELECT {USER_COLS} FROM users WHERE handle = ?"))?
            .query_row([handle], map_user)
            .optional()?)
    }

    /// How many administrators the workspace has.
    ///
    /// Used to refuse the last one's own demotion or deactivation — a
    /// workspace with no administrator cannot recover through the API.
    pub fn admin_count(&self) -> Result<u32> {
        let conn = self.conn()?;
        Ok(conn
            .prepare_cached("SELECT count(*) FROM users WHERE admin = 1 AND deactivated = 0")?
            .query_row([], |r| r.get::<_, i64>(0))? as u32)
    }

    /// Grant or revoke administrator. Returns the updated user.
    pub fn set_admin(&self, id: Id, admin: bool) -> Result<User> {
        let conn = self.conn()?;
        let n = conn
            .prepare_cached("UPDATE users SET admin = ? WHERE id = ?")?
            .execute(params![admin, to_sql(id)])?;
        if n == 0 {
            return Err(Error::NotFound);
        }
        drop(conn);
        self.user(id)
    }

    /// Deactivate or reactivate an account. Returns the updated user.
    ///
    /// Deactivation is reversible and keeps the row, so authorship, mentions
    /// and thread structure all stay intact. Deleting the account outright
    /// would cascade its messages away and rewrite other people's history.
    pub fn set_deactivated(&self, id: Id, deactivated: bool) -> Result<User> {
        let conn = self.conn()?;
        let n = conn
            .prepare_cached("UPDATE users SET deactivated = ? WHERE id = ?")?
            .execute(params![deactivated, to_sql(id)])?;
        if n == 0 {
            return Err(Error::NotFound);
        }
        drop(conn);
        self.user(id)
    }

    /// Fetch a user together with their password hash, for login.
    ///
    /// Returns `Ok(None)` for an unknown handle *or a deactivated account*, so
    /// the caller can still run a dummy verification and keep login timing —
    /// and its answer — independent of which of the two it was. Filtering here
    /// rather than after the password check is what keeps deactivation from
    /// being detectable by response timing.
    pub fn user_for_login(&self, handle: &str) -> Result<Option<(User, String)>> {
        let conn = self.conn()?;
        let row = conn
            .prepare_cached(&format!(
                "SELECT {USER_COLS}, password_hash FROM users \
                 WHERE handle = ? AND deactivated = 0"
            ))?
            .query_row([handle], |r| Ok((map_user(r)?, r.get::<_, String>(7)?)))
            .optional()?;
        Ok(row)
    }

    pub fn user(&self, id: Id) -> Result<User> {
        let conn = self.conn()?;
        conn.prepare_cached(&format!("SELECT {USER_COLS} FROM users WHERE id = ?"))?
            .query_row([to_sql(id)], map_user)
            .optional()?
            .ok_or(Error::NotFound)
    }

    /// Every account in the workspace.
    ///
    /// The client needs the full directory to render authors and resolve
    /// mentions; shipping it once at connect beats N lookups later. A
    /// multi-thousand-seat deployment would page this.
    pub fn all_users(&self) -> Result<Vec<User>> {
        let conn = self.conn()?;
        let mut stmt =
            conn.prepare_cached(&format!("SELECT {USER_COLS} FROM users ORDER BY id"))?;
        let rows = stmt.query_map([], map_user)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Update the mutable parts of a profile. Returns the new state.
    pub fn update_profile(&self, id: Id, display_name: &str, status: &str) -> Result<User> {
        let conn = self.conn()?;
        let n = conn
            .prepare_cached("UPDATE users SET display_name = ?, status = ? WHERE id = ?")?
            .execute(params![display_name, status, to_sql(id)])?;
        if n == 0 {
            return Err(Error::NotFound);
        }
        drop(conn);
        self.user(id)
    }

    /// The stored password hash for one account, for re-authenticating an
    /// already-signed-in user before a sensitive change.
    pub fn password_hash(&self, id: Id) -> Result<String> {
        let conn = self.conn()?;
        conn.prepare_cached("SELECT password_hash FROM users WHERE id = ?")?
            .query_row([to_sql(id)], |r| r.get(0))
            .optional()?
            .ok_or(Error::NotFound)
    }

    /// Replace a password hash. The caller has already verified the current
    /// password; this layer never sees plaintext.
    pub fn update_password(&self, id: Id, password_hash: &str) -> Result<()> {
        let conn = self.conn()?;
        let n = conn
            .prepare_cached("UPDATE users SET password_hash = ? WHERE id = ?")?
            .execute(params![password_hash, to_sql(id)])?;
        if n == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Revoke every session belonging to a user, optionally sparing one.
    ///
    /// `keep` is the digest of the caller's own token, so changing a password
    /// signs out every other device without signing out the tab doing the
    /// changing. Returns how many sessions were revoked.
    pub fn delete_sessions_for_user(&self, user: Id, keep: Option<&[u8]>) -> Result<usize> {
        let conn = self.conn()?;
        // One statement for both cases: a NULL `keep` never equals a token
        // hash, so the comparison drops out rather than needing a second query.
        Ok(conn
            .prepare_cached(
                "DELETE FROM sessions WHERE user_id = ? \
                 AND (? IS NULL OR token_hash <> ?)",
            )?
            .execute(params![to_sql(user), keep, keep])?)
    }

    /// Record a session. `token_hash` is a SHA-256 of the bearer token; the
    /// token itself is never stored, so a database dump cannot be replayed.
    pub fn create_session(
        &self,
        token_hash: &[u8],
        user_id: Id,
        created_at: u64,
        expires_at: u64,
    ) -> Result<()> {
        let conn = self.conn()?;
        conn.prepare_cached(
            "INSERT OR REPLACE INTO sessions (token_hash, user_id, created_at, expires_at) \
             VALUES (?, ?, ?, ?)",
        )?
        .execute(params![
            token_hash,
            to_sql(user_id),
            created_at as i64,
            expires_at as i64
        ])?;
        Ok(())
    }

    /// Resolve a session token hash to its user, rejecting expired sessions and
    /// deactivated accounts in the same query.
    pub fn session_user(&self, token_hash: &[u8], now_ms: u64) -> Result<User> {
        let conn = self.conn()?;
        conn.prepare_cached(&format!(
            "SELECT {USER_COLS_Q} FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = ? AND s.expires_at > ? AND u.deactivated = 0"
        ))?
        .query_row(params![token_hash, now_ms as i64], map_user)
        .optional()?
        .ok_or(Error::NotFound)
    }

    /// Log out a single session.
    pub fn delete_session(&self, token_hash: &[u8]) -> Result<()> {
        let conn = self.conn()?;
        conn.prepare_cached("DELETE FROM sessions WHERE token_hash = ?")?
            .execute([token_hash])?;
        Ok(())
    }

    /// Drop expired rows. Called on a timer; expiry is already enforced at
    /// lookup, so this is only reclaiming space.
    pub fn purge_expired_sessions(&self, now_ms: u64) -> Result<usize> {
        let conn = self.conn()?;
        Ok(conn
            .prepare_cached("DELETE FROM sessions WHERE expires_at <= ?")?
            .execute([now_ms as i64])?)
    }

    /// Resolve handles to ids, for turning `@mentions` into mention rows.
    pub fn ids_for_handles(&self, handles: &[String]) -> Result<Vec<(String, Id)>> {
        if handles.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn()?;
        // Bind each handle rather than interpolating, and reuse one prepared
        // statement across the batch.
        let mut stmt = conn
            .prepare_cached("SELECT handle, id FROM users WHERE handle = ? AND deactivated = 0")?;
        let mut out = Vec::with_capacity(handles.len());
        for h in handles {
            if let Some(pair) = stmt
                .query_row([h], |r| Ok((r.get::<_, String>(0)?, from_sql(r.get(1)?))))
                .optional()?
            {
                out.push(pair);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tensorchat_core::IdGen;

    fn store_with_user() -> (Store, IdGen, User) {
        let s = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        let u = s.create_user(g.next(), "alice", "Alice", "hash").unwrap();
        (s, g, u)
    }

    #[test]
    fn creates_and_reads_back_a_user() {
        let (s, _, u) = store_with_user();
        assert_eq!(s.user(u.id).unwrap(), u);
        assert_eq!(s.all_users().unwrap(), vec![u]);
    }

    #[test]
    fn duplicate_handle_is_a_conflict_not_a_raw_sqlite_error() {
        let (s, g, _) = store_with_user();
        let err = s
            .create_user(g.next(), "alice", "Other", "hash")
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "got {err:?}");
    }

    #[test]
    fn an_external_identity_provisions_once_and_is_recognised_after() {
        let s = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        let iss = "https://idp.example.com";

        let first = s
            .user_for_oidc_identity(iss, "sub-1", g.next(), "alice", "Alice", 100)
            .unwrap();
        let OidcLogin::Created(created) = first else {
            panic!("first sight should provision, got {first:?}");
        };
        assert_eq!(created.handle, "alice");
        // Arriving through a provider still makes you the first human here.
        assert!(created.admin);

        // The same subject is the same person, and does not create a second
        // account even though a fresh id was offered.
        let again = s
            .user_for_oidc_identity(iss, "sub-1", g.next(), "alice", "Alice Renamed", 200)
            .unwrap();
        let OidcLogin::Existing(found) = again else {
            panic!("a known identity must not provision again, got {again:?}");
        };
        assert_eq!(found.id, created.id);
        assert_eq!(s.all_users().unwrap().len(), 1);
    }

    #[test]
    fn a_local_password_can_never_open_a_provisioned_account() {
        let s = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        s.user_for_oidc_identity("https://idp.example.com", "s", g.next(), "alice", "A", 0)
            .unwrap();

        // The sentinel is not a PHC string, so there is no password to guess —
        // the account has no local credential by construction.
        let (_, hash) = s.user_for_login("alice").unwrap().unwrap();
        assert_eq!(hash, "!");
    }

    #[test]
    fn two_providers_may_both_call_their_users_subject_one() {
        let s = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        let a = s
            .user_for_oidc_identity("https://a.example.com", "1", g.next(), "alice", "A", 0)
            .unwrap();
        let b = s
            .user_for_oidc_identity("https://b.example.com", "1", g.next(), "bob", "B", 0)
            .unwrap();
        let (OidcLogin::Created(a), OidcLogin::Created(b)) = (a, b) else {
            panic!("both are first sightings");
        };
        assert_ne!(a.id, b.id, "the issuer is half the key, so these differ");
    }

    #[test]
    fn a_taken_handle_is_numbered_rather_than_refused() {
        let s = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        // Somebody already registered `alice` with a password.
        s.create_user(g.next(), "alice", "Local Alice", "hash")
            .unwrap();

        let out = s
            .user_for_oidc_identity(
                "https://idp.example.com",
                "sub-1",
                g.next(),
                "alice",
                "A",
                0,
            )
            .unwrap();
        let OidcLogin::Created(u) = out else {
            panic!("should have provisioned")
        };
        assert_eq!(u.handle, "alice2", "the name stays recognisable");

        // And again, for a third.
        let out = s
            .user_for_oidc_identity(
                "https://idp.example.com",
                "sub-2",
                g.next(),
                "alice",
                "A",
                0,
            )
            .unwrap();
        let OidcLogin::Created(u) = out else {
            panic!("should have provisioned")
        };
        assert_eq!(u.handle, "alice3");
    }

    #[test]
    fn numbering_a_long_handle_keeps_it_within_the_rules() {
        // Truncating to make room for the suffix must not leave a trailing
        // separator, which `validate_handle` rejects.
        let base = format!("{}-", "a".repeat(30));
        let numbered = numbered_handle(&base, 1);
        assert_eq!(numbered.len(), 31);
        assert!(numbered.ends_with('2'));
        tensorchat_core::text::validate_handle(&numbered).unwrap();

        assert_eq!(
            numbered_handle("alice", 0),
            "alice",
            "zero is the bare name"
        );
    }

    #[test]
    fn a_deactivated_account_cannot_come_back_through_its_provider() {
        let s = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        let iss = "https://idp.example.com";
        let OidcLogin::Created(u) = s
            .user_for_oidc_identity(iss, "sub-1", g.next(), "alice", "A", 0)
            .unwrap()
        else {
            panic!("should have provisioned")
        };
        s.set_deactivated(u.id, true).unwrap();

        // Neither signed in, nor handed a brand new account — which is what a
        // lookup that hid deactivated rows would have done.
        let out = s
            .user_for_oidc_identity(iss, "sub-1", g.next(), "alice", "A", 0)
            .unwrap();
        assert!(matches!(out, OidcLogin::Deactivated), "got {out:?}");
        assert_eq!(s.all_users().unwrap().len(), 1, "no second account");
    }

    #[test]
    fn login_lookup_returns_the_hash_and_none_for_unknown() {
        let (s, _, u) = store_with_user();
        let (found, hash) = s.user_for_login("alice").unwrap().unwrap();
        assert_eq!(found.id, u.id);
        assert_eq!(hash, "hash");
        assert!(s.user_for_login("nobody").unwrap().is_none());
    }

    #[test]
    fn sessions_expire_and_can_be_revoked() {
        let (s, _, u) = store_with_user();
        s.create_session(b"hash-a", u.id, 0, 1_000).unwrap();

        assert_eq!(s.session_user(b"hash-a", 500).unwrap().id, u.id);
        // Past expiry the same token no longer resolves.
        assert!(matches!(
            s.session_user(b"hash-a", 1_001),
            Err(Error::NotFound)
        ));

        s.create_session(b"hash-b", u.id, 0, 10_000).unwrap();
        s.delete_session(b"hash-b").unwrap();
        assert!(matches!(s.session_user(b"hash-b", 1), Err(Error::NotFound)));
    }

    #[test]
    fn deactivated_users_cannot_authenticate() {
        let (s, _, u) = store_with_user();
        s.create_session(b"tok", u.id, 0, u64::MAX / 2).unwrap();
        s.conn()
            .unwrap()
            .execute(
                "UPDATE users SET deactivated = 1 WHERE id = ?",
                [to_sql(u.id)],
            )
            .unwrap();
        assert!(matches!(s.session_user(b"tok", 1), Err(Error::NotFound)));
    }

    #[test]
    fn purges_only_expired_sessions() {
        let (s, _, u) = store_with_user();
        s.create_session(b"old", u.id, 0, 100).unwrap();
        s.create_session(b"new", u.id, 0, 10_000).unwrap();
        assert_eq!(s.purge_expired_sessions(1_000).unwrap(), 1);
        assert!(s.session_user(b"new", 1_000).is_ok());
    }

    #[test]
    fn resolves_handles_to_ids_skipping_unknown() {
        let (s, g, u) = store_with_user();
        let bob = s.create_user(g.next(), "bob", "Bob", "h").unwrap();
        let got = s
            .ids_for_handles(&["alice".into(), "ghost".into(), "bob".into()])
            .unwrap();
        assert_eq!(
            got,
            vec![("alice".to_string(), u.id), ("bob".to_string(), bob.id)]
        );
    }

    #[test]
    fn changing_a_password_replaces_the_stored_hash() {
        let (s, _, u) = store_with_user();
        assert_eq!(s.password_hash(u.id).unwrap(), "hash");
        s.update_password(u.id, "new-hash").unwrap();
        assert_eq!(s.password_hash(u.id).unwrap(), "new-hash");
        assert_eq!(s.user_for_login("alice").unwrap().unwrap().1, "new-hash");
    }

    #[test]
    fn revoking_sessions_can_spare_the_caller_and_never_touches_other_users() {
        let (s, g, u) = store_with_user();
        let bob = s.create_user(g.next(), "bob", "Bob", "h").unwrap();
        for t in [b"tok-a", b"tok-b", b"tok-c"] {
            s.create_session(t, u.id, 0, u64::MAX / 2).unwrap();
        }
        s.create_session(b"bobs-tok", bob.id, 0, u64::MAX / 2)
            .unwrap();

        assert_eq!(s.delete_sessions_for_user(u.id, Some(b"tok-b")).unwrap(), 2);
        assert!(
            s.session_user(b"tok-b", 1).is_ok(),
            "caller stays signed in"
        );
        assert!(s.session_user(b"tok-a", 1).is_err());
        assert!(s.session_user(b"tok-c", 1).is_err());
        assert!(
            s.session_user(b"bobs-tok", 1).is_ok(),
            "another user's sessions are untouched"
        );

        // No exemption revokes everything, including the caller's.
        assert_eq!(s.delete_sessions_for_user(u.id, None).unwrap(), 1);
        assert!(s.session_user(b"tok-b", 1).is_err());
    }

    #[test]
    fn the_first_account_is_an_administrator_and_later_ones_are_not() {
        // Somebody has to be, or the workspace can never have one.
        let (s, g, first) = store_with_user();
        assert!(first.admin);
        assert_eq!(s.admin_count().unwrap(), 1);

        let second = s.create_user(g.next(), "bob", "Bob", "h").unwrap();
        assert!(!second.admin);
        assert_eq!(s.admin_count().unwrap(), 1);
        // And it round-trips through a read, not just the returned value.
        assert!(s.user(first.id).unwrap().admin);
    }

    #[test]
    fn admin_can_be_granted_and_revoked() {
        let (s, g, _) = store_with_user();
        let bob = s.create_user(g.next(), "bob", "Bob", "h").unwrap();

        assert!(s.set_admin(bob.id, true).unwrap().admin);
        assert_eq!(s.admin_count().unwrap(), 2);
        assert!(!s.set_admin(bob.id, false).unwrap().admin);
        assert_eq!(s.admin_count().unwrap(), 1);
    }

    #[test]
    fn deactivation_is_reversible_and_blocks_login() {
        let (s, g, _) = store_with_user();
        let bob = s.create_user(g.next(), "bob", "Bob", "h").unwrap();
        s.create_session(b"bobs-token", bob.id, 0, u64::MAX / 2)
            .unwrap();

        assert!(s.user_for_login("bob").unwrap().is_some());

        let off = s.set_deactivated(bob.id, true).unwrap();
        assert!(off.deactivated);
        // Login is refused at the lookup, not after the password check, so a
        // deactivated account is indistinguishable from an unknown one.
        assert!(s.user_for_login("bob").unwrap().is_none());
        assert!(s.session_user(b"bobs-token", 1).is_err());
        // The row survives, so authorship and thread structure are intact.
        assert_eq!(s.user(bob.id).unwrap().handle, "bob");

        assert!(!s.set_deactivated(bob.id, false).unwrap().deactivated);
        assert!(s.user_for_login("bob").unwrap().is_some());
    }

    #[test]
    fn a_deactivated_administrator_does_not_count_toward_the_admin_total() {
        // Otherwise the "do not remove the last admin" guard would be satisfied
        // by an administrator who cannot sign in.
        let (s, _, first) = store_with_user();
        assert_eq!(s.admin_count().unwrap(), 1);
        s.set_deactivated(first.id, true).unwrap();
        assert_eq!(s.admin_count().unwrap(), 0);
    }

    #[test]
    fn administering_an_unknown_account_is_not_found() {
        let (s, _, _) = store_with_user();
        assert!(matches!(s.set_admin(Id(4242), true), Err(Error::NotFound)));
        assert!(matches!(
            s.set_deactivated(Id(4242), true),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn updates_profile() {
        let (s, _, u) = store_with_user();
        let updated = s.update_profile(u.id, "Alice A.", "on vacation").unwrap();
        assert_eq!(updated.display_name, "Alice A.");
        assert_eq!(updated.status, "on vacation");
    }
}
