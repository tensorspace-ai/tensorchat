//! Invite links.
//!
//! An invite is a bearer credential that buys exactly one thing: permission to
//! call `/api/register` while registration is otherwise closed. It grants no
//! privilege beyond that — the account it creates is an ordinary member, and
//! every later authorization decision is made the same way it would have been
//! for an account created any other way.
//!
//! The interesting part is redemption. "Does this invite have a seat left?" is a
//! read that decides a write, so it has to happen in the same transaction as the
//! account insert; otherwise two people redeeming the last seat of a single-use
//! link would both see `uses < max_uses` and both get in. See
//! [`Store::create_user_via_invite`].

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use tensorchat_core::{Id, User};

use crate::{Error, Result, Store, from_sql, to_sql};

/// An invite as it can safely be shown after creation: everything except the
/// secret, which exists only in the response that minted it.
#[derive(Debug, Clone, PartialEq)]
pub struct Invite {
    pub id: Id,
    pub label: String,
    /// The administrator who minted it, or `None` when there was none — a
    /// closed, empty workspace bootstrapped from the operator console.
    pub created_by: Option<Id>,
    pub created_at: u64,
    /// `None` never expires.
    pub expires_at: Option<u64>,
    /// `0` is unlimited.
    pub max_uses: u32,
    pub uses: u32,
}

impl Invite {
    /// Whether this invite would still be accepted at `now_ms`.
    ///
    /// Derived rather than stored: a "revoked" column would need a sweep to stay
    /// truthful about expiry, and there is nothing here that a comparison cannot
    /// answer at read time.
    pub fn is_live(&self, now_ms: u64) -> bool {
        let unexpired = self.expires_at.is_none_or(|e| e > now_ms);
        let has_seats = self.max_uses == 0 || self.uses < self.max_uses;
        unexpired && has_seats
    }
}

/// The `WHERE` clause that defines a redeemable invite, shared by the redemption
/// path and the pre-flight check so the two can never disagree about what counts
/// as valid. Binds `?` = token_hash, `?` = now_ms (twice).
const LIVE_INVITE: &str = "token_hash = ? \
     AND (expires_at IS NULL OR expires_at > ?) \
     AND (max_uses = 0 OR uses < max_uses)";

const INVITE_COLS: &str = "id, label, created_by, created_at, expires_at, max_uses, uses";

fn map_invite(row: &rusqlite::Row<'_>) -> rusqlite::Result<Invite> {
    Ok(Invite {
        id: from_sql(row.get(0)?),
        label: row.get(1)?,
        created_by: row.get::<_, Option<i64>>(2)?.map(from_sql),
        created_at: row.get::<_, i64>(3)? as u64,
        expires_at: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
        max_uses: row.get::<_, i64>(5)? as u32,
        uses: row.get::<_, i64>(6)? as u32,
    })
}

/// The arguments for minting an invite, grouped like [`crate::NewChannel`] —
/// seven positional parameters of which three are integers would be too easy to
/// transpose at a call site.
pub struct NewInvite<'a> {
    pub id: Id,
    /// SHA-256 of the token. The token itself is never stored.
    pub token_hash: &'a [u8],
    pub label: &'a str,
    /// `None` when nobody could be credited; see [`Invite::created_by`].
    pub created_by: Option<Id>,
    pub created_at: u64,
    /// `None` never expires.
    pub expires_at: Option<u64>,
    /// `0` is unlimited.
    pub max_uses: u32,
}

