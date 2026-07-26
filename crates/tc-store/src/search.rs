//! Full-text search over message history.
//!
//! Backed by FTS5 with an external-content index (see `schema.sql`), so search
//! adds an inverted index but not a second copy of every message body.
//!
//! Authorization is part of the query, not a filter applied afterwards: the
//! join against `members` means a user can only ever match messages in
//! channels they belong to, and dropping that join could not silently widen
//! the result set.

use rusqlite::{Row, params};
use tc_core::{Id, Message, SearchHit, model};

use crate::{Result, Store, from_sql, to_sql, unpack_ids};

pub const MAX_RESULTS: u32 = 50;

/// A parsed search request.
pub struct SearchQuery<'a> {
    /// Raw user input. Sanitized into an FTS5 expression by [`to_fts_query`].
    pub text: &'a str,
    /// Restrict to one channel.
    pub channel: Option<Id>,
    /// Restrict to one author.
    pub author: Option<Id>,
    pub limit: u32,
}

/// Turn user input into a safe FTS5 MATCH expression.
///
/// FTS5's query language has its own syntax (`NEAR`, `*`, `-`, column filters,
/// quoting). Passing raw input through would let a stray quote or operator turn
/// a search into a syntax error — or into a much more expensive query than the
/// user intended. Instead every run of word characters becomes a quoted term,
/// and terms are implicitly ANDed.
///
/// Returns `None` when the input has no searchable content.
fn to_fts_query(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len() + 8);
    let mut terms = 0;

    for raw in text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '\'') {
        let term = raw.trim_matches('\'');
        if term.is_empty() {
            continue;
        }
        if terms > 0 {
            out.push(' ');
        }
        // Double any embedded quote, the FTS5 escape, then wrap. A quoted
        // string is a literal phrase — no operators can escape it.
        out.push('"');
        for ch in term.chars() {
            if ch == '"' {
                out.push('"');
            }
            out.push(ch);
        }
        out.push('"');
        terms += 1;
        // Bound the term count so a pathological query cannot fan out.
        if terms >= 16 {
            break;
        }
    }

    (terms > 0).then_some(out)
}

fn map_hit(row: &Row<'_>) -> rusqlite::Result<SearchHit> {
    let mentions: Option<Vec<u8>> = row.get(8)?;
    Ok(SearchHit {
        message: Message {
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
        },
        snippet: row.get(9)?,
    })
}

