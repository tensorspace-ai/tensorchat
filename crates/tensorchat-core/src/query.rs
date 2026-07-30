//! Search-query operators: `from:`, `in:`, `before:`, `after:`, `has:`.
//!
//! Pure parsing, no I/O — the names this produces (`from:alice`, `in:general`)
//! are resolved to ids by the server, which is the layer that can look them up.
//!
//! # Design
//!
//! Two rules keep this predictable:
//!
//! * **An unrecognized `key:value` is free text.** `note:` in a message body,
//!   or a bare URL, must still be findable, and a typo like `form:alice` should
//!   search for "form alice" rather than silently returning everything. Only the
//!   known keys with a well-formed value are consumed.
//! * **The leftover text is returned verbatim.** It goes on to the FTS5
//!   sanitizer exactly as before, so operators cannot change how the remaining
//!   terms are escaped.
//!
//! `before:` and `after:` are both **exclusive of the named day**, which is the
//! convention users arrive with from Slack: `after:2026-01-15` means "the 16th
//! onwards".

/// Content filters from `has:`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Has {
    pub link: bool,
    pub file: bool,
    pub image: bool,
}

impl Has {
    pub fn any(self) -> bool {
        self.link || self.file || self.image
    }
}

/// A search string split into free text and filters.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ParsedQuery {
    /// What is left once operators are removed. Feeds the FTS5 sanitizer.
    pub text: String,
    /// Handle from `from:`, without a leading `@`, lowercased.
    pub from: Option<String>,
    /// Channel name from `in:`, without a leading `#`, lowercased.
    pub in_channel: Option<String>,
    /// Exclusive upper bound, Unix ms: midnight UTC starting the named day.
    pub before: Option<u64>,
    /// Inclusive lower bound, Unix ms: midnight UTC *after* the named day.
    pub after: Option<u64>,
    pub has: Has,
}

impl ParsedQuery {
    /// Whether anything narrows the search besides the free text.
    ///
    /// The server uses this to decide that `from:alice` on its own — no search
    /// terms at all — is a real query for "everything Alice said" rather than an
    /// empty one.
    pub fn has_filters(&self) -> bool {
        self.from.is_some()
            || self.in_channel.is_some()
            || self.before.is_some()
            || self.after.is_some()
            || self.has.any()
    }
}

const MS_PER_DAY: u64 = 86_400_000;

/// Parse `YYYY-MM-DD` into the Unix ms of that day's midnight UTC.
///
/// Deliberately strict: a partial or malformed date makes the whole token fall
/// back to free text, so `before:soon` searches for "before soon" instead of
/// quietly matching everything.
fn parse_date(s: &str) -> Option<u64> {
    let bytes = s.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: u32 = s[5..7].parse().ok()?;
    let day: u32 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    // Anything before 1970 cannot name a message; clamping to the epoch keeps
    // the return type unsigned without a special case downstream.
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return Some(0);
    }
    Some(days as u64 * MS_PER_DAY)
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 for a proleptic-Gregorian date.
///
/// Howard Hinnant's `days_from_civil`: shift the year to start in March so that
/// the leap day lands at the end of the cycle, which removes every special case
/// from the month-length arithmetic. Integer-only, so no floating point and no
/// dependency on a date crate for what is ultimately one formula.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = month as i64;
    let d = day as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Split a raw search string into free text and filters.
