//! Web Push subscriptions, and the notifications a push refers to.
//!
//! A subscription is the browser's promise to wake our service worker when the
//! site is closed. Only the endpoint is stored — see `migrations/008_push.sql`
//! for why the encryption keys are deliberately absent.

use rusqlite::{OptionalExtension, params};
use tensorchat_core::Id;

use crate::{Result, Store, from_sql, to_sql};

/// The most notifications one push will describe. A burst longer than this is
/// collapsed by the service worker anyway, one per conversation.
pub const MAX_NOTIFICATIONS: u32 = 20;

/// How many consecutive soft failures retire an endpoint. A push service that
/// has genuinely forgotten a subscription answers 404 or 410 and is pruned at
/// once; this is for the ones that just keep erroring.
pub const MAX_PUSH_FAILURES: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushSubscription {
    pub endpoint: String,
    pub user_id: Id,
}

/// One thing worth waking someone for, with enough context to render a
/// notification and link to the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationItem {
    pub channel_id: Id,
    pub message_id: Id,
    pub author_id: Id,
    pub body: String,
    /// Channel name; empty for a DM, which the caller titles by author instead.
    pub channel_name: String,
    pub is_dm: bool,
}

impl Store {
    /// Record a subscription, replacing any previous owner of the same endpoint.
    ///
    /// `INSERT OR REPLACE` rather than a plain insert: a browser can hand the
    /// same endpoint to a second account on a shared machine, and the row must
    /// follow whoever subscribed most recently or the previous user would keep
    /// receiving pushes about conversations they can no longer see.
    pub fn add_push_subscription(&self, endpoint: &str, user_id: Id, now_ms: u64) -> Result<()> {
        let conn = self.conn()?;
        conn.prepare_cached(
            "INSERT OR REPLACE INTO push_subscriptions (endpoint, user_id, created_at, failures) \
             VALUES (?, ?, ?, 0)",
        )?
        .execute(params![endpoint, to_sql(user_id), now_ms as i64])?;
        Ok(())
    }

    /// Forget one subscription. Returns whether it existed.
    pub fn remove_push_subscription(&self, endpoint: &str) -> Result<bool> {
        let conn = self.conn()?;
        Ok(conn
            .prepare_cached("DELETE FROM push_subscriptions WHERE endpoint = ?")?
            .execute([endpoint])?
            > 0)
    }

