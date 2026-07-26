//! Text rules shared by the server and the store: mention scanning, handle
//! normalization, and the validation limits that bound every user-supplied
//! string before it reaches the database.

use std::borrow::Cow;

/// Hard caps. These are enforced at the edge so no downstream layer has to
/// defend against unbounded input.
pub const MAX_BODY_BYTES: usize = 16 * 1024;
pub const MAX_HANDLE_LEN: usize = 32;
pub const MAX_DISPLAY_NAME_LEN: usize = 64;
pub const MAX_CHANNEL_NAME_LEN: usize = 80;
pub const MAX_TOPIC_LEN: usize = 250;
pub const MAX_STATUS_LEN: usize = 100;
pub const MAX_EMOJI_LEN: usize = 64;
/// Mentions beyond this many per message are ignored, so one message cannot
/// fan out an unbounded number of badge updates.
pub const MAX_MENTIONS_PER_MESSAGE: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mention {
    /// `@alice` — the handle, already normalized.
    User(String),
    /// `@channel` — everyone in the channel.
    Channel,
    /// `@here` — everyone currently online in the channel.
    Here,
}

/// Extract mentions from a message body, in order, deduplicated.
///
/// Runs once at write time; the resulting user IDs are stored on the message
/// so unread/mention counting never re-parses text at read time.
pub fn scan_mentions(body: &str) -> Vec<Mention> {
    let bytes = body.as_bytes();
    let mut out: Vec<Mention> = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'@' {
            i += 1;
            continue;
        }
        // A mention must start at a word boundary, so `email@example.com` and
        // `foo@bar` do not produce one.
        if i > 0 && is_handle_byte(bytes[i - 1]) {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() && is_handle_byte(bytes[end]) {
            end += 1;
        }
        if end == start {
            i += 1;
            continue;
        }
        // Trailing punctuation is part of the sentence, not the handle:
        // "ping @alice." mentions `alice`.
        let mut trimmed = end;
        while trimmed > start && matches!(bytes[trimmed - 1], b'.' | b'-' | b'_') {
            trimmed -= 1;
        }
        if trimmed > start {
            let raw = &body[start..trimmed];
            let m = match raw.to_ascii_lowercase().as_str() {
                "channel" | "everyone" => Mention::Channel,
                "here" => Mention::Here,
                h => Mention::User(h.to_string()),
            };
            if !out.contains(&m) && out.len() < MAX_MENTIONS_PER_MESSAGE {
                out.push(m);
            }
        }
        i = end;
    }
    out
}

#[inline]
fn is_handle_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-'
}

/// Canonical form of a handle: lowercase ASCII.
///
/// Borrows when the input is already canonical, which is the common path for
/// login lookups.
pub fn normalize_handle(raw: &str) -> Cow<'_, str> {
    let t = raw.trim().trim_start_matches('@');
    if t.bytes().all(|b| !b.is_ascii_uppercase()) && t.len() == raw.len() {
        Cow::Borrowed(t)
    } else {
        Cow::Owned(t.to_ascii_lowercase())
    }
}

/// A handle must be usable in `@mention` syntax without ambiguity, which is
/// exactly the character class [`scan_mentions`] recognizes.
pub fn validate_handle(h: &str) -> Result<(), &'static str> {
    if h.is_empty() || h.len() > MAX_HANDLE_LEN {
        return Err("handle must be 1-32 characters");
    }
    if !h
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'))
    {
        return Err("handle may contain only a-z, 0-9, '.', '_' and '-'");
    }
    if h.starts_with(['.', '-', '_']) || h.ends_with(['.', '-', '_']) {
        return Err("handle must start and end with a letter or digit");
    }
    // Reserved so `@here` can never be shadowed by a real account.
    if matches!(h, "here" | "channel" | "everyone") {
        return Err("that handle is reserved");
    }
    Ok(())
}

/// Channel names follow Slack's convention: lowercase, dash-separated.
pub fn validate_channel_name(n: &str) -> Result<(), &'static str> {
    if n.is_empty() || n.len() > MAX_CHANNEL_NAME_LEN {
        return Err("channel name must be 1-80 characters");
    }
    if !n
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
    {
        return Err("channel name may contain only a-z, 0-9, '-' and '_'");
    }
    Ok(())
}

/// Trim and length-check a message body.
///
/// Returns `None` for a body that is empty once trimmed — those are dropped
/// rather than rejected loudly, since they are usually a stray Enter.
pub fn clean_body(raw: &str) -> Result<Option<&str>, &'static str> {
    if raw.len() > MAX_BODY_BYTES {
        return Err("message is too long");
    }
    let t = raw.trim();
    Ok(if t.is_empty() { None } else { Some(t) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_mentions_at_word_boundaries_only() {
        assert_eq!(
            scan_mentions("hey @alice and @bob"),
            vec![Mention::User("alice".into()), Mention::User("bob".into())]
        );
        // Email addresses are not mentions.
        assert_eq!(scan_mentions("mail me at jj@example.com"), vec![]);
        // Leading punctuation still starts a boundary.
        assert_eq!(
            scan_mentions("(@alice)"),
            vec![Mention::User("alice".into())]
        );
    }

    #[test]
    fn strips_trailing_punctuation_and_dedupes() {
        assert_eq!(
            scan_mentions("@alice. @alice, @alice"),
            vec![Mention::User("alice".into())]
        );
        assert_eq!(scan_mentions("@bob-"), vec![Mention::User("bob".into())]);
    }

    #[test]
    fn recognizes_broadcast_mentions_case_insensitively() {
        assert_eq!(
            scan_mentions("@here @Channel @everyone"),
            vec![Mention::Here, Mention::Channel]
        );
    }

    #[test]
    fn mention_scan_is_bounded() {
        let body = (0..500).map(|i| format!("@u{i} ")).collect::<String>();
        assert_eq!(scan_mentions(&body).len(), MAX_MENTIONS_PER_MESSAGE);
    }

    #[test]
    fn handles_unicode_bodies_without_panicking() {
        // Byte indexing into `body` must land on char boundaries; a mention
        // adjacent to multi-byte text is the case that would panic if not.
        assert_eq!(
            scan_mentions("こんにちは @alice 🎉 @bob"),
            vec![Mention::User("alice".into()), Mention::User("bob".into())]
        );
        assert_eq!(
            scan_mentions("🎉@alice"),
            vec![Mention::User("alice".into())]
        );
    }

    #[test]
    fn validates_handles() {
        assert!(validate_handle("alice").is_ok());
        assert!(validate_handle("a.b-c_1").is_ok());
        assert!(
            validate_handle("Alice").is_err(),
            "uppercase must be normalized first"
        );
        assert!(validate_handle("here").is_err(), "reserved");
        assert!(validate_handle("-nope").is_err());
        assert!(validate_handle("").is_err());
    }

    #[test]
    fn normalize_handle_borrows_when_already_canonical() {
        assert!(matches!(normalize_handle("alice"), Cow::Borrowed("alice")));
        assert_eq!(
            normalize_handle(" @Alice "),
            Cow::Owned::<str>("alice".into())
        );
    }

    #[test]
    fn clean_body_rejects_oversize_and_drops_blank() {
        assert_eq!(clean_body("  hi  ").unwrap(), Some("hi"));
        assert_eq!(clean_body("   \n ").unwrap(), None);
        assert!(clean_body(&"x".repeat(MAX_BODY_BYTES + 1)).is_err());
    }
}
