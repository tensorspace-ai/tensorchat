//! Domain entities.
//!
//! Field names are deliberately short (`ch`, `au`, `b`). These structs are
//! encoded with named MessagePack, so every key is repeated on every frame;
//! two-character keys keep the framing overhead near-negligible next to the
//! message text while still being self-describing (a positional encoding would
//! couple Rust field *order* to the TypeScript decoder — a silent-corruption
//! footgun for the sake of a few bytes).

use crate::id::Id;
use serde::{Deserialize, Serialize};

/// A human (or bot) account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: Id,
    /// Unique, lowercase, no `@`. This is what `@mentions` resolve against.
    #[serde(rename = "h")]
    pub handle: String,
    #[serde(rename = "n")]
    pub display_name: String,
    /// Free-text status ("in a meeting"), empty when unset.
    #[serde(rename = "st", default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(rename = "bot", default, skip_serializing_if = "is_false")]
    pub bot: bool,
    #[serde(rename = "d", default, skip_serializing_if = "is_false")]
    pub deactivated: bool,
    /// Workspace administrator. Not a secret — clients show an admin badge and
    /// reveal admin-only controls — so it travels on the ordinary `User`.
    #[serde(rename = "adm", default, skip_serializing_if = "is_false")]
    pub admin: bool,
}

/// Live connection state. Derived from the hub, never persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Presence {
    Online,
    Away,
    Offline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    /// Discoverable and joinable by anyone in the workspace.
    Public,
    /// Invite-only.
    Private,
    /// Exactly two members, auto-created, cannot be renamed.
    Dm,
    /// Three or more members, no name.
    Group,
}

impl ChannelKind {
    /// DMs and group DMs are addressed by their member set, not by name, and
    /// are never listed in the public directory.
    #[inline]
    pub fn is_direct(self) -> bool {
        matches!(self, ChannelKind::Dm | ChannelKind::Group)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub id: Id,
    #[serde(rename = "k")]
    pub kind: ChannelKind,
    /// Empty for DMs and group DMs — clients render those from the member list.
    #[serde(rename = "n", default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(rename = "t", default, skip_serializing_if = "String::is_empty")]
    pub topic: String,
    #[serde(rename = "cb")]
    pub created_by: Id,
    #[serde(rename = "arc", default, skip_serializing_if = "is_false")]
    pub archived: bool,
    /// Members of DMs/groups; empty for named channels, whose membership is
    /// fetched on demand (it can be thousands of rows).
    #[serde(rename = "m", default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<Id>,
    /// Newest message in the channel, for unread math and sorting the sidebar.
    #[serde(rename = "last", default, skip_serializing_if = "is_zero_id")]
    pub last_message: Id,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: Id,
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "mt")]
    pub mime: String,
    #[serde(rename = "sz")]
    pub size: u64,
    /// Pixel dimensions when the blob is an image, so the client can reserve
    /// layout space before the bytes arrive and avoid a scroll jump.
    #[serde(rename = "w", default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(rename = "hh", default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// Aggregated reaction counts, used in *history* responses only.
///
/// Live reaction updates travel as per-user deltas ([`crate::proto::ServerFrame::React`])
/// precisely because `me` is viewer-dependent: a broadcast frame must be
/// identical for every recipient or it cannot be encoded once and shared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReactionSummary {
    #[serde(rename = "e")]
    pub emoji: String,
    #[serde(rename = "c")]
    pub count: u32,
    /// Whether the requesting user is among the reactors.
    #[serde(rename = "me", default, skip_serializing_if = "is_false")]
    pub me: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: Id,
    #[serde(rename = "ch")]
    pub channel_id: Id,
    #[serde(rename = "au")]
    pub author_id: Id,
    #[serde(rename = "b")]
    pub body: String,
    /// Set on replies; points at the message that started the thread.
    #[serde(rename = "th", default, skip_serializing_if = "Option::is_none")]
    pub thread_root: Option<Id>,
    /// Replies to this message, when it is itself a thread root.
    #[serde(rename = "rc", default, skip_serializing_if = "is_zero_u32")]
    pub reply_count: u32,
    /// Wall-clock ms of the last edit; `None` if never edited.
    #[serde(rename = "ed", default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<u64>,
    /// Tombstone: the row survives so thread structure and IDs stay stable,
    /// but the body is cleared.
    #[serde(rename = "del", default, skip_serializing_if = "is_false")]
    pub deleted: bool,
    #[serde(rename = "at", default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    #[serde(rename = "rx", default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<ReactionSummary>,
    /// User IDs explicitly mentioned, precomputed at write time so unread
    /// badge math never has to re-parse message bodies.
    #[serde(rename = "mn", default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<Id>,
}

impl Message {
    #[inline]
    pub fn is_reply(&self) -> bool {
        self.thread_root.is_some()
    }
}

/// How far a user has read in one channel.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReadState {
    #[serde(rename = "ch")]
    pub channel_id: Id,
    #[serde(rename = "lr")]
    pub last_read: Id,
    #[serde(rename = "u")]
    pub unread: u32,
    /// Unread messages that `@`-mention the user (or `@channel`). Drives the
    /// red badge, as opposed to the plain bold-unread state.
    #[serde(rename = "mn")]
    pub mentions: u32,
    /// Muted: the client suppresses this channel's unread badge.
    ///
    /// The counts are still reported truthfully rather than zeroed. Muting is a
    /// presentation choice, and a client that wants to show "12 unread, quietly"
    /// should not have to ask the server a second time to find out.
    #[serde(rename = "mu", default, skip_serializing_if = "is_false")]
    pub muted: bool,
}

/// A search hit: the message plus enough context to render a result row
/// without a second round trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    #[serde(rename = "m")]
    pub message: Message,
    /// Body with `\u{2}`/`\u{3}` sentinels around matched terms. Sentinels
    /// rather than HTML so the client can escape first, then mark up — an
    /// injection-safe ordering.
    #[serde(rename = "sn")]
    pub snippet: String,
}

#[inline]
fn is_false(b: &bool) -> bool {
    !*b
}

#[inline]
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

#[inline]
fn is_zero_id(v: &Id) -> bool {
    v.is_zero()
}

/// Marks the start of a highlighted run in [`SearchHit::snippet`].
pub const HL_START: char = '\u{2}';
/// Marks the end of a highlighted run in [`SearchHit::snippet`].
pub const HL_END: char = '\u{3}';