    /// Every live endpoint for one account.
    ///
    /// Refuses deactivated accounts, matching [`Store::session_user`] and
    /// [`Store::api_token_user`]. Without that, deactivating someone would still
    /// buzz their phone for every mention — and since their sessions are gone,
    /// the service worker's fetch would fail and it would show the generic
    /// fallback, leaking "something is happening" to an account that has been
    /// shut out.
    pub fn push_subscriptions_for(&self, user_id: Id) -> Result<Vec<String>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(
            "SELECT p.endpoint FROM push_subscriptions p \
               JOIN users u ON u.id = p.user_id \
              WHERE p.user_id = ? AND p.failures < ? AND u.deactivated = 0",
        )?;
        Ok(stmt
            .query_map(params![to_sql(user_id), MAX_PUSH_FAILURES as i64], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Whether an account has any subscription at all, so the client can show
    /// the toggle in the right state on a fresh page load.
    pub fn has_push_subscription(&self, user_id: Id, endpoint: &str) -> Result<bool> {
        let conn = self.conn()?;
        Ok(conn
            .prepare_cached(
                "SELECT 1 FROM push_subscriptions WHERE user_id = ? AND endpoint = ? LIMIT 1",
            )?
            .query_row(params![to_sql(user_id), endpoint], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Note that a delivery failed. Enough of these retires the endpoint.
    pub fn record_push_failure(&self, endpoint: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.prepare_cached(
            "UPDATE push_subscriptions SET failures = failures + 1 WHERE endpoint = ?",
        )?
        .execute([endpoint])?;
        Ok(())
    }

    /// Note that a delivery succeeded, clearing any accumulated failures.
    pub fn record_push_success(&self, endpoint: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.prepare_cached("UPDATE push_subscriptions SET failures = 0 WHERE endpoint = ?")?
            .execute([endpoint])?;
        Ok(())
    }

    /// What a push is about: unread messages that mention this user, plus
    /// everything unread in a direct or group message.
    ///
    /// The same rule the in-page notifier uses, evaluated server-side — the
    /// service worker has no store to consult, and this is what lets a push
    /// carry no payload. Muted channels are excluded and so is the caller's own
    /// authorship, because nobody needs waking for their own message arriving.
    pub fn pending_notifications(&self, user_id: Id, limit: u32) -> Result<Vec<NotificationItem>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(
            "SELECT m.id, m.channel_id, m.author_id, m.body, c.name, c.kind \
               FROM messages m \
               JOIN members mem ON mem.channel_id = m.channel_id AND mem.user_id = ?1 \
               JOIN channels c ON c.id = m.channel_id \
              WHERE m.id > mem.last_read \
                AND m.author_id <> ?1 \
                AND m.deleted = 0 \
                AND mem.muted = 0 \
                AND (c.kind IN (2, 3) \
                     OR EXISTS (SELECT 1 FROM mentions x \
                                 WHERE x.user_id = ?1 AND x.message_id = m.id)) \
              ORDER BY m.id DESC \
              LIMIT ?2",
        )?;
        Ok(stmt
            .query_map(
                params![to_sql(user_id), limit.clamp(1, MAX_NOTIFICATIONS) as i64],
                |r| {
                    let kind: i64 = r.get(5)?;
                    Ok(NotificationItem {
                        message_id: from_sql(r.get(0)?),
                        channel_id: from_sql(r.get(1)?),
                        author_id: from_sql(r.get(2)?),
                        body: r.get(3)?,
                        channel_name: r.get(4)?,
                        is_dm: kind == 2 || kind == 3,
                    })
                },
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Read a server setting.
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn()?;
        Ok(conn
            .prepare_cached("SELECT value FROM settings WHERE key = ?")?
            .query_row([key], |r| r.get::<_, String>(0))
            .optional()?)
    }

    /// Write a server setting, creating it if absent.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.prepare_cached("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)")?
            .execute(params![key, value])?;
        Ok(())
    }

    /// Read a setting, or create it from `make` if it is not there yet.
    ///
    /// One `IMMEDIATE` transaction, because two processes starting at once must
    /// not each mint a VAPID keypair and have the second overwrite the first —
    /// that would invalidate every subscription created against the first.
    pub fn setting_or_init(&self, key: &str, make: impl FnOnce() -> String) -> Result<String> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx
            .prepare_cached("SELECT value FROM settings WHERE key = ?")?
            .query_row([key], |r| r.get(0))
            .optional()?;
        if let Some(v) = existing {
            return Ok(v);
        }
        let value = make();
        tx.prepare_cached("INSERT INTO settings (key, value) VALUES (?, ?)")?
            .execute(params![key, &value])?;
        tx.commit()?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NewChannel, NewMessage};
    use tensorchat_core::{ChannelKind, IdGen};

    struct Fx {
        s: Store,
        g: IdGen,
        alice: Id,
        bob: Id,
        public: Id,
        dm: Id,
    }

    fn fx() -> Fx {
        let s = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        let alice = s.create_user(g.next(), "alice", "Alice", "h").unwrap().id;
        let bob = s.create_user(g.next(), "bob", "Bob", "h").unwrap().id;
        let public = s
            .create_channel(NewChannel {
                id: g.next(),
                kind: ChannelKind::Public,
                name: "general",
                topic: "",
                created_by: alice,
                created_at: 1,
                members: vec![bob],
            })
            .unwrap()
            .id;
        let dm = s.open_dm(g.next(), alice, vec![bob], 1).unwrap().id;
        Fx {
            s,
            g,
            alice,
            bob,
            public,
            dm,
        }
    }

    fn post(f: &Fx, ch: Id, author: Id, body: &str, mentions: &[Id]) -> Id {
        f.s.insert_message(NewMessage {
            id: f.g.next(),
            channel_id: ch,
            author_id: author,
            body,
            thread_root: None,
            attachments: &[],
            mentions,
        })
        .unwrap()
        .id
    }

    #[test]
    fn subscriptions_round_trip_and_are_removable() {
        let f = fx();
        f.s.add_push_subscription("https://push.example/a", f.alice, 1)
            .unwrap();
        f.s.add_push_subscription("https://push.example/b", f.alice, 1)
            .unwrap();
        f.s.add_push_subscription("https://push.example/c", f.bob, 1)
            .unwrap();

        let mut alices = f.s.push_subscriptions_for(f.alice).unwrap();
        alices.sort();
        assert_eq!(alices, ["https://push.example/a", "https://push.example/b"]);
        assert_eq!(f.s.push_subscriptions_for(f.bob).unwrap().len(), 1);

        assert!(
            f.s.has_push_subscription(f.alice, "https://push.example/a")
                .unwrap()
        );
        assert!(
            !f.s.has_push_subscription(f.bob, "https://push.example/a")
                .unwrap()
        );

        assert!(
            f.s.remove_push_subscription("https://push.example/a")
                .unwrap()
        );
        assert!(
            !f.s.remove_push_subscription("https://push.example/a")
                .unwrap(),
            "removal is idempotent"
        );
        assert_eq!(f.s.push_subscriptions_for(f.alice).unwrap().len(), 1);
    }

    #[test]
    fn an_endpoint_follows_whoever_subscribed_last() {
        // Two accounts on one browser: the endpoint is the same, and the older
        // owner must stop receiving pushes about conversations they left.
        let f = fx();
        f.s.add_push_subscription("https://push.example/shared", f.alice, 1)
            .unwrap();
        f.s.add_push_subscription("https://push.example/shared", f.bob, 2)
            .unwrap();
        assert!(f.s.push_subscriptions_for(f.alice).unwrap().is_empty());
        assert_eq!(f.s.push_subscriptions_for(f.bob).unwrap().len(), 1);
    }

    #[test]
    fn repeated_failures_retire_an_endpoint_and_success_clears_them() {
        let f = fx();
        f.s.add_push_subscription("https://push.example/flaky", f.alice, 1)
            .unwrap();
        for _ in 0..MAX_PUSH_FAILURES {
            f.s.record_push_failure("https://push.example/flaky")
                .unwrap();
        }
        assert!(
            f.s.push_subscriptions_for(f.alice).unwrap().is_empty(),
            "a permanently broken endpoint stops being retried"
        );

        f.s.record_push_success("https://push.example/flaky")
            .unwrap();
        assert_eq!(
            f.s.push_subscriptions_for(f.alice).unwrap().len(),
            1,
            "one success brings it back"
        );
    }

    #[test]
    fn deactivating_an_account_stops_its_pushes() {
        // Deactivation already revokes every session, so the service worker's
        // fetch would fail and it would fall back to a generic "new message" —
        // which would keep telling a shut-out account that something is
        // happening. Cut it off at the source instead.
        let f = fx();
        f.s.add_push_subscription("https://push.example/a", f.alice, 1)
            .unwrap();
        assert_eq!(f.s.push_subscriptions_for(f.alice).unwrap().len(), 1);

        f.s.set_deactivated(f.alice, true).unwrap();
        assert!(f.s.push_subscriptions_for(f.alice).unwrap().is_empty());

        // ...and reactivating restores them, so a temporary suspension does not
        // silently require every device to re-subscribe.
        f.s.set_deactivated(f.alice, false).unwrap();
        assert_eq!(f.s.push_subscriptions_for(f.alice).unwrap().len(), 1);
    }

    #[test]
    fn notifications_cover_mentions_and_direct_messages() {
        let f = fx();
        // An ordinary channel message that does not mention alice: not worth
        // waking her for.
        post(&f, f.public, f.bob, "morning everyone", &[]);
        let mention = post(&f, f.public, f.bob, "@alice can you look", &[f.alice]);
        let dm = post(&f, f.dm, f.bob, "are you around?", &[]);

        let items = f.s.pending_notifications(f.alice, 20).unwrap();
        let ids: Vec<Id> = items.iter().map(|i| i.message_id).collect();
        assert_eq!(ids, vec![dm, mention], "newest first");

        let by_id = |id: Id| items.iter().find(|i| i.message_id == id).unwrap();
        assert!(by_id(dm).is_dm);
        assert!(!by_id(mention).is_dm);
        assert_eq!(by_id(mention).channel_name, "general");
        assert_eq!(by_id(mention).author_id, f.bob);
        assert_eq!(by_id(dm).body, "are you around?");
    }

    #[test]
    fn your_own_messages_never_notify_you() {
        let f = fx();
        post(&f, f.dm, f.alice, "typing to myself", &[]);
        post(&f, f.public, f.alice, "@alice note to self", &[f.alice]);
        assert!(f.s.pending_notifications(f.alice, 20).unwrap().is_empty());
    }

    #[test]
    fn reading_a_channel_clears_its_notifications() {
        let f = fx();
        let m = post(&f, f.dm, f.bob, "are you around?", &[]);
        assert_eq!(f.s.pending_notifications(f.alice, 20).unwrap().len(), 1);
        f.s.mark_read(f.dm, f.alice, m).unwrap();
        assert!(f.s.pending_notifications(f.alice, 20).unwrap().is_empty());
    }

    #[test]
    fn a_muted_channel_does_not_notify() {
        // Mute is the user saying "do not interrupt me about this", which has
        // to hold for the notification that arrives while the tab is closed —
        // that is the interruption that actually matters.
        let f = fx();
        post(&f, f.public, f.bob, "@alice urgent", &[f.alice]);
        assert_eq!(f.s.pending_notifications(f.alice, 20).unwrap().len(), 1);
        f.s.set_muted(f.public, f.alice, true).unwrap();
        assert!(f.s.pending_notifications(f.alice, 20).unwrap().is_empty());
    }

    #[test]
    fn a_deleted_message_stops_notifying() {
        let f = fx();
        let m = post(&f, f.dm, f.bob, "oops", &[]);
        f.s.delete_message(m, f.bob, false).unwrap();
        assert!(f.s.pending_notifications(f.alice, 20).unwrap().is_empty());
    }

    #[test]
    fn notifications_never_reach_across_membership() {
        let f = fx();
        let carol =
            f.s.create_user(f.g.next(), "carol", "Carol", "h")
                .unwrap()
                .id;
        post(&f, f.dm, f.bob, "private to alice", &[]);
        assert!(
            f.s.pending_notifications(carol, 20).unwrap().is_empty(),
            "a non-member is not notified about a conversation they cannot see"
        );
    }

    #[test]
    fn the_notification_count_is_bounded() {
        let f = fx();
        for i in 0..40 {
            post(&f, f.dm, f.bob, &format!("message {i}"), &[]);
        }
        assert_eq!(
            f.s.pending_notifications(f.alice, 1_000).unwrap().len(),
            MAX_NOTIFICATIONS as usize,
            "a caller cannot ask for an unbounded page"
        );
    }

    #[test]
    fn settings_round_trip_and_initialize_once() {
        let f = fx();
        assert_eq!(f.s.setting("vapid").unwrap(), None);

        let first = f.s.setting_or_init("vapid", || "generated".into()).unwrap();
        assert_eq!(first, "generated");
        // The second call must not regenerate: a new keypair would invalidate
        // every subscription already created against the old one.
        let second =
            f.s.setting_or_init("vapid", || panic!("must not be called again"))
                .unwrap();
        assert_eq!(second, "generated");
        assert_eq!(f.s.setting("vapid").unwrap().as_deref(), Some("generated"));

        f.s.set_setting("vapid", "rotated").unwrap();
        assert_eq!(f.s.setting("vapid").unwrap().as_deref(), Some("rotated"));
    }
}
