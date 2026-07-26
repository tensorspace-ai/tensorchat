//! Channels, membership, and read state.

use rusqlite::{OptionalExtension, Row, TransactionBehavior, params};
use tc_core::{Channel, ChannelKind, Id, ReadState};

use crate::{Error, Result, Store, from_sql, to_sql};

/// Unread and mention counts stop at this number. The UI renders anything at
/// or above it as "99+", so counting further is work nobody sees — and it
/// bounds the cost of a badge refresh on a channel with a huge backlog.
pub const UNREAD_CAP: u32 = 99;

const CHAN_COLS: &str = "id, kind, name, topic, created_by, archived, last_message";

fn map_channel(row: &Row<'_>) -> rusqlite::Result<Channel> {
    Ok(Channel {
        id: from_sql(row.get(0)?),
        kind: kind_from_i64(row.get(1)?),
        name: row.get(2)?,
        topic: row.get(3)?,
        created_by: from_sql(row.get(4)?),
        archived: row.get(5)?,
        members: Vec::new(),
        last_message: from_sql(row.get(6)?),
    })
}

fn kind_to_i64(k: ChannelKind) -> i64 {
    match k {
        ChannelKind::Public => 0,
        ChannelKind::Private => 1,
        ChannelKind::Dm => 2,
        ChannelKind::Group => 3,
    }
}

fn kind_from_i64(v: i64) -> ChannelKind {
    match v {
        1 => ChannelKind::Private,
        2 => ChannelKind::Dm,
        3 => ChannelKind::Group,
        // Unknown discriminants fall back to the least-privileged
        // interpretation rather than panicking on a forward-compatible row.
        _ => ChannelKind::Public,
    }
}

/// Deterministic key for a direct conversation: the sorted member ids.
///
/// Makes "open a DM with these people" a unique-index probe rather than a
/// set-equality search, and makes double-creation impossible under a race —
/// the second insert loses to the unique constraint.
fn dm_key(members: &[Id]) -> String {
    let mut ids: Vec<u64> = members.iter().map(|i| i.0).collect();
    ids.sort_unstable();
    ids.dedup();
    use std::fmt::Write as _;
    let mut s = String::with_capacity(ids.len() * 20);
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{id}");
    }
    s
}

pub struct NewChannel<'a> {
    pub id: Id,
    pub kind: ChannelKind,
    pub name: &'a str,
    pub topic: &'a str,
    pub created_by: Id,
    pub created_at: u64,
    /// Initial membership. The creator is added automatically if absent.
    pub members: Vec<Id>,
}

impl Store {
    /// Create a channel and its initial membership atomically.
    pub fn create_channel(&self, spec: NewChannel<'_>) -> Result<Channel> {
        let mut conn = self.conn()?;
        // IMMEDIATE, not the default DEFERRED. A deferred transaction that
        // reads first and only later writes must be failed with SQLITE_BUSY the
        // moment it tries to upgrade, because its read snapshot may already be
        // stale — `busy_timeout` cannot rescue it. Taking the write lock up
        // front means contending writers queue on the timeout instead.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let mut members = spec.members;
        if !members.contains(&spec.created_by) {
            members.push(spec.created_by);
        }
        let key = spec.kind.is_direct().then(|| dm_key(&members));

        tx.prepare_cached(
            "INSERT INTO channels (id, kind, name, topic, created_by, created_at, dm_key) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?
        .execute(params![
            to_sql(spec.id),
            kind_to_i64(spec.kind),
            spec.name,
            spec.topic,
            to_sql(spec.created_by),
            spec.created_at as i64,
            key,
        ])
        .map_err(|e| Error::from_sqlite(e, "that channel"))?;

        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO members (channel_id, user_id, joined_at) VALUES (?, ?, ?)",
            )?;
            for m in &members {
                stmt.execute(params![to_sql(spec.id), to_sql(*m), spec.created_at as i64])?;
            }
        }
        tx.commit()?;

