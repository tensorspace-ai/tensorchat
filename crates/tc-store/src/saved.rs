//! Saved ("starred") messages.
//!
//! The private counterpart to a pin: a save is visible only to the person who
//! made it, so nothing here touches the broadcast path. Every read joins
//! against membership rather than filtering afterwards — the same rule search
//! follows — so leaving a channel takes its messages out of your saved list
//! instead of leaving you a private window into it.

use rusqlite::params;
use tc_core::{Id, Message};

use crate::{Result, Store, to_sql};

/// Upper bound on one page of saved messages.
pub const MAX_SAVED_PAGE: u32 = 200;

impl Store {
    /// Save or unsave a message for one user. Idempotent; returns whether
    /// anything changed.
    ///
    /// Unlike a pin, this is private state: nothing is broadcast, and the only
    /// person who can read it back is its owner.
    pub fn set_saved(&self, user: Id, message: Id, on: bool, now_ms: u64) -> Result<bool> {
        let conn = self.conn()?;
        let n = if on {
            // The caller has already checked that `user` can see `message`.
            conn.prepare_cached(
                "INSERT OR IGNORE INTO saved (user_id, message_id, saved_at) VALUES (?, ?, ?)",
            )?
            .execute(params![to_sql(user), to_sql(message), now_ms as i64])?
        } else {
            conn.prepare_cached("DELETE FROM saved WHERE user_id = ? AND message_id = ?")?
                .execute(params![to_sql(user), to_sql(message)])?
        };
        Ok(n > 0)
    }

    /// Everything a user has saved, newest first.
    ///
    /// Joined against membership rather than filtered afterwards, exactly as
    /// search is: leaving a channel must take its messages out of your saved
    /// list, or the list becomes a way to keep reading a private channel after
    /// being removed from it.
    pub fn saved_messages(&self, user: Id, limit: u32) -> Result<Vec<Message>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {cols} FROM saved s \
             JOIN messages m ON m.id = s.message_id \
             JOIN members mem ON mem.channel_id = m.channel_id AND mem.user_id = s.user_id \
             WHERE s.user_id = ? AND m.deleted = 0 \
             ORDER BY m.id DESC LIMIT ?",
            cols = crate::messages::MSG_COLS_Q,
        ))?;
        let mut messages: Vec<Message> = stmt
            .query_map(
                params![to_sql(user), limit as i64],
                crate::messages::map_message,
            )?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        drop(conn);

        self.hydrate(&mut messages, user)?;
        Ok(messages)
    }

    /// The ids a user has saved, for marking them in history. Subject to the
    /// same membership join as [`Store::saved_messages`].
    pub fn saved_ids(&self, user: Id) -> Result<Vec<Id>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(
            "SELECT s.message_id FROM saved s \
             JOIN messages m ON m.id = s.message_id \
             JOIN members mem ON mem.channel_id = m.channel_id AND mem.user_id = s.user_id \
             WHERE s.user_id = ? AND m.deleted = 0 ORDER BY s.message_id DESC",
        )?;
        Ok(stmt
            .query_map([to_sql(user)], |r| Ok(crate::from_sql(r.get(0)?)))?
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
                kind: ChannelKind::Private,
                name: "secrets",
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
    fn saving_is_idempotent_and_per_user() {
        let f = fx();
        let m = post(&f, "remember this");

        assert!(f.s.set_saved(f.alice, m, true, 1).unwrap());
        assert!(
            !f.s.set_saved(f.alice, m, true, 2).unwrap(),
            "second save is a no-op"
        );

        assert_eq!(f.s.saved_ids(f.alice).unwrap(), vec![m]);
        assert!(
            f.s.saved_ids(f.bob).unwrap().is_empty(),
            "one person's saves are not another's"
        );

        assert!(f.s.set_saved(f.alice, m, false, 3).unwrap());
        assert!(!f.s.set_saved(f.alice, m, false, 4).unwrap());
        assert!(f.s.saved_ids(f.alice).unwrap().is_empty());
    }

    #[test]
    fn saved_messages_come_back_hydrated_and_newest_first() {
        let f = fx();
        let (a, b) = (post(&f, "first"), post(&f, "second"));
        f.s.set_reaction(a, f.bob, "⭐", true).unwrap();
        f.s.set_saved(f.alice, a, true, 1).unwrap();
        f.s.set_saved(f.alice, b, true, 2).unwrap();

        let saved = f.s.saved_messages(f.alice, MAX_SAVED_PAGE).unwrap();
        assert_eq!(saved.iter().map(|m| m.id).collect::<Vec<_>>(), vec![b, a]);
        assert_eq!(saved[1].reactions.len(), 1, "must arrive hydrated");
    }

    #[test]
    fn leaving_a_channel_takes_its_messages_out_of_your_saved_list() {
        // Otherwise the saved list is a way to keep reading a private channel
        // after being removed from it.
        let f = fx();
        let m = post(&f, "the passphrase is");
        f.s.set_saved(f.bob, m, true, 1).unwrap();
        assert_eq!(f.s.saved_ids(f.bob).unwrap(), vec![m]);

        f.s.leave_channel(f.channel, f.bob).unwrap();
        assert!(f.s.saved_ids(f.bob).unwrap().is_empty());
        assert!(
            f.s.saved_messages(f.bob, MAX_SAVED_PAGE)
                .unwrap()
                .is_empty()
        );

        // Rejoining restores it — the save row was never destroyed, only hidden.
        f.s.join_channel(f.channel, f.bob, 2).unwrap();
        assert_eq!(f.s.saved_ids(f.bob).unwrap(), vec![m]);
    }

    #[test]
    fn a_deleted_message_drops_out_of_the_saved_list() {
        let f = fx();
        let m = post(&f, "temporary");
        f.s.set_saved(f.alice, m, true, 1).unwrap();
        f.s.delete_message(m, f.alice, false).unwrap();
        assert!(
            f.s.saved_messages(f.alice, MAX_SAVED_PAGE)
                .unwrap()
                .is_empty()
        );
        assert!(f.s.saved_ids(f.alice).unwrap().is_empty());
    }
}
