//! Pinned messages.
//!
//! A pin is channel-scoped and viewer-independent — everyone in a channel sees
//! the same set — which is what lets the change broadcast as a single encoded
//! frame like any other channel event.

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use tc_core::{Id, Message};

use crate::{Error, Result, Store, to_sql};

/// The most pins one channel may hold.
///
/// Not an arbitrary tidiness rule: the pin list is fetched whole when a channel
/// is opened, so an unbounded one would turn channel-open into an unbounded
/// query. A hundred is far past the point where a pinned list stops being
/// useful to read anyway.
pub const MAX_PINS_PER_CHANNEL: usize = 100;

impl Store {
    /// Pin a message. Idempotent; returns the channel it lives in and whether
    /// this call actually changed anything, so the caller only broadcasts a
    /// real change.
    pub fn pin_message(&self, message: Id, by: Id, now_ms: u64) -> Result<(Id, bool)> {
        let mut conn = self.conn()?;
        // IMMEDIATE: this reads the message and counts existing pins before
        // writing, and a DEFERRED transaction upgrading from read to write
        // cannot be rescued by busy_timeout.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let (channel, deleted): (i64, bool) = tx
            .prepare_cached("SELECT channel_id, deleted FROM messages WHERE id = ?")?
            .query_row([to_sql(message)], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?
            .ok_or(Error::NotFound)?;
        if deleted {
            return Err(Error::Invalid("that message was deleted"));
        }

        // Counted inside the same transaction as the insert, so two people
        // pinning at once cannot both observe room and both take it.
        let pins: i64 = tx
            .prepare_cached("SELECT count(*) FROM pins WHERE channel_id = ?")?
            .query_row([channel], |r| r.get(0))?;
        if pins as usize >= MAX_PINS_PER_CHANNEL {
            return Err(Error::Invalid("this channel has too many pinned messages"));
        }

        let n = tx
            .prepare_cached(
                "INSERT OR IGNORE INTO pins (channel_id, message_id, pinned_by, pinned_at) \
                 VALUES (?, ?, ?, ?)",
            )?
            .execute(params![channel, to_sql(message), to_sql(by), now_ms as i64])?;
        tx.commit()?;
        Ok((crate::from_sql(channel), n > 0))
    }

    /// Unpin a message. Idempotent; returns the channel and whether anything
    /// changed.
    pub fn unpin_message(&self, message: Id) -> Result<(Id, bool)> {
        let conn = self.conn()?;
        let channel: i64 = conn
            .prepare_cached("SELECT channel_id FROM messages WHERE id = ?")?
            .query_row([to_sql(message)], |r| r.get(0))
            .optional()?
            .ok_or(Error::NotFound)?;
        let n = conn
            .prepare_cached("DELETE FROM pins WHERE channel_id = ? AND message_id = ?")?
            .execute(params![channel, to_sql(message)])?;
        Ok((crate::from_sql(channel), n > 0))
    }

    /// Every pinned message in a channel, newest first.
    ///
    /// Ordered by message id rather than pin time: the list reads as an extract
    /// of the channel, so it should run in the channel's own order, not in the
    /// order somebody happened to click.
    pub fn pinned_messages(&self, channel: Id, viewer: Id) -> Result<Vec<Message>> {
        if !self.is_member(channel, viewer)? {
            return Err(Error::Forbidden);
        }
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {cols} FROM messages m \
             JOIN pins p ON p.message_id = m.id \
             WHERE p.channel_id = ? AND m.deleted = 0 \
             ORDER BY m.id DESC LIMIT {MAX_PINS_PER_CHANNEL}",
            cols = crate::messages::MSG_COLS_Q,
        ))?;
        let mut messages: Vec<Message> = stmt
            .query_map([to_sql(channel)], crate::messages::map_message)?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        drop(conn);

