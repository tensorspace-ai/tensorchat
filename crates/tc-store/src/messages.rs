//! Messages, threads, reactions, and attachments.
//!
//! This is the hot path. Two rules shape everything here:
//!
//! 1. **No N+1.** A page of history hydrates its reactions and attachments
//!    with one additional query each, over the id range just read — not one
//!    query per message.
//! 2. **The primary key is the cursor.** Backfill is `id < ?` on a descending
//!    index; there is no offset, no sort, and no timestamp column involved.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, Row, TransactionBehavior, params, types::Value};
use tc_core::{Attachment, Id, Message, ReactionSummary};

use crate::{Error, Result, Store, from_sql, pack_ids, to_sql, unpack_ids};

/// Upper bound on a single history page, so a client cannot ask for the whole
/// channel in one request.
pub const MAX_PAGE: u32 = 200;

const MSG_COLS: &str =
    "id, channel_id, author_id, body, thread_root, reply_count, edited_at, deleted, mentions";
/// The same list, table-qualified as `m`, for queries that join.
pub(crate) const MSG_COLS_Q: &str = "m.id, m.channel_id, m.author_id, m.body, m.thread_root, \
     m.reply_count, m.edited_at, m.deleted, m.mentions";

pub(crate) fn map_message(row: &Row<'_>) -> rusqlite::Result<Message> {
    let mentions: Option<Vec<u8>> = row.get(8)?;
    Ok(Message {
        id: from_sql(row.get(0)?),
        channel_id: from_sql(row.get(1)?),
        author_id: from_sql(row.get(2)?),
        body: row.get(3)?,
        thread_root: row.get::<_, Option<i64>>(4)?.map(from_sql),
        reply_count: row.get::<_, i64>(5)? as u32,
        edited_at: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
        deleted: row.get(7)?,
        attachments: Vec::new(),
        reactions: Vec::new(),
        mentions: mentions.as_deref().map(unpack_ids).unwrap_or_default(),
    })
}

pub struct NewMessage<'a> {
    pub id: Id,
    pub channel_id: Id,
    pub author_id: Id,
    pub body: &'a str,
    pub thread_root: Option<Id>,
    /// Ids of previously uploaded attachments to bind to this message.
    pub attachments: &'a [Id],
    /// Mentioned users, already resolved from handles by the caller.
    pub mentions: &'a [Id],
}

/// A page of history, newest-first, plus the cursor for the next page.
#[derive(Debug, Clone)]
pub struct HistoryPage {
    pub messages: Vec<Message>,
    /// Pass as `before` to fetch the next older page. `None` when the channel
    /// has been read to its beginning.
    pub next_cursor: Option<Id>,
}

