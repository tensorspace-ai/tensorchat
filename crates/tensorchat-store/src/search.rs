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
use tensorchat_core::{Id, Message, SearchHit, model};

use crate::{Result, Store, from_sql, to_sql, unpack_ids};

pub const MAX_RESULTS: u32 = 50;

/// A parsed search request.
///
/// The `from:` / `in:` / `before:` / `after:` / `has:` operators a user types
/// are parsed by `tensorchat_core::query` and resolved to ids by the server; by the
/// time a request reaches here every filter is already a concrete value.
#[derive(Default)]
pub struct SearchQuery<'a> {
    /// Free text, operators already removed. Sanitized into an FTS5 expression
    /// by [`to_fts_query`]. May be empty when the request is filters-only.
    pub text: &'a str,
    /// Restrict to one channel.
    pub channel: Option<Id>,
    /// Restrict to one author.
    pub author: Option<Id>,
    /// Exclusive upper bound on message id, from `before:`.
    pub before: Option<Id>,
    /// Inclusive lower bound on message id, from `after:`.
    pub after: Option<Id>,
    /// Only messages whose body contains a URL.
    pub has_link: bool,
    /// Only messages carrying an attachment.
    pub has_file: bool,
    /// Only messages carrying an image attachment.
    pub has_image: bool,
    pub limit: u32,
}

impl SearchQuery<'_> {
    /// Whether anything besides the free text narrows this search.
    fn has_filters(&self) -> bool {
        self.channel.is_some()
            || self.author.is_some()
            || self.before.is_some()
            || self.after.is_some()
            || self.has_link
            || self.has_file
            || self.has_image
    }
}

