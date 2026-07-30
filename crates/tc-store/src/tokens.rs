//! Bot accounts and long-lived API tokens.
//!
//! A bot is an ordinary `User` with `bot = true` and no usable password, so
//! every existing authorization rule applies to it unchanged — most importantly
//! channel membership. That is what contains a leaked token: it can reach
//! exactly the channels its bot was added to, and nothing else.

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use tc_core::{Id, User};

use crate::{Error, Result, Store, from_sql, to_sql};

/// A token as it can safely be shown after creation: everything except the
/// secret, which exists only in the response that minted it.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiToken {
    pub id: Id,
    pub user_id: Id,
    pub label: String,
    pub created_at: u64,
    pub last_used: Option<u64>,
}

impl Store {
    /// Create a bot account.
    ///
    /// `password_hash` is a value no password can ever produce, so the account
    /// exists in the user directory and can be mentioned and added to channels,
    /// but interactive login is impossible by construction rather than by a
    /// check somebody could forget.
    pub fn create_bot(&self, id: Id, handle: &str, display_name: &str) -> Result<User> {
        let conn = self.conn()?;
        conn.prepare_cached(
            "INSERT INTO users (id, handle, display_name, password_hash, bot) \
             VALUES (?, ?, ?, '!', 1)",
        )?
        .execute(params![to_sql(id), handle, display_name])
        .map_err(|e| Error::from_sqlite(e, "that handle"))?;

        Ok(User {
            id,
            handle: handle.to_string(),
            display_name: display_name.to_string(),
            status: String::new(),
            bot: true,
            deactivated: false,
            admin: false,
        })
    }

    /// Record a token. Only its digest is stored; the caller shows the secret
    /// once and can never recover it.
    pub fn create_api_token(
        &self,
        id: Id,
        token_hash: &[u8],
        user_id: Id,
        label: &str,
        created_by: Id,
        now_ms: u64,
    ) -> Result<ApiToken> {
        let conn = self.conn()?;
        conn.prepare_cached(
            "INSERT INTO api_tokens (token_hash, id, user_id, label, created_by, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )?
        .execute(params![
            token_hash,
            to_sql(id),
            to_sql(user_id),
            label,
            to_sql(created_by),
            now_ms as i64
        ])?;

        Ok(ApiToken {
            id,
            user_id,
            label: label.to_string(),
            created_at: now_ms,
            last_used: None,
        })
    }

    /// Resolve a token digest to its account, and stamp `last_used`.
    ///
    /// Refuses deactivated accounts, matching [`Store::session_user`] — so
    /// deactivating a bot kills its integrations without having to hunt down
    /// every token first.
    pub fn api_token_user(&self, token_hash: &[u8], now_ms: u64) -> Result<User> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let user_id: Option<i64> = tx
            .prepare_cached(
                "SELECT t.user_id FROM api_tokens t JOIN users u ON u.id = t.user_id \
                 WHERE t.token_hash = ? AND u.deactivated = 0",
            )?
            .query_row([token_hash], |r| r.get(0))
            .optional()?;
        let Some(user_id) = user_id else {
            return Err(Error::NotFound);
        };

        tx.prepare_cached("UPDATE api_tokens SET last_used = ? WHERE token_hash = ?")?
            .execute(params![now_ms as i64, token_hash])?;
        tx.commit()?;
        drop(conn);

        self.user(from_sql(user_id))
    }