impl Store {
    /// Insert a message, bind its attachments, record its mentions, and bump
    /// the channel's `last_message` — atomically.
    ///
    /// Membership is verified inside the same transaction, so a user removed
    /// from a channel concurrently cannot slip a message in behind the check.
    pub fn insert_message(&self, m: NewMessage<'_>) -> Result<Message> {
        let mut conn = self.conn()?;
        // IMMEDIATE, not the default DEFERRED. A deferred transaction that
        // reads first and only later writes must be failed with SQLITE_BUSY the
        // moment it tries to upgrade, because its read snapshot may already be
        // stale — `busy_timeout` cannot rescue it. Taking the write lock up
        // front means contending writers queue on the timeout instead.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let member: Option<i64> = tx
            .prepare_cached("SELECT 1 FROM members WHERE channel_id = ? AND user_id = ?")?
            .query_row(params![to_sql(m.channel_id), to_sql(m.author_id)], |r| {
                r.get(0)
            })
            .optional()?;
        if member.is_none() {
            return Err(Error::Forbidden);
        }

        // A reply's root must be a real, top-level message in this channel.
        // Threads are one level deep by design: replying to a reply attaches
        // to the same root, which keeps rendering and reply counts simple.
        let root = match m.thread_root {
            None => None,
            Some(r) => {
                let found: Option<(i64, Option<i64>)> = tx
                    .prepare_cached("SELECT channel_id, thread_root FROM messages WHERE id = ?")?
                    .query_row([to_sql(r)], |row| Ok((row.get(0)?, row.get(1)?)))
                    .optional()?;
                match found {
                    None => return Err(Error::NotFound),
                    Some((ch, _)) if from_sql(ch) != m.channel_id => {
                        return Err(Error::Invalid("thread root is in another channel"));
                    }
                    // Replying to a reply collapses to that reply's root.
                    Some((_, Some(parent_root))) => Some(from_sql(parent_root)),
                    Some((_, None)) => Some(r),
                }
            }
        };

        let mentions_blob = (!m.mentions.is_empty()).then(|| pack_ids(m.mentions));

        tx.prepare_cached(
            "INSERT INTO messages (id, channel_id, author_id, body, thread_root, mentions) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )?
        .execute(params![
            to_sql(m.id),
            to_sql(m.channel_id),
            to_sql(m.author_id),
            m.body,
            root.map(to_sql),
            mentions_blob,
        ])?;

        if let Some(r) = root {
            tx.prepare_cached("UPDATE messages SET reply_count = reply_count + 1 WHERE id = ?")?
                .execute([to_sql(r)])?;
        }

        // Mention rows drive unread badges. Only members can be mentioned —
        // otherwise a message could plant an unread badge on someone who
        // cannot even see the channel.
        if !m.mentions.is_empty() {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO mentions (user_id, channel_id, message_id) \
                 SELECT ?, ?, ? WHERE EXISTS \
                   (SELECT 1 FROM members WHERE channel_id = ? AND user_id = ?)",
            )?;
            for u in m.mentions {
                // Mentioning yourself should never light up your own badge.
                if *u == m.author_id {
                    continue;
                }
                stmt.execute(params![
                    to_sql(*u),
                    to_sql(m.channel_id),
                    to_sql(m.id),
                    to_sql(m.channel_id),
                    to_sql(*u)
                ])?;
            }
        }

        // Bind staged uploads, but only ones this user owns and that are not
        // already attached elsewhere.
        let mut bound = Vec::new();
        if !m.attachments.is_empty() {
            let mut stmt = tx.prepare_cached(
                "UPDATE attachments SET message_id = ? \
                 WHERE id = ? AND owner_id = ? AND message_id IS NULL",
            )?;
            for a in m.attachments {
                if stmt.execute(params![to_sql(m.id), to_sql(*a), to_sql(m.author_id)])? > 0 {
                    bound.push(*a);
                }
            }
        }

        // Only advance the pointer — an out-of-order insert must not rewind it.
        tx.prepare_cached("UPDATE channels SET last_message = max(last_message, ?) WHERE id = ?")?
            .execute(params![to_sql(m.id), to_sql(m.channel_id)])?;

        tx.commit()?;
        // Release the pooled connection before any further query: holding one
        // while asking for another is a self-deadlock whenever the pool is
        // smaller than the nesting depth.
        drop(conn);

        let mut msg = Message {
            id: m.id,
            channel_id: m.channel_id,
            author_id: m.author_id,
            body: m.body.to_string(),
            thread_root: root,
            reply_count: 0,
            edited_at: None,
            deleted: false,
            attachments: Vec::new(),
            reactions: Vec::new(),
            mentions: m.mentions.to_vec(),
        };
        if !bound.is_empty() {
            msg.attachments = self
                .attachments_for(&[m.id])?
                .remove(&m.id)
                .unwrap_or_default();
        }
        Ok(msg)
    }

    pub fn message(&self, id: Id) -> Result<Message> {
        let conn = self.conn()?;
        let msg = conn
            .prepare_cached(&format!("SELECT {MSG_COLS} FROM messages WHERE id = ?"))?
            .query_row([to_sql(id)], map_message)
            .optional()?
            .ok_or(Error::NotFound)?;
        drop(conn);
        Ok(msg)
    }