        self.hydrate(&mut messages, viewer)?;
        Ok(messages)
    }

    /// Just the ids, for a client that wants to mark pinned messages in history
    /// without pulling their bodies a second time.
    pub fn pinned_ids(&self, channel: Id, viewer: Id) -> Result<Vec<Id>> {
        if !self.is_member(channel, viewer)? {
            return Err(Error::Forbidden);
        }
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(
            "SELECT message_id FROM pins WHERE channel_id = ? ORDER BY message_id DESC",
        )?;
        Ok(stmt
            .query_map([to_sql(channel)], |r| Ok(crate::from_sql(r.get(0)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NewChannel, NewMessage};
    use tc_core::{ChannelKind, IdGen};

    struct Fx {
        s: Store,
        g: IdGen,
        alice: Id,
        bob: Id,
        channel: Id,
    }

    fn fx() -> Fx {
        let s = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        let alice = s.create_user(g.next(), "alice", "Alice", "h").unwrap().id;
        let bob = s.create_user(g.next(), "bob", "Bob", "h").unwrap().id;
        let channel = s
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
        Fx {
            s,
            g,
            alice,
            bob,
            channel,
        }
    }

    fn post(f: &Fx, body: &str) -> Id {
        f.s.insert_message(NewMessage {
            id: f.g.next(),
            channel_id: f.channel,
            author_id: f.alice,
            body,
            thread_root: None,
            attachments: &[],
            mentions: &[],
        })
        .unwrap()
        .id
    }

    #[test]
    fn pinning_is_idempotent_and_reports_real_changes() {
        let f = fx();
        let m = post(&f, "worth keeping");

        let (channel, changed) = f.s.pin_message(m, f.alice, 10).unwrap();
        assert_eq!(channel, f.channel);
        assert!(changed);
        // A second pin is a no-op, so the caller does not re-broadcast.
        assert!(!f.s.pin_message(m, f.bob, 11).unwrap().1);

        assert!(f.s.unpin_message(m).unwrap().1);
        assert!(!f.s.unpin_message(m).unwrap().1);
    }

    #[test]
    fn pinned_messages_come_back_hydrated_and_newest_first() {
        let f = fx();
        let (a, b) = (post(&f, "first"), post(&f, "second"));
        f.s.set_reaction(a, f.bob, "📌", true).unwrap();
        f.s.pin_message(a, f.alice, 1).unwrap();
        f.s.pin_message(b, f.alice, 2).unwrap();

        let pinned = f.s.pinned_messages(f.channel, f.alice).unwrap();
        assert_eq!(
            pinned.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![b, a],
            "channel order, not the order they were pinned"
        );
        assert_eq!(pinned[1].reactions.len(), 1, "must arrive hydrated");
        assert_eq!(f.s.pinned_ids(f.channel, f.alice).unwrap(), vec![b, a]);
    }

    #[test]
    fn a_deleted_message_cannot_be_pinned_and_drops_out_of_the_list() {
        let f = fx();
        let m = post(&f, "oops");
        f.s.pin_message(m, f.alice, 1).unwrap();
        f.s.delete_message(m, f.alice, false).unwrap();

        // The pin row survives the soft delete, but a tombstone must not show
        // up in the pinned list as a blank entry.
        assert!(f.s.pinned_messages(f.channel, f.alice).unwrap().is_empty());

        let other = post(&f, "also oops");
        f.s.delete_message(other, f.alice, false).unwrap();
        assert!(matches!(
            f.s.pin_message(other, f.alice, 2),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn pins_are_scoped_to_members() {
        let f = fx();
        let carol = f.s.create_user(f.g.next(), "carol", "C", "h").unwrap().id;
        let m = post(&f, "private business");
        f.s.pin_message(m, f.alice, 1).unwrap();

        assert!(matches!(
            f.s.pinned_messages(f.channel, carol),
            Err(Error::Forbidden)
        ));
        assert!(matches!(
            f.s.pinned_ids(f.channel, carol),
            Err(Error::Forbidden)
        ));
    }

    #[test]
    fn pinning_an_unknown_message_is_not_found() {
        let f = fx();
        assert!(matches!(
            f.s.pin_message(Id(424242), f.alice, 1),
            Err(Error::NotFound)
        ));
        assert!(matches!(
            f.s.unpin_message(Id(424242)),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn the_per_channel_cap_is_enforced() {
        let f = fx();
        for _ in 0..MAX_PINS_PER_CHANNEL {
            let m = post(&f, "pin me");
            f.s.pin_message(m, f.alice, 1).unwrap();
        }
        let one_too_many = post(&f, "no room");
        assert!(matches!(
            f.s.pin_message(one_too_many, f.alice, 1),
            Err(Error::Invalid(_))
        ));

        // Freeing a slot lets the next one in, so the cap is a ceiling rather
        // than a permanent lockout.
        let pinned = f.s.pinned_ids(f.channel, f.alice).unwrap();
        f.s.unpin_message(pinned[0]).unwrap();
        assert!(f.s.pin_message(one_too_many, f.alice, 1).unwrap().1);
    }
}
