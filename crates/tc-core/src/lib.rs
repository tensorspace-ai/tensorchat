//! `tc-core` — the vocabulary of TensorChat.
//!
//! Domain types, the WebSocket wire protocol, ID generation, and the text
//! rules that bound user input. It has no I/O and no async runtime, so both
//! the storage layer and the server can depend on it without either depending
//! on the other.
//!
//! The two ideas worth knowing before reading anything else:
//!
//! * [`id::Id`] — every entity is a time-sortable u64, which makes the primary
//!   key double as the pagination cursor and the time index.
//! * [`proto`] — broadcast frames carry no viewer-specific fields, which is
//!   what allows the server to serialize an event once and share the bytes
//!   with every subscriber.

pub mod id;
pub mod model;
pub mod proto;
pub mod text;

pub use id::{Id, IdGen};
pub use model::{
    Attachment, Channel, ChannelKind, Message, Presence, ReactionSummary, ReadState, SearchHit,
    User,
};
pub use proto::{ClientFrame, ErrCode, PROTOCOL_VERSION, ServerFrame};

/// Wall-clock milliseconds since the Unix epoch.
///
/// Used for `edited_at` and session expiry — anywhere a human-facing timestamp
/// is needed that is not already implied by an [`Id`].
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