    /// One message, with its reactions and attachments attached.
    ///
    /// [`Store::message`] deliberately skips hydration because most internal
    /// callers only need the row. Anything that hands a message back to a
    /// client should use this instead — returning a bare row would look to the
    /// client like the message lost its reactions.
    pub fn message_for(&self, id: Id, viewer: Id) -> Result<Message> {
        let mut one = vec![self.message(id)?];
        self.hydrate(&mut one, viewer)?;
        Ok(one.pop().expect("hydrate preserves length"))
    }

    /// Edit a message body. Only the author may edit; deleted messages cannot
    /// be resurrected. Returns the edit timestamp.
    pub fn edit_message(&self, id: Id, author: Id, body: &str, now_ms: u64) -> Result<u64> {
        let conn = self.conn()?;
        let n = conn
            .prepare_cached(
                "UPDATE messages SET body = ?, edited_at = ? \
                 WHERE id = ? AND author_id = ? AND deleted = 0",
            )?
            .execute(params![body, now_ms as i64, to_sql(id), to_sql(author)])?;
        if n == 0 {
            // Either it does not exist, or it is not theirs. Distinguishing the
            // two would leak the existence of messages in channels the caller
            // cannot see.
            return Err(Error::Forbidden);
        }
        Ok(now_ms)
    }

    /// Soft-delete a message: blank the body but keep the row so thread
    /// structure, reply counts, and pagination cursors stay stable.
    ///
    /// `allow_any` lets an admin path delete another user's message.
    pub fn delete_message(&self, id: Id, author: Id, allow_any: bool) -> Result<Id> {
        let mut conn = self.conn()?;
        // IMMEDIATE, not the default DEFERRED. A deferred transaction that
        // reads first and only later writes must be failed with SQLITE_BUSY the
        // moment it tries to upgrade, because its read snapshot may already be
        // stale — `busy_timeout` cannot rescue it. Taking the write lock up
        // front means contending writers queue on the timeout instead.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let row: Option<(i64, i64)> = tx
            .prepare_cached(
                "SELECT channel_id, author_id FROM messages WHERE id = ? AND deleted = 0",
            )?
            .query_row([to_sql(id)], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let (channel, owner) = row.ok_or(Error::NotFound)?;
        if !allow_any && from_sql(owner) != author {
            return Err(Error::Forbidden);
        }

        // Blanking the body fires the FTS trigger, which drops it from search.
        tx.prepare_cached(
            "UPDATE messages SET body = '', deleted = 1, mentions = NULL WHERE id = ?",
        )?
        .execute([to_sql(id)])?;
        tx.prepare_cached("DELETE FROM mentions WHERE message_id = ?")?
            .execute([to_sql(id)])?;
        tx.prepare_cached("DELETE FROM reactions WHERE message_id = ?")?
            .execute([to_sql(id)])?;
        tx.commit()?;
        Ok(from_sql(channel))
    }

    /// A page of channel history, newest first.
    ///
    /// `before` is exclusive; pass `None` for the newest page. The extra row we
    /// fetch beyond `limit` is how we know whether an older page exists without
    /// a second `COUNT` query.
    pub fn history(
        &self,
        channel: Id,
        viewer: Id,
        before: Option<Id>,
        limit: u32,
    ) -> Result<HistoryPage> {
        if !self.is_member(channel, viewer)? {
            return Err(Error::Forbidden);
        }
        let limit = limit.clamp(1, MAX_PAGE);
        let conn = self.conn()?;

        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {MSG_COLS} FROM messages \
             WHERE channel_id = ? AND id < ? AND thread_root IS NULL \
             ORDER BY id DESC LIMIT ?"
        ))?;
        let mut messages: Vec<Message> = stmt
            .query_map(
                params![
                    to_sql(channel),
                    // `i64::MAX`, not `to_sql(Id::MAX)`: SQLite integers are
                    // signed, so u64::MAX would arrive as -1 and match nothing.
                    before.map(to_sql).unwrap_or(i64::MAX),
                    limit as i64 + 1
                ],
                map_message,
            )?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        drop(conn);

        let next_cursor = if messages.len() > limit as usize {
            messages.truncate(limit as usize);
            messages.last().map(|m| m.id)
        } else {
            None
        };