pub fn parse(raw: &str) -> ParsedQuery {
    let mut q = ParsedQuery::default();
    let mut text = String::with_capacity(raw.len());

    for token in raw.split_whitespace() {
        if let Some((key, value)) = token.split_once(':')
            && !value.is_empty()
            && consume(&mut q, key, value)
        {
            continue;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(token);
    }

    q.text = text;
    q
}

/// Apply one `key:value` pair. Returns whether it was recognized — a `false`
/// sends the whole token back to the free text.
fn consume(q: &mut ParsedQuery, key: &str, value: &str) -> bool {
    // Keys are matched case-insensitively; `From:alice` at the start of a
    // sentence should still work.
    let key = key.to_ascii_lowercase();
    match key.as_str() {
        "from" => {
            let h = value.trim_start_matches('@').to_ascii_lowercase();
            if h.is_empty() {
                return false;
            }
            q.from = Some(h);
            true
        }
        "in" => {
            let c = value.trim_start_matches('#').to_ascii_lowercase();
            if c.is_empty() {
                return false;
            }
            q.in_channel = Some(c);
            true
        }
        // Exclusive of the named day at both ends; see the module doc.
        "before" => match parse_date(value) {
            Some(ms) => {
                q.before = Some(ms);
                true
            }
            None => false,
        },
        "after" => match parse_date(value) {
            Some(ms) => {
                q.after = Some(ms.saturating_add(MS_PER_DAY));
                true
            }
            None => false,
        },
        "has" => match value.to_ascii_lowercase().as_str() {
            "link" | "url" => {
                q.has.link = true;
                true
            }
            "file" | "attachment" => {
                q.has.file = true;
                true
            }
            "image" | "img" | "photo" => {
                q.has.image = true;
                true
            }
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_with_no_operators_is_unchanged() {
        let q = parse("the deploy pipeline is green");
        assert_eq!(q.text, "the deploy pipeline is green");
        assert!(!q.has_filters());
    }

    #[test]
    fn operators_are_lifted_out_of_the_text() {
        let q = parse("deploy from:@alice in:#general");
        assert_eq!(
            q.text, "deploy",
            "operators must not reach the FTS sanitizer"
        );
        assert_eq!(q.from.as_deref(), Some("alice"));
        assert_eq!(q.in_channel.as_deref(), Some("general"));
        assert!(q.has_filters());
    }

    #[test]
    fn sigils_are_optional_and_keys_are_case_insensitive() {
        let q = parse("From:Alice IN:General");
        assert_eq!(q.from.as_deref(), Some("alice"));
        assert_eq!(q.in_channel.as_deref(), Some("general"));
        assert_eq!(q.text, "");
    }

    #[test]
    fn operators_can_appear_anywhere_in_the_query() {
        let q = parse("in:general deploy from:bob failed");
        assert_eq!(q.text, "deploy failed");
        assert_eq!(q.from.as_deref(), Some("bob"));
        assert_eq!(q.in_channel.as_deref(), Some("general"));
    }

    #[test]
    fn an_unknown_key_stays_free_text() {
        // A typo must search, not silently drop the constraint and return
        // everything — and a message that genuinely says "note:" is findable.
        let q = parse("form:alice note:this http://example.com");
        assert_eq!(q.text, "form:alice note:this http://example.com");
        assert!(!q.has_filters());
    }

    #[test]
    fn a_key_with_no_value_stays_free_text() {
        for probe in ["from:", "in:", "has:", "before:", ":alice"] {
            let q = parse(probe);
            assert_eq!(q.text, probe, "{probe:?}");
            assert!(!q.has_filters(), "{probe:?}");
        }
    }

    #[test]
    fn has_accepts_the_obvious_synonyms() {
        assert!(parse("has:link").has.link);
        assert!(parse("has:url").has.link);
        assert!(parse("has:file").has.file);
        assert!(parse("has:attachment").has.file);
        assert!(parse("has:image").has.image);
        assert!(parse("has:photo").has.image);

        let both = parse("has:link has:image");
        assert!(both.has.link && both.has.image && !both.has.file);

        // Anything else is text, not a silently-ignored filter.
        let unknown = parse("has:pizza");
        assert!(!unknown.has.any());
        assert_eq!(unknown.text, "has:pizza");
    }

    #[test]
    fn dates_become_midnight_utc() {
        // 2026-01-15T00:00:00Z
        assert_eq!(parse("before:2026-01-15").before, Some(1_768_435_200_000));
        // `after:` is exclusive of the named day, so it starts a day later.
        assert_eq!(parse("after:2026-01-15").after, Some(1_768_521_600_000));
        // The epoch itself, as a fixed point for the civil-date arithmetic.
        assert_eq!(parse("before:1970-01-01").before, Some(0));
    }

    #[test]
    fn leap_days_are_handled() {
        assert!(
            parse("before:2024-02-29").before.is_some(),
            "2024 is a leap year"
        );
        assert!(parse("before:2023-02-29").before.is_none(), "2023 is not");
        // Centuries: 2000 is a leap year, 1900 is not.
        assert!(parse("before:2000-02-29").before.is_some());
        assert!(parse("before:1900-02-29").before.is_none());
    }

    #[test]
    fn a_malformed_date_stays_free_text() {
        // Not "match everything since the beginning of time".
        for probe in [
            "before:soon",
            "before:2026",
            "before:2026-1-5",
            "after:2026-13-01",
            "after:2026-01-32",
            "before:0000-00-00",
            "before:20260115",
        ] {
            let q = parse(probe);
            assert_eq!(q.text, probe, "{probe:?}");
            assert_eq!(q.before, None, "{probe:?}");
            assert_eq!(q.after, None, "{probe:?}");
        }
    }

    #[test]
    fn dates_before_the_epoch_clamp_rather_than_underflow() {
        assert_eq!(parse("before:1900-01-01").before, Some(0));
    }

    #[test]
    fn a_query_of_only_operators_has_no_text_but_is_still_a_query() {
        let q = parse("from:alice");
        assert_eq!(q.text, "");
        assert!(
            q.has_filters(),
            "\"everything Alice said\" is a real search, not an empty one"
        );
    }

    #[test]
    fn the_last_of_a_repeated_operator_wins() {
        // Arbitrary, but it has to be one of them, and "the most recently typed"
        // is what someone correcting themselves expects.
        let q = parse("from:alice from:bob");
        assert_eq!(q.from.as_deref(), Some("bob"));
    }

    #[test]
    fn a_url_in_the_query_survives_intact() {
        // `https:` looks exactly like an operator and must not be eaten.
        let q = parse("see https://example.com/x for details");
        assert_eq!(q.text, "see https://example.com/x for details");
        assert!(!q.has_filters());
    }

    #[test]
    fn civil_date_conversion_matches_known_values() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(days_from_civil(2024, 2, 29), 19782);
    }
}