impl Store {
    /// Search messages visible to `viewer`, best match first.
    ///
    /// Returns an empty result (not an error) for a query with no searchable
    /// terms, which is what an in-progress keystroke looks like.
    pub fn search(&self, viewer: Id, q: SearchQuery<'_>) -> Result<Vec<SearchHit>> {
        let Some(expr) = to_fts_query(q.text) else {
            return Ok(Vec::new());
        };
        let limit = q.limit.clamp(1, MAX_RESULTS);
        let conn = self.conn()?;

        // The snippet markers are the sentinels from tc_core::model, not HTML:
        // the client escapes the text first and only then converts sentinels to
        // markup, so a message body can never inject tags into its own snippet.
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT m.id, m.channel_id, m.author_id, m.body, m.thread_root, m.reply_count, \
                    m.edited_at, m.deleted, m.mentions, \
                    snippet(messages_fts, 0, char({hl_start}), char({hl_end}), '…', 14) \
               FROM messages_fts f \
               JOIN messages m ON m.id = f.rowid \
               JOIN members mem ON mem.channel_id = m.channel_id AND mem.user_id = ?1 \
              WHERE messages_fts MATCH ?2 \
                AND m.deleted = 0 \
                AND (?3 IS NULL OR m.channel_id = ?3) \
                AND (?4 IS NULL OR m.author_id = ?4) \
              ORDER BY f.rank \
              LIMIT ?5",
            hl_start = model::HL_START as u32,
            hl_end = model::HL_END as u32,
        ))?;

        let mut hits: Vec<SearchHit> = stmt
            .query_map(
                params![
                    to_sql(viewer),
                    expr,
                    q.channel.map(to_sql),
                    q.author.map(to_sql),
                    limit as i64
                ],
                map_hit,
            )?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        drop(conn);

        // Search results render as real messages, so they get the same
        // reaction/attachment hydration — still batched, one pass.
        let mut messages: Vec<Message> = hits.iter().map(|h| h.message.clone()).collect();
        self.hydrate(&mut messages, viewer)?;
        for (hit, hydrated) in hits.iter_mut().zip(messages) {
            hit.message = hydrated;
        }
        Ok(hits)
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
        ch: Id,
        other: Id,
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
        // A channel alice is NOT in.
        let other = s
            .create_channel(NewChannel {
                id: g.next(),
                kind: ChannelKind::Private,
                name: "secret",
                topic: "",
                created_by: bob,
                created_at: 1,
                members: vec![],
            })
            .unwrap()
            .id;
        Fx {
            s,
            g,
            alice,
            bob,
            ch,
            other,
        }
    }

    fn post(f: &Fx, ch: Id, author: Id, body: &str) -> Id {
        f.s.insert_message(NewMessage {
            id: f.g.next(),
            channel_id: ch,
            author_id: author,
            body,
            thread_root: None,
            attachments: &[],
            mentions: &[],
        })
        .unwrap()
        .id
    }

    fn ids(hits: &[SearchHit]) -> Vec<Id> {
        hits.iter().map(|h| h.message.id).collect()
    }

    #[test]
    fn finds_messages_and_highlights_the_match() {
        let f = fx();
        let m = post(&f, f.ch, f.alice, "the deploy pipeline is green");
        let hits =
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "deploy",
                    channel: None,
                    author: None,
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(ids(&hits), vec![m]);
        assert!(
            hits[0].snippet.contains(model::HL_START),
            "expected a highlight sentinel"
        );
        assert!(
            !hits[0].snippet.contains('<'),
            "snippets must not contain markup"
        );
    }

    #[test]
    fn never_returns_messages_from_channels_you_are_not_in() {
        let f = fx();
        post(&f, f.other, f.bob, "the secret deploy key");
        let hits =
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "secret deploy",
                    channel: None,
                    author: None,
                    limit: 10,
                },
            )
            .unwrap();
        assert!(hits.is_empty(), "search must not leak private channels");
        // Bob is a member, so he can find it.
        assert_eq!(
            f.s.search(
                f.bob,
                SearchQuery {
                    text: "secret deploy",
                    channel: None,
                    author: None,
                    limit: 10,
                }
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn terms_are_anded_together() {
        let f = fx();
        let both = post(&f, f.ch, f.alice, "green deploy pipeline");
        post(&f, f.ch, f.alice, "green tea");
        let hits =
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "green deploy",
                    channel: None,
                    author: None,
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(ids(&hits), vec![both]);
    }

    #[test]
    fn deleted_messages_leave_the_index() {
        let f = fx();
        let m = post(&f, f.ch, f.alice, "ephemeral secret");
        f.s.delete_message(m, f.alice, false).unwrap();
        assert!(
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "ephemeral",
                    channel: None,
                    author: None,
                    limit: 10,
                }
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn edits_update_the_index() {
        let f = fx();
        let m = post(&f, f.ch, f.alice, "orginal typo");
        f.s.edit_message(m, f.alice, "original text", 5).unwrap();
        assert!(
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "orginal",
                    channel: None,
                    author: None,
                    limit: 10,
                }
            )
            .unwrap()
            .is_empty(),
            "stale term must be gone"
        );
        assert_eq!(
            ids(&f
                .s
                .search(
                    f.alice,
                    SearchQuery {
                        text: "original",
                        channel: None,
                        author: None,
                        limit: 10,
                    }
                )
                .unwrap()),
            vec![m]
        );
    }

    #[test]
    fn filters_by_channel_and_author() {
        let f = fx();
        let by_alice = post(&f, f.ch, f.alice, "shared word");
        let by_bob = post(&f, f.ch, f.bob, "shared word");
        let all =
            f.s.search(
                f.bob,
                SearchQuery {
                    text: "shared",
                    channel: None,
                    author: None,
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(all.len(), 2);

        let only_bob =
            f.s.search(
                f.bob,
                SearchQuery {
                    text: "shared",
                    channel: None,
                    author: Some(f.bob),
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(ids(&only_bob), vec![by_bob]);

        let in_ch =
            f.s.search(
                f.bob,
                SearchQuery {
                    text: "shared",
                    channel: Some(f.ch),
                    author: Some(f.alice),
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(ids(&in_ch), vec![by_alice]);
    }

    #[test]
    fn fts_operators_in_user_input_are_treated_as_literal_text() {
        let f = fx();
        // Each of these would be a syntax error or an operator if passed
        // through raw; all must simply find nothing (or match literally).
        for probe in [
            "\"unterminated",
            "foo AND (bar",
            "NEAR(a b)",
            "a OR b*",
            "-excluded",
            "col:value",
            "\"\"\"",
        ] {
            let r = f.s.search(
                f.alice,
                SearchQuery {
                    text: probe,
                    channel: None,
                    author: None,
                    limit: 10,
                },
            );
            assert!(r.is_ok(), "query {probe:?} should not error, got {r:?}");
        }
    }

    #[test]
    fn a_literal_quote_in_a_message_is_findable() {
        let f = fx();
        let m = post(&f, f.ch, f.alice, r#"he said "shipit" loudly"#);
        let hits =
            f.s.search(
                f.alice,
                SearchQuery {
                    text: r#""shipit""#,
                    channel: None,
                    author: None,
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(ids(&hits), vec![m]);
    }

    #[test]
    fn empty_or_punctuation_only_queries_return_nothing_quietly() {
        let f = fx();
        post(&f, f.ch, f.alice, "hello");
        for probe in ["", "   ", "!!!", "-- ??"] {
            assert!(
                f.s.search(
                    f.alice,
                    SearchQuery {
                        text: probe,
                        channel: None,
                        author: None,
                        limit: 10,
                    }
                )
                .unwrap()
                .is_empty()
            );
        }
    }

    #[test]
    fn search_is_diacritic_and_case_insensitive() {
        let f = fx();
        let m = post(&f, f.ch, f.alice, "Café Réunion");
        for probe in ["cafe", "CAFÉ", "réunion", "reunion"] {
            assert_eq!(
                ids(&f
                    .s
                    .search(
                        f.alice,
                        SearchQuery {
                            text: probe,
                            channel: None,
                            author: None,
                            limit: 10,
                        }
                    )
                    .unwrap()),
                vec![m],
                "probe {probe:?}"
            );
        }
    }

    #[test]
    fn results_carry_reactions_like_any_other_message() {
        let f = fx();
        let m = post(&f, f.ch, f.alice, "reactable content");
        f.s.set_reaction(m, f.bob, "👍", true).unwrap();
        let hits =
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "reactable",
                    channel: None,
                    author: None,
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(hits[0].message.reactions.len(), 1);
        assert_eq!(hits[0].message.reactions[0].count, 1);
    }

    #[test]
    fn query_sanitizer_produces_quoted_anded_terms() {
        assert_eq!(
            to_fts_query("green deploy").as_deref(),
            Some(r#""green" "deploy""#)
        );
        assert_eq!(to_fts_query("a-b").as_deref(), Some(r#""a" "b""#));
        assert_eq!(
            to_fts_query(r#"say "hi""#).as_deref(),
            Some(r#""say" "hi""#)
        );
        assert_eq!(to_fts_query("   "), None);
        // Bounded term count.
        let many = (0..40).map(|i| format!("w{i} ")).collect::<String>();
        assert_eq!(to_fts_query(&many).unwrap().matches('"').count(), 32);
    }
}