    /// Every token belonging to one account, newest first. Never includes a
    /// secret — there is nothing stored that could reconstruct one.
    pub fn api_tokens_for(&self, user_id: Id) -> Result<Vec<ApiToken>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(
            "SELECT id, user_id, label, created_at, last_used FROM api_tokens \
             WHERE user_id = ? ORDER BY id DESC",
        )?;
        Ok(stmt
            .query_map([to_sql(user_id)], |r| {
                Ok(ApiToken {
                    id: from_sql(r.get(0)?),
                    user_id: from_sql(r.get(1)?),
                    label: r.get(2)?,
                    created_at: r.get::<_, i64>(3)? as u64,
                    last_used: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Revoke one token by its public id. Returns whether it existed.
    pub fn delete_api_token(&self, id: Id) -> Result<bool> {
        let conn = self.conn()?;
        Ok(conn
            .prepare_cached("DELETE FROM api_tokens WHERE id = ?")?
            .execute([to_sql(id)])?
            > 0)
    }

    /// Every bot account, for an administration screen.
    pub fn bots(&self) -> Result<Vec<User>> {
        Ok(self.all_users()?.into_iter().filter(|u| u.bot).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tc_core::IdGen;

    fn fx() -> (Store, IdGen, Id) {
        let s = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        let admin = s.create_user(g.next(), "alice", "Alice", "h").unwrap().id;
        (s, g, admin)
    }

    #[test]
    fn a_bot_cannot_log_in_with_any_password() {
        // The stored hash is not a valid PHC string, so verification cannot
        // succeed no matter what is presented.
        let (s, g, _) = fx();
        let bot = s.create_bot(g.next(), "deploybot", "Deploy Bot").unwrap();
        assert!(bot.bot);
        assert!(!bot.admin);

        let (_, hash) = s.user_for_login("deploybot").unwrap().unwrap();
        assert_eq!(hash, "!");
        // It is still a real account: visible, mentionable, addable.
        assert!(s.all_users().unwrap().iter().any(|u| u.id == bot.id));
        assert_eq!(s.bots().unwrap(), vec![bot]);
    }

    #[test]
    fn a_bot_never_becomes_the_bootstrap_administrator() {
        // `create_user` promotes the first account; `create_bot` must not, or a
        // fresh instance whose first account is a bot would have an
        // administrator nobody can sign in as.
        let s = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        let bot = s.create_bot(g.next(), "firstbot", "Bot").unwrap();
        assert!(!bot.admin);
        assert_eq!(s.admin_count().unwrap(), 0);

        let human = s.create_user(g.next(), "alice", "Alice", "h").unwrap();
        assert!(human.admin, "the first human still becomes administrator");
    }

    #[test]
    fn a_token_resolves_to_its_account_and_records_use() {
        let (s, g, admin) = fx();
        let bot = s.create_bot(g.next(), "deploybot", "Deploy Bot").unwrap();
        s.create_api_token(g.next(), b"digest", bot.id, "ci", admin, 100)
            .unwrap();

        assert_eq!(s.api_token_user(b"digest", 500).unwrap().id, bot.id);
        let listed = s.api_tokens_for(bot.id).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].label, "ci");
        assert_eq!(
            listed[0].last_used,
            Some(500),
            "last_used distinguishes a live integration from a forgotten one"
        );

        assert!(matches!(
            s.api_token_user(b"wrong", 1),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn deactivating_a_bot_kills_its_tokens_without_revoking_them_one_by_one() {
        let (s, g, admin) = fx();
        let bot = s.create_bot(g.next(), "deploybot", "Deploy Bot").unwrap();
        s.create_api_token(g.next(), b"digest", bot.id, "ci", admin, 1)
            .unwrap();
        assert!(s.api_token_user(b"digest", 2).is_ok());

        s.set_deactivated(bot.id, true).unwrap();
        assert!(matches!(
            s.api_token_user(b"digest", 3),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn tokens_are_revocable_individually() {
        let (s, g, admin) = fx();
        let bot = s.create_bot(g.next(), "deploybot", "Deploy Bot").unwrap();
        let keep = g.next();
        let drop_me = g.next();
        s.create_api_token(keep, b"keep", bot.id, "keep", admin, 1)
            .unwrap();
        s.create_api_token(drop_me, b"drop", bot.id, "drop", admin, 1)
            .unwrap();

        assert!(s.delete_api_token(drop_me).unwrap());
        assert!(
            !s.delete_api_token(drop_me).unwrap(),
            "revocation is idempotent"
        );
        assert!(matches!(s.api_token_user(b"drop", 1), Err(Error::NotFound)));
        assert!(s.api_token_user(b"keep", 1).is_ok());
        assert_eq!(s.api_tokens_for(bot.id).unwrap().len(), 1);
    }

    #[test]
    fn a_password_change_does_not_disturb_api_tokens() {
        // The whole reason these are not sessions: rotating a password must not
        // silently break an integration.
        let (s, g, admin) = fx();
        let bot = s.create_bot(g.next(), "deploybot", "Deploy Bot").unwrap();
        s.create_api_token(g.next(), b"digest", bot.id, "ci", admin, 1)
            .unwrap();
        s.create_session(b"a-session", bot.id, 0, u64::MAX / 2)
            .unwrap();

        s.delete_sessions_for_user(bot.id, None).unwrap();
        assert!(s.session_user(b"a-session", 1).is_err());
        assert!(s.api_token_user(b"digest", 1).is_ok());
    }

    #[test]
    fn duplicate_bot_handles_conflict_with_humans_too() {
        // One namespace, because `@mentions` resolve against it.
        let (s, g, _) = fx();
        assert!(matches!(
            s.create_bot(g.next(), "alice", "Not Alice"),
            Err(Error::Conflict(_))
        ));
    }
}