impl Store {
    /// Record an invite. Only its digest is stored; the caller shows the link
    /// once and can never recover it.
    pub fn create_invite(&self, new: NewInvite<'_>) -> Result<Invite> {
        let conn = self.conn()?;
        conn.prepare_cached(
            "INSERT INTO invites \
             (token_hash, id, label, created_by, created_at, expires_at, max_uses) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?
        .execute(params![
            new.token_hash,
            to_sql(new.id),
            new.label,
            new.created_by.map(to_sql),
            new.created_at as i64,
            new.expires_at.map(|v| v as i64),
            new.max_uses as i64,
        ])?;

        Ok(Invite {
            id: new.id,
            label: new.label.to_string(),
            created_by: new.created_by,
            created_at: new.created_at,
            expires_at: new.expires_at,
            max_uses: new.max_uses,
            uses: 0,
        })
    }

    /// Whether a presented token would be accepted right now.
    ///
    /// Only ever used to decide what the sign-up screen should say. Registration
    /// re-checks under the write lock, because anything answered here is stale
    /// the moment it is returned.
    pub fn invite_is_live(&self, token_hash: &[u8], now_ms: u64) -> Result<bool> {
        let conn = self.conn()?;
        Ok(conn
            .prepare_cached(&format!(
                "SELECT 1 FROM invites WHERE {LIVE_INVITE} LIMIT 1"
            ))?
            .query_row(params![token_hash, now_ms as i64], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Create an account, consuming a seat on `token_hash`.
    ///
    /// The seat check and the account insert share one transaction, so a
    /// single-use link cannot admit two people who race each other. `IMMEDIATE`
    /// for the reason [`Store::create_user`] uses it: this reads to decide a
    /// write, and a deferred transaction that discovered the conflict at commit
    /// time could not be retried safely.
    ///
    /// Returns [`Error::Forbidden`] when the invite is unknown, expired, or
    /// exhausted — one error for all three, because telling an attacker which of
    /// them it was would confirm that a token exists.
    pub fn create_user_via_invite(
        &self,
        id: Id,
        handle: &str,
        display_name: &str,
        password_hash: &str,
        token_hash: &[u8],
        now_ms: u64,
    ) -> Result<User> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Claim the seat first. `uses < max_uses` inside the UPDATE means the
        // check and the increment are one atomic step: a second redemption of
        // the last seat matches zero rows rather than reading a stale count.
        let claimed = tx
            .prepare_cached(&format!(
                "UPDATE invites SET uses = uses + 1 WHERE {LIVE_INVITE}"
            ))?
            .execute(params![token_hash, now_ms as i64])?;
        if claimed == 0 {
            return Err(Error::Forbidden);
        }

        // Same bootstrap rule as `create_user`: the first human to register is
        // the administrator. An invite-only workspace still needs one, and the
        // very first account can legitimately arrive through a link an operator
        // minted out of band.
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
        // A duplicate handle rolls back the whole transaction, so the seat is
        // returned rather than burned by a failed attempt.
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

    /// Every invite, newest first.
    ///
    /// Includes spent and expired ones: an administration screen needs to show
    /// that a link is dead as much as that it is live, and hiding them would
    /// make "did that invite get used?" unanswerable.
    pub fn invites(&self) -> Result<Vec<Invite>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {INVITE_COLS} FROM invites ORDER BY id DESC"
        ))?;
        Ok(stmt
            .query_map([], map_invite)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Revoke one invite by its public id. Returns whether it existed.
    pub fn delete_invite(&self, id: Id) -> Result<bool> {
        let conn = self.conn()?;
        Ok(conn
            .prepare_cached("DELETE FROM invites WHERE id = ?")?
            .execute([to_sql(id)])?
            > 0)
    }

    /// Drop invites that expired some time ago. Called on the same timer as
    /// session purging; expiry is already enforced at redemption, so this is
    /// only reclaiming space.
    ///
    /// Exhausted-but-unexpired invites are deliberately kept — they are the
    /// audit trail for "who did we let in", and they are one row each.
    pub fn purge_expired_invites(&self, cutoff_ms: u64) -> Result<usize> {
        let conn = self.conn()?;
        Ok(conn
            .prepare_cached("DELETE FROM invites WHERE expires_at IS NOT NULL AND expires_at <= ?")?
            .execute([cutoff_ms as i64])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tensorchat_core::IdGen;

    fn fx() -> (Store, IdGen, Id) {
        let s = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        let admin = s.create_user(g.next(), "alice", "Alice", "h").unwrap().id;
        (s, g, admin)
    }

    /// Mint an invite, defaulting to no expiry and no use cap.
    fn mint(s: &Store, g: &IdGen, by: Id, digest: &[u8]) -> Invite {
        mint_with(s, g.next(), by, digest, "", None, 0)
    }

    fn mint_with(
        s: &Store,
        id: Id,
        by: Id,
        digest: &[u8],
        label: &str,
        expires_at: Option<u64>,
        max_uses: u32,
    ) -> Invite {
        s.create_invite(NewInvite {
            id,
            token_hash: digest,
            label,
            created_by: Some(by),
            created_at: 100,
            expires_at,
            max_uses,
        })
        .unwrap()
    }

    #[test]
    fn an_invite_admits_an_account_and_records_the_use() {
        let (s, g, admin) = fx();
        let inv = mint(&s, &g, admin, b"digest");
        assert_eq!(inv.uses, 0);
        assert!(inv.is_live(1_000));

        let bob = s
            .create_user_via_invite(g.next(), "bob", "Bob", "h", b"digest", 200)
            .unwrap();
        assert_eq!(bob.handle, "bob");
        assert!(!bob.admin, "an invited account is an ordinary member");
        assert_eq!(s.user(bob.id).unwrap().handle, "bob");
        assert_eq!(s.invites().unwrap()[0].uses, 1);
    }

    #[test]
    fn a_single_use_invite_admits_exactly_one_account() {
        let (s, g, admin) = fx();
        mint_with(&s, g.next(), admin, b"once", "", None, 1);

        s.create_user_via_invite(g.next(), "bob", "Bob", "h", b"once", 200)
            .unwrap();
        // The seat is gone, so the second redemption is refused rather than
        // silently over-subscribing the link.
        assert!(matches!(
            s.create_user_via_invite(g.next(), "carol", "Carol", "h", b"once", 200),
            Err(Error::Forbidden)
        ));
        assert!(s.user_for_login("carol").unwrap().is_none());
        assert!(!s.invites().unwrap()[0].is_live(200));
    }

    #[test]
    fn an_expired_invite_is_refused() {
        let (s, g, admin) = fx();
        let inv = mint_with(&s, g.next(), admin, b"stale", "", Some(500), 0);
        assert!(inv.is_live(499));
        assert!(!inv.is_live(500), "expiry is exclusive at the boundary");

        assert!(s.invite_is_live(b"stale", 499).unwrap());
        assert!(!s.invite_is_live(b"stale", 501).unwrap());
        assert!(matches!(
            s.create_user_via_invite(g.next(), "bob", "Bob", "h", b"stale", 501),
            Err(Error::Forbidden)
        ));
    }

    #[test]
    fn an_unknown_token_is_refused_the_same_way_an_exhausted_one_is() {
        // One error for both, so a probe cannot distinguish "no such invite"
        // from "that invite is spent" and thereby confirm a token exists.
        let (s, g, admin) = fx();
        mint_with(&s, g.next(), admin, b"once", "", None, 1);
        s.create_user_via_invite(g.next(), "bob", "Bob", "h", b"once", 200)
            .unwrap();

        let spent = s.create_user_via_invite(g.next(), "carol", "C", "h", b"once", 200);
        let unknown = s.create_user_via_invite(g.next(), "carol", "C", "h", b"nope", 200);
        assert!(matches!(spent, Err(Error::Forbidden)));
        assert!(matches!(unknown, Err(Error::Forbidden)));
        assert!(!s.invite_is_live(b"nope", 1).unwrap());
    }

    #[test]
    fn a_failed_registration_does_not_burn_a_seat() {
        // The handle collision rolls the transaction back, so the person who
        // picked a taken name can try again with another one.
        let (s, g, admin) = fx();
        mint_with(&s, g.next(), admin, b"once", "", None, 1);

        assert!(matches!(
            s.create_user_via_invite(g.next(), "alice", "Alice II", "h", b"once", 200),
            Err(Error::Conflict(_))
        ));
        assert_eq!(s.invites().unwrap()[0].uses, 0, "the seat is still there");

        s.create_user_via_invite(g.next(), "bob", "Bob", "h", b"once", 200)
            .unwrap();
    }

    #[test]
    fn the_first_human_through_an_invite_still_becomes_administrator() {
        // An operator can mint a link before anybody has registered, and the
        // workspace must still end up with someone who can administer it.
        let s = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        let bot = s.create_bot(g.next(), "hookbot", "Hook").unwrap();
        mint_with(&s, g.next(), bot.id, b"first", "", None, 0);

        let alice = s
            .create_user_via_invite(g.next(), "alice", "Alice", "h", b"first", 200)
            .unwrap();
        assert!(alice.admin);
        assert_eq!(s.admin_count().unwrap(), 1);

        let bob = s
            .create_user_via_invite(g.next(), "bob", "Bob", "h", b"first", 300)
            .unwrap();
        assert!(!bob.admin, "only the first");
    }

    #[test]
    fn invites_are_revocable_individually_and_listed_newest_first() {
        let (s, g, admin) = fx();
        let keep = g.next();
        let drop_me = g.next();
        mint_with(&s, keep, admin, b"keep", "keep", None, 0);
        mint_with(&s, drop_me, admin, b"drop", "drop", None, 0);

        let listed = s.invites().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, drop_me, "newest first");
        assert_eq!(listed[0].label, "drop");

        assert!(s.delete_invite(drop_me).unwrap());
        assert!(
            !s.delete_invite(drop_me).unwrap(),
            "revocation is idempotent"
        );
        assert!(matches!(
            s.create_user_via_invite(g.next(), "bob", "Bob", "h", b"drop", 200),
            Err(Error::Forbidden)
        ));
        assert!(
            s.create_user_via_invite(g.next(), "bob", "Bob", "h", b"keep", 200)
                .is_ok()
        );
    }

    #[test]
    fn purging_drops_expired_invites_and_spares_the_rest() {
        let (s, g, admin) = fx();
        mint_with(&s, g.next(), admin, b"old", "", Some(100), 0);
        mint_with(&s, g.next(), admin, b"live", "", Some(10_000), 0);
        // Spent but unexpired: kept, because it is the record of who got in.
        mint_with(&s, g.next(), admin, b"spent", "", None, 1);
        s.create_user_via_invite(g.next(), "bob", "Bob", "h", b"spent", 2)
            .unwrap();

        assert_eq!(s.purge_expired_invites(1_000).unwrap(), 1);
        assert_eq!(s.invites().unwrap().len(), 2);
        assert!(s.invite_is_live(b"live", 1_000).unwrap());
    }

    #[test]
    fn an_unlimited_invite_never_runs_out() {
        let (s, g, admin) = fx();
        let inv = mint(&s, &g, admin, b"open");
        assert_eq!(inv.max_uses, 0);

        for (i, name) in ["bob", "carol", "dave"].iter().enumerate() {
            s.create_user_via_invite(g.next(), name, name, "h", b"open", 200 + i as u64)
                .unwrap();
        }
        let after = &s.invites().unwrap()[0];
        assert_eq!(after.uses, 3);
        assert!(after.is_live(10_000), "no cap means no exhaustion");
    }
}