/// The filter predicates shared by the text and filters-only queries, so the
/// two can never disagree about what a `has:` means. Parameters 3..=8.
const FILTERS: &str = "AND (?3 IS NULL OR m.channel_id = ?3) \
     AND (?4 IS NULL OR m.author_id = ?4) \
     AND (?5 IS NULL OR m.id < ?5) \
     AND (?6 IS NULL OR m.id >= ?6) \
     AND (?7 = 0 OR (m.body LIKE '%http://%' OR m.body LIKE '%https://%')) \
     AND (?8 = 0 OR EXISTS (SELECT 1 FROM attachments a WHERE a.message_id = m.id)) \
     AND (?9 = 0 OR EXISTS (SELECT 1 FROM attachments a \
                             WHERE a.message_id = m.id AND a.mime LIKE 'image/%'))";

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
    /// Returns an empty result (not an error) for a query with neither search
    /// terms nor filters, which is what an in-progress keystroke looks like.
    ///
    /// A query with filters but no terms — `from:alice in:general` — is a real
    /// request for "everything Alice said there" and is answered newest-first
    /// from the message table, without touching the full-text index at all.
    pub fn search(&self, viewer: Id, q: SearchQuery<'_>) -> Result<Vec<SearchHit>> {
        let expr = to_fts_query(q.text);
        if expr.is_none() && !q.has_filters() {
            return Ok(Vec::new());
        }
        let limit = q.limit.clamp(1, MAX_RESULTS);
        let conn = self.conn()?;

        // The snippet markers are the sentinels from tensorchat_core::model, not HTML:
        // the client escapes the text first and only then converts sentinels to
        // markup, so a message body can never inject tags into its own snippet.
        //
        // Both shapes carry the same membership join and the same filters; they
        // differ only in whether there is an index to match and rank against.
        let sql = match &expr {
            Some(_) => format!(
                "SELECT m.id, m.channel_id, m.author_id, m.body, m.thread_root, m.reply_count, \
                        m.edited_at, m.deleted, m.mentions, \
                        snippet(messages_fts, 0, char({hl_start}), char({hl_end}), '…', 14) \
                   FROM messages_fts f \
                   JOIN messages m ON m.id = f.rowid \
                   JOIN members mem ON mem.channel_id = m.channel_id AND mem.user_id = ?1 \
                  WHERE messages_fts MATCH ?2 \
                    AND m.deleted = 0 {FILTERS} \
                  ORDER BY f.rank \
                  LIMIT ?10",
                hl_start = model::HL_START as u32,
                hl_end = model::HL_END as u32,
            ),
            // No text to rank by, so newest first — and no snippet, since there
            // is no match to highlight. `?2` is bound and unused so that both
            // statements take the same parameter list.
            None => format!(
                "SELECT m.id, m.channel_id, m.author_id, m.body, m.thread_root, m.reply_count, \
                        m.edited_at, m.deleted, m.mentions, '' \
                   FROM messages m \
                   JOIN members mem ON mem.channel_id = m.channel_id AND mem.user_id = ?1 \
                  WHERE ?2 IS NULL \
                    AND m.deleted = 0 {FILTERS} \
                  ORDER BY m.id DESC \
                  LIMIT ?10"
            ),
        };
        let mut stmt = conn.prepare_cached(&sql)?;

        let mut hits: Vec<SearchHit> = stmt
            .query_map(
                params![
                    to_sql(viewer),
                    expr,
                    q.channel.map(to_sql),
                    q.author.map(to_sql),
                    q.before.map(to_sql),
                    q.after.map(to_sql),
                    q.has_link,
                    q.has_file,
                    q.has_image,
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
    use tensorchat_core::{ChannelKind, IdGen};

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

    /// Post a message carrying one attachment of the given type.
    fn post_with_attachment(f: &Fx, body: &str, name: &str, mime: &str) -> Id {
        let a =
            f.s.create_attachment(f.g.next(), f.alice, name, mime, 10, None, "blob/path")
                .unwrap();
        f.s.insert_message(NewMessage {
            id: f.g.next(),
            channel_id: f.ch,
            author_id: f.alice,
            body,
            thread_root: None,
            attachments: &[a.id],
            mentions: &[],
        })
        .unwrap()
        .id
    }

    fn ids(hits: &[SearchHit]) -> Vec<Id> {
        hits.iter().map(|h| h.message.id).collect()
    }

    /// A search request with the given text and the two long-standing filters.
    /// Everything added since defaults off, so a test names only what it means.
    fn q<'a>(text: &'a str, channel: Option<Id>, author: Option<Id>) -> SearchQuery<'a> {
        SearchQuery {
            text,
            channel,
            author,
            limit: 10,
            ..Default::default()
        }
    }

    #[test]
    fn finds_messages_and_highlights_the_match() {
        let f = fx();
        let m = post(&f, f.ch, f.alice, "the deploy pipeline is green");
        let hits = f.s.search(f.alice, q("deploy", None, None)).unwrap();
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
        let hits = f.s.search(f.alice, q("secret deploy", None, None)).unwrap();
        assert!(hits.is_empty(), "search must not leak private channels");
        // Bob is a member, so he can find it.
        assert_eq!(
            f.s.search(f.bob, q("secret deploy", None, None))
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
        let hits = f.s.search(f.alice, q("green deploy", None, None)).unwrap();
        assert_eq!(ids(&hits), vec![both]);
    }

    #[test]
    fn deleted_messages_leave_the_index() {
        let f = fx();
        let m = post(&f, f.ch, f.alice, "ephemeral secret");
        f.s.delete_message(m, f.alice, false).unwrap();
        assert!(
            f.s.search(f.alice, q("ephemeral", None, None))
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
            f.s.search(f.alice, q("orginal", None, None))
                .unwrap()
                .is_empty(),
            "stale term must be gone"
        );
        assert_eq!(
            ids(&f.s.search(f.alice, q("original", None, None)).unwrap()),
            vec![m]
        );
    }

    #[test]
    fn filters_by_channel_and_author() {
        let f = fx();
        let by_alice = post(&f, f.ch, f.alice, "shared word");
        let by_bob = post(&f, f.ch, f.bob, "shared word");
        let all = f.s.search(f.bob, q("shared", None, None)).unwrap();
        assert_eq!(all.len(), 2);

        let only_bob = f.s.search(f.bob, q("shared", None, Some(f.bob))).unwrap();
        assert_eq!(ids(&only_bob), vec![by_bob]);

        let in_ch =
            f.s.search(f.bob, q("shared", Some(f.ch), Some(f.alice)))
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
            let r = f.s.search(f.alice, q(probe, None, None));
            assert!(r.is_ok(), "query {probe:?} should not error, got {r:?}");
        }
    }

    #[test]
    fn a_literal_quote_in_a_message_is_findable() {
        let f = fx();
        let m = post(&f, f.ch, f.alice, r#"he said "shipit" loudly"#);
        let hits = f.s.search(f.alice, q(r#""shipit""#, None, None)).unwrap();
        assert_eq!(ids(&hits), vec![m]);
    }

    #[test]
    fn empty_or_punctuation_only_queries_return_nothing_quietly() {
        let f = fx();
        post(&f, f.ch, f.alice, "hello");
        for probe in ["", "   ", "!!!", "-- ??"] {
            assert!(
                f.s.search(f.alice, q(probe, None, None))
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
                ids(&f.s.search(f.alice, q(probe, None, None)).unwrap()),
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
        let hits = f.s.search(f.alice, q("reactable", None, None)).unwrap();
        assert_eq!(hits[0].message.reactions.len(), 1);
        assert_eq!(hits[0].message.reactions[0].count, 1);
    }

    // -- Operators (parsed by tensorchat_core::query, resolved before they get here) --

    #[test]
    fn a_date_bound_excludes_messages_outside_it() {
        let f = fx();
        let old = post(&f, f.ch, f.alice, "shared word");
        let new = post(&f, f.ch, f.alice, "shared word");
        // Ids are time-sortable, so a date bound is an id bound. Use the older
        // message's own id as the cutoff.
        let cut = new;

        let before =
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "shared",
                    before: Some(cut),
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(ids(&before), vec![old], "before is exclusive of the cutoff");

        let after =
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "shared",
                    after: Some(cut),
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(ids(&after), vec![new], "after is inclusive of the cutoff");
    }

    #[test]
    fn has_link_matches_only_bodies_carrying_a_url() {
        let f = fx();
        let with = post(&f, f.ch, f.alice, "the runbook is at https://example.com/x");
        post(&f, f.ch, f.alice, "the runbook is in the wiki");
        // A word that merely contains "http" must not count.
        post(&f, f.ch, f.alice, "the runbook mentions httpd config");

        let hits =
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "runbook",
                    has_link: true,
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(ids(&hits), vec![with]);
    }

    #[test]
    fn has_file_and_has_image_distinguish_attachment_kinds() {
        let f = fx();
        let doc = post_with_attachment(&f, "quarterly summary", "notes.pdf", "application/pdf");
        let pic = post_with_attachment(&f, "quarterly chart", "chart.png", "image/png");
        post(&f, f.ch, f.alice, "quarterly notes, no attachment");

        let files =
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "quarterly",
                    has_file: true,
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut got = ids(&files);
        got.sort_unstable();
        let mut want = vec![doc, pic];
        want.sort_unstable();
        assert_eq!(got, want, "has:file covers every attachment");

        let images =
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "quarterly",
                    has_image: true,
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(ids(&images), vec![pic], "has:image is narrower");
    }

    #[test]
    fn filters_without_any_search_terms_still_return_results() {
        // "everything alice said" is a real request, not an empty query.
        let f = fx();
        post(&f, f.ch, f.bob, "from bob");
        let a1 = post(&f, f.ch, f.alice, "from alice one");
        let a2 = post(&f, f.ch, f.alice, "from alice two");

        let hits =
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "",
                    author: Some(f.alice),
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(ids(&hits), vec![a2, a1], "newest first, with no ranking");
        assert_eq!(
            hits[0].snippet, "",
            "nothing matched, so nothing to highlight"
        );
    }

    #[test]
    fn a_filters_only_query_is_still_scoped_by_membership() {
        // The filters-only path skips the FTS index entirely, so it needs its
        // own proof that it did not also skip the authorization join.
        let f = fx();
        post(&f, f.other, f.bob, "in the private channel");
        let hits =
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "",
                    author: Some(f.bob),
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(hits.is_empty(), "membership must gate this path too");
        assert_eq!(
            f.s.search(
                f.bob,
                SearchQuery {
                    text: "",
                    author: Some(f.bob),
                    limit: 10,
                    ..Default::default()
                }
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn a_filters_only_query_still_omits_deleted_messages() {
        let f = fx();
        let m = post(&f, f.ch, f.alice, "regrettable");
        f.s.delete_message(m, f.alice, false).unwrap();
        assert!(
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "",
                    author: Some(f.alice),
                    limit: 10,
                    ..Default::default()
                }
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn an_empty_query_with_no_filters_at_all_stays_empty() {
        // Otherwise an in-progress keystroke would dump the whole workspace.
        let f = fx();
        post(&f, f.ch, f.alice, "anything");
        assert!(
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "",
                    limit: 10,
                    ..Default::default()
                }
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn filters_compose_with_each_other_and_with_text() {
        let f = fx();
        post(&f, f.ch, f.bob, "release https://example.com/a");
        let want = post(&f, f.ch, f.alice, "release https://example.com/b");
        post(&f, f.ch, f.alice, "release with no link");

        let hits =
            f.s.search(
                f.alice,
                SearchQuery {
                    text: "release",
                    author: Some(f.alice),
                    channel: Some(f.ch),
                    has_link: true,
                    limit: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(ids(&hits), vec![want]);
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