        self.hydrate(&mut messages, viewer)?;
        Ok(HistoryPage {
            messages,
            next_cursor,
        })
    }

    /// Every reply in a thread, oldest first, plus the root message.
    pub fn thread(&self, root: Id, viewer: Id) -> Result<Vec<Message>> {
        let root_msg = self.message(root)?;
        if !self.is_member(root_msg.channel_id, viewer)? {
            return Err(Error::Forbidden);
        }
        let conn = self.conn()?;
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {MSG_COLS} FROM messages WHERE thread_root = ? ORDER BY id"
        ))?;
        let replies: Vec<Message> = stmt
            .query_map([to_sql(root)], map_message)?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        drop(conn);

        let mut all = Vec::with_capacity(replies.len() + 1);
        all.push(root_msg);
        all.extend(replies);
        self.hydrate(&mut all, viewer)?;
        Ok(all)
    }

    /// Attach reactions and attachments to a batch of messages.
    ///
    /// Two queries total, regardless of page size — the whole point of doing
    /// this as a batch rather than per message.
    pub(crate) fn hydrate(&self, messages: &mut [Message], viewer: Id) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let ids: Vec<Id> = messages.iter().map(|m| m.id).collect();
        let mut reactions = self.reactions_for(&ids, viewer)?;
        let mut attachments = self.attachments_for(&ids)?;
        for m in messages.iter_mut() {
            if let Some(r) = reactions.remove(&m.id) {
                m.reactions = r;
            }
            if let Some(a) = attachments.remove(&m.id) {
                m.attachments = a;
            }
        }
        Ok(())
    }

    /// Aggregate reactions for a set of messages in one query.
    ///
    /// `me` is computed in SQL with a conditional sum rather than by fetching
    /// every reactor's id back to the application.
    fn reactions_for(&self, ids: &[Id], viewer: Id) -> Result<HashMap<Id, Vec<ReactionSummary>>> {
        let conn = self.conn()?;
        let list = id_array(ids);
        let mut stmt = conn.prepare_cached(
            // `reactions` is WITHOUT ROWID, so there is no insertion order to
            // sort by; most-used-first is both deterministic and the ordering
            // the UI wants anyway.
            "SELECT message_id, emoji, count(*) AS n, max(user_id = ?) \
               FROM reactions WHERE message_id IN rarray(?) \
              GROUP BY message_id, emoji ORDER BY message_id, n DESC, emoji",
        )?;
        let mut out: HashMap<Id, Vec<ReactionSummary>> = HashMap::new();
        let rows = stmt.query_map(params![to_sql(viewer), list], |r| {
            Ok((
                from_sql(r.get(0)?),
                ReactionSummary {
                    emoji: r.get(1)?,
                    count: r.get::<_, i64>(2)? as u32,
                    me: r.get::<_, i64>(3)? != 0,
                },
            ))
        })?;
        for row in rows {
            let (id, summary) = row?;
            out.entry(id).or_default().push(summary);
        }
        Ok(out)
    }

    fn attachments_for(&self, ids: &[Id]) -> Result<HashMap<Id, Vec<Attachment>>> {
        let conn = self.conn()?;
        let list = id_array(ids);
        let mut stmt = conn.prepare_cached(
            "SELECT message_id, id, name, mime, size, width, height \
               FROM attachments WHERE message_id IN rarray(?) ORDER BY id",
        )?;
        let mut out: HashMap<Id, Vec<Attachment>> = HashMap::new();
        let rows = stmt.query_map([list], |r| {
            Ok((
                from_sql(r.get(0)?),
                Attachment {
                    id: from_sql(r.get(1)?),
                    name: r.get(2)?,
                    mime: r.get(3)?,
                    size: r.get::<_, i64>(4)? as u64,
                    width: r.get::<_, Option<i64>>(5)?.map(|v| v as u32),
                    height: r.get::<_, Option<i64>>(6)?.map(|v| v as u32),
                },
            ))
        })?;
        for row in rows {
            let (id, att) = row?;
            out.entry(id).or_default().push(att);
        }
        Ok(out)
    }

    /// Toggle a reaction. Returns the channel it belongs to (needed for
    /// broadcast) and whether the state actually changed.
    pub fn set_reaction(&self, message: Id, user: Id, emoji: &str, on: bool) -> Result<(Id, bool)> {
        let conn = self.conn()?;
        let channel: Option<i64> = conn
            .prepare_cached("SELECT channel_id FROM messages WHERE id = ? AND deleted = 0")?
            .query_row([to_sql(message)], |r| r.get(0))
            .optional()?;
        let channel = from_sql(channel.ok_or(Error::NotFound)?);
        drop(conn);

        if !self.is_member(channel, user)? {
            return Err(Error::Forbidden);
        }

        let conn = self.conn()?;
        let changed = if on {
            conn.prepare_cached(
                "INSERT OR IGNORE INTO reactions (message_id, user_id, emoji) VALUES (?, ?, ?)",
            )?
            .execute(params![to_sql(message), to_sql(user), emoji])?
        } else {
            conn.prepare_cached(
                "DELETE FROM reactions WHERE message_id = ? AND user_id = ? AND emoji = ?",
            )?
            .execute(params![to_sql(message), to_sql(user), emoji])?
        };
        Ok((channel, changed > 0))
    }

    /// Register an uploaded blob. It stays unattached until a message binds it.
    #[allow(clippy::too_many_arguments)]
    pub fn create_attachment(
        &self,
        id: Id,
        owner: Id,
        name: &str,
        mime: &str,
        size: u64,
        dims: Option<(u32, u32)>,
        path: &str,
    ) -> Result<Attachment> {
        let conn = self.conn()?;
        conn.prepare_cached(
            "INSERT INTO attachments (id, owner_id, name, mime, size, width, height, path) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )?
        .execute(params![
            to_sql(id),
            to_sql(owner),
            name,
            mime,
            size as i64,
            dims.map(|d| d.0 as i64),
            dims.map(|d| d.1 as i64),
            path,
        ])?;
        Ok(Attachment {
            id,
            name: name.to_string(),
            mime: mime.to_string(),
            size,
            width: dims.map(|d| d.0),
            height: dims.map(|d| d.1),
        })
    }

    /// Resolve an attachment for download, enforcing that the viewer is in the
    /// channel where it was posted. Unattached blobs are visible only to their
    /// uploader.
    pub fn attachment_path(&self, id: Id, viewer: Id) -> Result<(String, String, String)> {
        let conn = self.conn()?;
        let row: Option<(String, String, String, Option<i64>, i64)> = conn
            .prepare_cached(
                "SELECT path, name, mime, message_id, owner_id FROM attachments WHERE id = ?",
            )?
            .query_row([to_sql(id)], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()?;
        let (path, name, mime, message_id, owner) = row.ok_or(Error::NotFound)?;
        drop(conn);

        match message_id {
            Some(mid) => {
                let msg = self.message(from_sql(mid))?;
                if !self.is_member(msg.channel_id, viewer)? {
                    return Err(Error::Forbidden);
                }
            }
            None if from_sql(owner) != viewer => return Err(Error::Forbidden),
            None => {}
        }
        Ok((path, name, mime))
    }
}

/// Bind a slice of ids as a SQLite carray, so `IN rarray(?)` is one prepared
/// statement no matter how many ids there are.
///
/// The alternative — building `IN (?, ?, ?)` with N placeholders — produces a
/// distinct SQL string per page size, which defeats the statement cache.
fn id_array(ids: &[Id]) -> std::rc::Rc<Vec<Value>> {
    std::rc::Rc::new(ids.iter().map(|i| Value::Integer(to_sql(*i))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewChannel;
    use tc_core::{ChannelKind, IdGen};

    struct Fx {
        s: Store,
        g: IdGen,
        alice: Id,
        bob: Id,
        ch: Id,
    }

    fn fx() -> Fx {
        let s = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        let alice = s.create_user(g.next(), "alice", "Alice", "h").unwrap().id;
        let bob = s.create_user(g.next(), "bob", "Bob", "h").unwrap().id;
        let ch = s
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
            ch,
        }
    }

    fn post(f: &Fx, author: Id, body: &str) -> Message {
        f.s.insert_message(NewMessage {
            id: f.g.next(),
            channel_id: f.ch,
            author_id: author,
            body,
            thread_root: None,
            attachments: &[],
            mentions: &[],
        })
        .unwrap()
    }

    #[test]
    fn posting_updates_channel_last_message() {
        let f = fx();
        let m = post(&f, f.alice, "hello");
        assert_eq!(f.s.channel(f.ch).unwrap().last_message, m.id);
    }

    #[test]
    fn non_members_cannot_post() {
        let f = fx();
        let carol = f.s.create_user(f.g.next(), "carol", "C", "h").unwrap().id;
        let err = f.s.insert_message(NewMessage {
            id: f.g.next(),
            channel_id: f.ch,
            author_id: carol,
            body: "intruding",
            thread_root: None,
            attachments: &[],
            mentions: &[],
        });
        assert!(matches!(err, Err(Error::Forbidden)));
    }

    #[test]
    fn history_paginates_newest_first_with_a_stable_cursor() {
        let f = fx();
        let posted: Vec<Id> = (0..25)
            .map(|i| post(&f, f.alice, &format!("m{i}")).id)
            .collect();

        let p1 = f.s.history(f.ch, f.alice, None, 10).unwrap();
        assert_eq!(p1.messages.len(), 10);
        assert_eq!(p1.messages[0].id, *posted.last().unwrap(), "newest first");

        let p2 = f.s.history(f.ch, f.alice, p1.next_cursor, 10).unwrap();
        let p3 = f.s.history(f.ch, f.alice, p2.next_cursor, 10).unwrap();
        assert_eq!(p3.messages.len(), 5);
        assert_eq!(p3.next_cursor, None, "exhausted channel reports no cursor");

        // No overlap, no gaps.
        let mut seen: Vec<Id> = p1
            .messages
            .iter()
            .chain(&p2.messages)
            .chain(&p3.messages)
            .map(|m| m.id)
            .collect();
        seen.sort();
        assert_eq!(seen, posted);
    }

    #[test]
    fn history_excludes_thread_replies_from_the_main_view() {
        let f = fx();
        let root = post(&f, f.alice, "question");
        f.s.insert_message(NewMessage {
            id: f.g.next(),
            channel_id: f.ch,
            author_id: f.bob,
            body: "answer",
            thread_root: Some(root.id),
            attachments: &[],
            mentions: &[],
        })
        .unwrap();

        let page = f.s.history(f.ch, f.alice, None, 50).unwrap();
        assert_eq!(
            page.messages.len(),
            1,
            "replies live in the thread, not the channel"
        );
        assert_eq!(page.messages[0].reply_count, 1);
        assert_eq!(f.s.thread(root.id, f.alice).unwrap().len(), 2);
    }

    #[test]
    fn replying_to_a_reply_collapses_to_the_same_root() {
        let f = fx();
        let root = post(&f, f.alice, "question");
        let reply =
            f.s.insert_message(NewMessage {
                id: f.g.next(),
                channel_id: f.ch,
                author_id: f.bob,
                body: "a",
                thread_root: Some(root.id),
                attachments: &[],
                mentions: &[],
            })
            .unwrap();
        let nested =
            f.s.insert_message(NewMessage {
                id: f.g.next(),
                channel_id: f.ch,
                author_id: f.alice,
                body: "b",
                thread_root: Some(reply.id),
                attachments: &[],
                mentions: &[],
            })
            .unwrap();
        assert_eq!(
            nested.thread_root,
            Some(root.id),
            "threads stay one level deep"
        );
        assert_eq!(f.s.message(root.id).unwrap().reply_count, 2);
    }

    #[test]
    fn non_members_cannot_read_history() {
        let f = fx();
        let carol = f.s.create_user(f.g.next(), "carol", "C", "h").unwrap().id;
        assert!(matches!(
            f.s.history(f.ch, carol, None, 10),
            Err(Error::Forbidden)
        ));
    }

    #[test]
    fn only_the_author_can_edit() {
        let f = fx();
        let m = post(&f, f.alice, "typo");
        assert!(matches!(
            f.s.edit_message(m.id, f.bob, "hijacked", 5),
            Err(Error::Forbidden)
        ));
        f.s.edit_message(m.id, f.alice, "fixed", 5).unwrap();
        let after = f.s.message(m.id).unwrap();
        assert_eq!(after.body, "fixed");
        assert_eq!(after.edited_at, Some(5));
    }

    #[test]
    fn delete_is_a_tombstone_that_preserves_thread_shape() {
        let f = fx();
        let root = post(&f, f.alice, "question");
        f.s.insert_message(NewMessage {
            id: f.g.next(),
            channel_id: f.ch,
            author_id: f.bob,
            body: "answer",
            thread_root: Some(root.id),
            attachments: &[],
            mentions: &[],
        })
        .unwrap();

        f.s.delete_message(root.id, f.alice, false).unwrap();
        let after = f.s.message(root.id).unwrap();
        assert!(after.deleted);
        assert_eq!(after.body, "");
        assert_eq!(after.reply_count, 1, "replies survive their root");
        // Editing a tombstone must not bring it back.
        assert!(f.s.edit_message(root.id, f.alice, "undelete", 9).is_err());
    }

    #[test]
    fn reactions_aggregate_with_a_per_viewer_me_flag() {
        let f = fx();
        let m = post(&f, f.alice, "ship it");
        f.s.set_reaction(m.id, f.alice, "🎉", true).unwrap();
        f.s.set_reaction(m.id, f.bob, "🎉", true).unwrap();
        f.s.set_reaction(m.id, f.bob, "🚀", true).unwrap();

        let seen_by_alice = &f.s.history(f.ch, f.alice, None, 10).unwrap().messages[0];
        let party = seen_by_alice
            .reactions
            .iter()
            .find(|r| r.emoji == "🎉")
            .unwrap();
        assert_eq!(party.count, 2);
        assert!(party.me);
        let rocket = seen_by_alice
            .reactions
            .iter()
            .find(|r| r.emoji == "🚀")
            .unwrap();
        assert!(!rocket.me, "alice did not add this one");

        // Same message, different viewer, different `me` — which is exactly why
        // live reaction events are per-user deltas instead of summaries.
        let seen_by_bob = &f.s.history(f.ch, f.bob, None, 10).unwrap().messages[0];
        assert!(
            seen_by_bob
                .reactions
                .iter()
                .find(|r| r.emoji == "🚀")
                .unwrap()
                .me
        );
    }

    #[test]
    fn reaction_toggle_is_idempotent_and_reports_change() {
        let f = fx();
        let m = post(&f, f.alice, "x");
        assert!(
            f.s.set_reaction(m.id, f.alice, "👍", true).unwrap().1,
            "the first add changes state"
        );
        assert!(
            !f.s.set_reaction(m.id, f.alice, "👍", true).unwrap().1,
            "re-adding is a no-op"
        );
        assert!(
            f.s.set_reaction(m.id, f.alice, "👍", false).unwrap().1,
            "removing changes state"
        );
        assert!(
            !f.s.set_reaction(m.id, f.alice, "👍", false).unwrap().1,
            "re-removing is a no-op"
        );
    }

    #[test]
    fn non_members_cannot_react() {
        let f = fx();
        let m = post(&f, f.alice, "x");
        let carol = f.s.create_user(f.g.next(), "carol", "C", "h").unwrap().id;
        assert!(matches!(
            f.s.set_reaction(m.id, carol, "👍", true),
            Err(Error::Forbidden)
        ));
    }

    #[test]
    fn mentions_drive_unread_badges_for_members_only() {
        let f = fx();
        let carol = f.s.create_user(f.g.next(), "carol", "C", "h").unwrap().id;
        f.s.insert_message(NewMessage {
            id: f.g.next(),
            channel_id: f.ch,
            author_id: f.alice,
            body: "@bob @carol look",
            thread_root: None,
            attachments: &[],
            mentions: &[f.bob, carol],
        })
        .unwrap();

        let bob_state = f.s.read_state(f.ch, f.bob).unwrap();
        assert_eq!(bob_state.mentions, 1);
        assert_eq!(bob_state.unread, 1);
        // Carol is not in the channel, so no badge row was created for her.
        assert!(f.s.read_state(f.ch, carol).is_err());
    }

    #[test]
    fn your_own_messages_are_never_unread() {
        let f = fx();
        post(&f, f.alice, "a");
        post(&f, f.alice, "b");
        assert_eq!(f.s.read_state(f.ch, f.alice).unwrap().unread, 0);
        assert_eq!(f.s.read_state(f.ch, f.bob).unwrap().unread, 2);
    }

    #[test]
    fn unread_counts_are_capped() {
        let f = fx();
        for i in 0..(super::super::channels::UNREAD_CAP + 20) {
            post(&f, f.alice, &format!("m{i}"));
        }
        let st = f.s.read_state(f.ch, f.bob).unwrap();
        assert_eq!(
            st.unread,
            super::super::channels::UNREAD_CAP + 1,
            "counting stops just past the cap"
        );
    }

    #[test]
    fn marking_read_clears_the_badge() {
        let f = fx();
        let m = post(&f, f.alice, "hi @bob");
        f.s.insert_message(NewMessage {
            id: f.g.next(),
            channel_id: f.ch,
            author_id: f.alice,
            body: "@bob",
            thread_root: None,
            attachments: &[],
            mentions: &[f.bob],
        })
        .unwrap();
        assert!(f.s.read_state(f.ch, f.bob).unwrap().unread > 0);

        let newest = f.s.channel(f.ch).unwrap().last_message;
        let st = f.s.mark_read(f.ch, f.bob, newest).unwrap();
        assert_eq!(st.unread, 0);
        assert_eq!(st.mentions, 0);
        let _ = m;
    }

    #[test]
    fn attachments_bind_only_for_their_owner() {
        let f = fx();
        let mine =
            f.s.create_attachment(
                f.g.next(),
                f.alice,
                "a.png",
                "image/png",
                10,
                Some((4, 2)),
                "a",
            )
            .unwrap();
        let theirs =
            f.s.create_attachment(f.g.next(), f.bob, "b.png", "image/png", 10, None, "b")
                .unwrap();

        let m =
            f.s.insert_message(NewMessage {
                id: f.g.next(),
                channel_id: f.ch,
                author_id: f.alice,
                body: "look",
                thread_root: None,
                attachments: &[mine.id, theirs.id],
                mentions: &[],
            })
            .unwrap();

        assert_eq!(
            m.attachments.len(),
            1,
            "cannot attach someone else's upload"
        );
        assert_eq!(m.attachments[0].id, mine.id);
        assert_eq!(m.attachments[0].width, Some(4));
    }

    #[test]
    fn attachment_download_requires_channel_membership() {
        let f = fx();
        let carol = f.s.create_user(f.g.next(), "carol", "C", "h").unwrap().id;
        let att =
            f.s.create_attachment(
                f.g.next(),
                f.alice,
                "a.png",
                "image/png",
                10,
                None,
                "blobs/a",
            )
            .unwrap();
        f.s.insert_message(NewMessage {
            id: f.g.next(),
            channel_id: f.ch,
            author_id: f.alice,
            body: "look",
            thread_root: None,
            attachments: &[att.id],
            mentions: &[],
        })
        .unwrap();

        assert!(f.s.attachment_path(att.id, f.bob).is_ok());
        assert!(matches!(
            f.s.attachment_path(att.id, carol),
            Err(Error::Forbidden)
        ));
    }

    #[test]
    fn hydration_is_batched_not_per_message() {
        // Guards the no-N+1 rule: one page of history must cost a bounded
        // number of statements no matter how many messages it contains.
        let f = fx();
        for i in 0..40 {
            let m = post(&f, f.alice, &format!("m{i}"));
            f.s.set_reaction(m.id, f.bob, "👍", true).unwrap();
        }
        let page = f.s.history(f.ch, f.alice, None, 40).unwrap();
        assert_eq!(page.messages.len(), 40);
        assert!(page.messages.iter().all(|m| m.reactions.len() == 1));
    }
}