        Ok(Channel {
            id: spec.id,
            kind: spec.kind,
            name: spec.name.to_string(),
            topic: spec.topic.to_string(),
            created_by: spec.created_by,
            archived: false,
            members: if spec.kind.is_direct() {
                members
            } else {
                Vec::new()
            },
            last_message: Id::ZERO,
        })
    }

    /// Find an existing direct conversation for exactly this member set, or
    /// create one.
    ///
    /// Racing callers converge on the same channel: the loser of the unique
    /// index re-reads the winner's row instead of failing.
    pub fn open_dm(&self, id: Id, creator: Id, members: Vec<Id>, now_ms: u64) -> Result<Channel> {
        let mut all = members;
        if !all.contains(&creator) {
            all.push(creator);
        }
        all.sort_unstable();
        all.dedup();
        if all.len() < 2 {
            return Err(Error::Invalid("a direct message needs at least two people"));
        }
        let key = dm_key(&all);

        if let Some(existing) = self.channel_by_dm_key(&key)? {
            return Ok(existing);
        }

        let kind = if all.len() == 2 {
            ChannelKind::Dm
        } else {
            ChannelKind::Group
        };
        match self.create_channel(NewChannel {
            id,
            kind,
            name: "",
            topic: "",
            created_by: creator,
            created_at: now_ms,
            members: all,
        }) {
            Ok(c) => Ok(c),
            // Someone else created it between our probe and our insert.
            Err(Error::Conflict(_)) => self.channel_by_dm_key(&key)?.ok_or(Error::NotFound),
            Err(e) => Err(e),
        }
    }

    fn channel_by_dm_key(&self, key: &str) -> Result<Option<Channel>> {
        let conn = self.conn()?;
        let found = conn
            .prepare_cached(&format!(
                "SELECT {CHAN_COLS} FROM channels WHERE dm_key = ?"
            ))?
            .query_row([key], map_channel)
            .optional()?;
        drop(conn);
        match found {
            Some(mut c) => {
                c.members = self.members(c.id)?;
                Ok(Some(c))
            }
            None => Ok(None),
        }
    }

    pub fn channel(&self, id: Id) -> Result<Channel> {
        let conn = self.conn()?;
        let mut c = conn
            .prepare_cached(&format!("SELECT {CHAN_COLS} FROM channels WHERE id = ?"))?
            .query_row([to_sql(id)], map_channel)
            .optional()?
            .ok_or(Error::NotFound)?;
        drop(conn);
        if c.kind.is_direct() {
            c.members = self.members(c.id)?;
        }
        Ok(c)
    }

    /// Every channel a user belongs to.
    ///
    /// Direct conversations arrive with their member list populated (clients
    /// render their title from it); named channels do not, since that list can
    /// be thousands of rows and is only needed when the channel is opened.
    pub fn channels_for_user(&self, user: Id) -> Result<Vec<Channel>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(
            "SELECT c.id, c.kind, c.name, c.topic, c.created_by, c.archived, c.last_message \
             FROM members m JOIN channels c ON c.id = m.channel_id \
             WHERE m.user_id = ? ORDER BY c.last_message DESC",
        )?;
        let mut channels: Vec<Channel> = stmt
            .query_map([to_sql(user)], map_channel)?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        drop(conn);

        for c in channels.iter_mut().filter(|c| c.kind.is_direct()) {
            c.members = self.members(c.id)?;
        }
        Ok(channels)
    }

    /// Public channels the user could join, for the browse view.
    pub fn public_channels(&self) -> Result<Vec<Channel>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {CHAN_COLS} FROM channels WHERE kind = 0 AND archived = 0 ORDER BY name"
        ))?;
        Ok(stmt
            .query_map([], map_channel)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn members(&self, channel: Id) -> Result<Vec<Id>> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare_cached("SELECT user_id FROM members WHERE channel_id = ? ORDER BY user_id")?;
        Ok(stmt
            .query_map([to_sql(channel)], |r| Ok(from_sql(r.get(0)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Membership check. This is the authorization primitive for every read and
    /// write in the system, so it is a single covered index probe.
    pub fn is_member(&self, channel: Id, user: Id) -> Result<bool> {
        let conn = self.conn()?;
        let found: Option<i64> = conn
            .prepare_cached("SELECT 1 FROM members WHERE channel_id = ? AND user_id = ?")?
            .query_row(params![to_sql(channel), to_sql(user)], |r| r.get(0))
            .optional()?;
        Ok(found.is_some())
    }

    /// Add a member. Idempotent; returns whether this call actually joined
    /// them, so the caller only broadcasts a real change.
    pub fn join_channel(&self, channel: Id, user: Id, now_ms: u64) -> Result<bool> {
        let conn = self.conn()?;
        let n = conn
            .prepare_cached(
                "INSERT OR IGNORE INTO members (channel_id, user_id, joined_at) VALUES (?, ?, ?)",
            )?
            .execute(params![to_sql(channel), to_sql(user), now_ms as i64])
            .map_err(|e| Error::from_sqlite(e, "that membership"))?;
        Ok(n > 0)
    }

    /// Remove a member. Idempotent; returns whether anything changed.
    pub fn leave_channel(&self, channel: Id, user: Id) -> Result<bool> {
        let conn = self.conn()?;
        let n = conn
            .prepare_cached("DELETE FROM members WHERE channel_id = ? AND user_id = ?")?
            .execute(params![to_sql(channel), to_sql(user)])?;
        Ok(n > 0)
    }

    /// Update channel metadata. `None` leaves a field untouched.
    pub fn update_channel(
        &self,
        id: Id,
        name: Option<&str>,
        topic: Option<&str>,
        archived: Option<bool>,
    ) -> Result<Channel> {
        {
            let conn = self.conn()?;
            // COALESCE keeps this a single statement regardless of which
            // subset of fields the caller supplied.
            conn.prepare_cached(
                "UPDATE channels SET name = COALESCE(?, name), topic = COALESCE(?, topic), \
                 archived = COALESCE(?, archived) WHERE id = ?",
            )?
            .execute(params![name, topic, archived, to_sql(id)])
            .map_err(|e| Error::from_sqlite(e, "that channel name"))?;
        }
        self.channel(id)
    }

    /// Read cursor and unread/mention counts for every channel the user is in.
    ///
    /// One statement for the whole sidebar. Both counts are computed from the
    /// user's stored cursor via index range scans, and each is capped at
    /// [`UNREAD_CAP`] + 1 rows so a long backlog cannot make login slow.
    pub fn read_states(&self, user: Id) -> Result<Vec<ReadState>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT m.channel_id, m.last_read,
                    (SELECT count(*) FROM (
                        SELECT 1 FROM messages msg
                         WHERE msg.channel_id = m.channel_id AND msg.id > m.last_read
                           AND msg.author_id <> m.user_id AND msg.deleted = 0
                         LIMIT {cap})),
                    (SELECT count(*) FROM (
                        SELECT 1 FROM mentions mn
                         WHERE mn.user_id = m.user_id AND mn.channel_id = m.channel_id
                           AND mn.message_id > m.last_read
                         LIMIT {cap}))
               FROM members m WHERE m.user_id = ?",
            cap = UNREAD_CAP + 1
        ))?;
        Ok(stmt
            .query_map([to_sql(user)], |r| {
                Ok(ReadState {
                    channel_id: from_sql(r.get(0)?),
                    last_read: from_sql(r.get(1)?),
                    unread: r.get::<_, i64>(2)? as u32,
                    mentions: r.get::<_, i64>(3)? as u32,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Advance a read cursor and return the resulting state.
    ///
    /// `max(last_read, ?)` keeps the cursor monotonic: an out-of-order or
    /// replayed frame from a background tab cannot un-read messages.
    pub fn mark_read(&self, channel: Id, user: Id, up_to: Id) -> Result<ReadState> {
        let conn = self.conn()?;
        let n = conn
            .prepare_cached(
                "UPDATE members SET last_read = max(last_read, ?) \
                 WHERE channel_id = ? AND user_id = ?",
            )?
            .execute(params![to_sql(up_to), to_sql(channel), to_sql(user)])?;
        if n == 0 {
            return Err(Error::Forbidden);
        }
        drop(conn);
        self.read_state(channel, user)
    }

    /// Read state for one channel.
    pub fn read_state(&self, channel: Id, user: Id) -> Result<ReadState> {
        let conn = self.conn()?;
        conn.prepare_cached(&format!(
            "SELECT m.last_read,
                    (SELECT count(*) FROM (
                        SELECT 1 FROM messages msg
                         WHERE msg.channel_id = m.channel_id AND msg.id > m.last_read
                           AND msg.author_id <> m.user_id AND msg.deleted = 0
                         LIMIT {cap})),
                    (SELECT count(*) FROM (
                        SELECT 1 FROM mentions mn
                         WHERE mn.user_id = m.user_id AND mn.channel_id = m.channel_id
                           AND mn.message_id > m.last_read
                         LIMIT {cap}))
               FROM members m WHERE m.channel_id = ? AND m.user_id = ?",
            cap = UNREAD_CAP + 1
        ))?
        .query_row(params![to_sql(channel), to_sql(user)], |r| {
            Ok(ReadState {
                channel_id: channel,
                last_read: from_sql(r.get(0)?),
                unread: r.get::<_, i64>(1)? as u32,
                mentions: r.get::<_, i64>(2)? as u32,
            })
        })
        .optional()?
        .ok_or(Error::Forbidden)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tc_core::IdGen;

    struct Fx {
        s: Store,
        g: IdGen,
        alice: Id,
        bob: Id,
    }

    fn fx() -> Fx {
        let s = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        let alice = s.create_user(g.next(), "alice", "Alice", "h").unwrap().id;
        let bob = s.create_user(g.next(), "bob", "Bob", "h").unwrap().id;
        Fx { s, g, alice, bob }
    }

    fn public(f: &Fx, name: &str) -> Channel {
        f.s.create_channel(NewChannel {
            id: f.g.next(),
            kind: ChannelKind::Public,
            name,
            topic: "",
            created_by: f.alice,
            created_at: 1,
            members: vec![],
        })
        .unwrap()
    }

    #[test]
    fn creator_is_a_member_even_if_not_listed() {
        let f = fx();
        let c = public(&f, "general");
        assert!(f.s.is_member(c.id, f.alice).unwrap());
        assert!(!f.s.is_member(c.id, f.bob).unwrap());
    }

    #[test]
    fn duplicate_channel_name_conflicts_but_empty_names_do_not() {
        let f = fx();
        public(&f, "general");
        let err = f.s.create_channel(NewChannel {
            id: f.g.next(),
            kind: ChannelKind::Public,
            name: "general",
            topic: "",
            created_by: f.alice,
            created_at: 1,
            members: vec![],
        });
        assert!(matches!(err, Err(Error::Conflict(_))));

        // DMs all have an empty name; the partial unique index must not treat
        // that as a collision.
        f.s.open_dm(f.g.next(), f.alice, vec![f.bob], 1).unwrap();
        let carol = f.s.create_user(f.g.next(), "carol", "C", "h").unwrap().id;
        f.s.open_dm(f.g.next(), f.alice, vec![carol], 1).unwrap();
    }

    #[test]
    fn open_dm_is_idempotent_regardless_of_member_order() {
        let f = fx();
        let a = f.s.open_dm(f.g.next(), f.alice, vec![f.bob], 1).unwrap();
        // Same pair, opened from the other side.
        let b = f.s.open_dm(f.g.next(), f.bob, vec![f.alice], 1).unwrap();
        assert_eq!(a.id, b.id, "a DM must not be duplicated");
        assert_eq!(a.kind, ChannelKind::Dm);
        assert_eq!(b.members, vec![f.alice.min(f.bob), f.alice.max(f.bob)]);
    }

    #[test]
    fn three_person_dm_is_a_group_and_distinct_from_the_pair() {
        let f = fx();
        let carol = f.s.create_user(f.g.next(), "carol", "C", "h").unwrap().id;
        let pair = f.s.open_dm(f.g.next(), f.alice, vec![f.bob], 1).unwrap();
        let trio =
            f.s.open_dm(f.g.next(), f.alice, vec![f.bob, carol], 1)
                .unwrap();
        assert_ne!(pair.id, trio.id);
        assert_eq!(trio.kind, ChannelKind::Group);
    }

    #[test]
    fn dm_with_only_yourself_is_rejected() {
        let f = fx();
        assert!(matches!(
            f.s.open_dm(f.g.next(), f.alice, vec![f.alice], 1),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn join_and_leave_are_idempotent() {
        let f = fx();
        let c = public(&f, "general");
        assert!(f.s.join_channel(c.id, f.bob, 2).unwrap());
        assert!(
            !f.s.join_channel(c.id, f.bob, 2).unwrap(),
            "second join is a no-op"
        );
        assert!(f.s.leave_channel(c.id, f.bob).unwrap());
        assert!(!f.s.leave_channel(c.id, f.bob).unwrap());
    }

    #[test]
    fn channels_for_user_populates_dm_members_only() {
        let f = fx();
        public(&f, "general");
        f.s.open_dm(f.g.next(), f.alice, vec![f.bob], 1).unwrap();
        let list = f.s.channels_for_user(f.alice).unwrap();
        assert_eq!(list.len(), 2);
        let dm = list.iter().find(|c| c.kind.is_direct()).unwrap();
        let named = list.iter().find(|c| !c.kind.is_direct()).unwrap();
        assert_eq!(dm.members.len(), 2);
        assert!(named.members.is_empty(), "named channel membership is lazy");
    }

    #[test]
    fn read_cursor_only_moves_forward() {
        let f = fx();
        let c = public(&f, "general");
        f.s.mark_read(c.id, f.alice, Id(500)).unwrap();
        let st = f.s.mark_read(c.id, f.alice, Id(100)).unwrap();
        assert_eq!(st.last_read, Id(500), "cursor must not rewind");
    }

    #[test]
    fn marking_read_on_a_channel_you_are_not_in_is_forbidden() {
        let f = fx();
        let c = public(&f, "general");
        assert!(matches!(
            f.s.mark_read(c.id, f.bob, Id(1)),
            Err(Error::Forbidden)
        ));
    }

    #[test]
    fn update_channel_leaves_unspecified_fields_alone() {
        let f = fx();
        let c = public(&f, "general");
        let up =
            f.s.update_channel(c.id, None, Some("standup notes"), None)
                .unwrap();
        assert_eq!(up.name, "general");
        assert_eq!(up.topic, "standup notes");
        assert!(!up.archived);
    }

    #[test]
    fn public_directory_excludes_private_and_archived() {
        let f = fx();
        let open = public(&f, "general");
        f.s.create_channel(NewChannel {
            id: f.g.next(),
            kind: ChannelKind::Private,
            name: "secrets",
            topic: "",
            created_by: f.alice,
            created_at: 1,
            members: vec![],
        })
        .unwrap();
        let gone = public(&f, "old");
        f.s.update_channel(gone.id, None, None, Some(true)).unwrap();

        let dir = f.s.public_channels().unwrap();
        assert_eq!(dir.iter().map(|c| c.id).collect::<Vec<_>>(), vec![open.id]);
    }
}
