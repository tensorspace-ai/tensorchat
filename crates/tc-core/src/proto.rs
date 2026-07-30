//! The WebSocket wire protocol.
//!
//! # Design constraint: broadcast frames are viewer-independent
//!
//! Every [`ServerFrame`] that fans out to more than one recipient contains no
//! per-viewer state. That is what lets the hub encode an event **once** and
//! hand every subscriber the same refcounted buffer, instead of running serde
//! once per connection. Anything viewer-dependent (`me` on a reaction, unread
//! counts) is either sent as a per-user frame or derived client-side from a
//! delta.
//!
//! If you add a field here, ask: *would two users receive different bytes?* If
//! yes, it does not belong on a broadcast frame.

use crate::id::Id;
use crate::model::{Channel, Message, Presence, ReadState, User};
use serde::{Deserialize, Serialize};

/// Protocol version, negotiated in [`ClientFrame::Hello`]. Bumped on any
/// breaking change to frame shapes.
pub const PROTOCOL_VERSION: u16 = 1;

/// Frames sent by a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientFrame {
    /// Must be the first frame. The connection is closed if anything else
    /// arrives before it.
    Hello {
        #[serde(rename = "tk")]
        token: String,
        #[serde(rename = "v", default)]
        version: u16,
    },
    /// Start receiving events for these channels. Idempotent.
    Sub {
        #[serde(rename = "ch")]
        channels: Vec<Id>,
    },
    Unsub {
        #[serde(rename = "ch")]
        channels: Vec<Id>,
    },
    /// Post a message. `nonce` is echoed in [`ServerFrame::Ack`] so the client
    /// can reconcile its optimistic local echo with the authoritative ID.
    Send {
        #[serde(rename = "n")]
        nonce: u32,
        #[serde(rename = "ch")]
        channel: Id,
        #[serde(rename = "b")]
        body: String,
        #[serde(rename = "th", default)]
        thread_root: Option<Id>,
        #[serde(rename = "at", default)]
        attachments: Vec<Id>,
    },
    Edit {
        id: Id,
        #[serde(rename = "b")]
        body: String,
    },
    Del {
        id: Id,
    },
    /// `on = false` removes the reaction. Idempotent in both directions.
    React {
        id: Id,
        #[serde(rename = "e")]
        emoji: String,
        #[serde(rename = "on")]
        on: bool,
    },
    /// Fire-and-forget typing indicator; rate-limited server-side.
    Typing {
        #[serde(rename = "ch")]
        channel: Id,
    },
    /// Advance the read cursor. Monotonic — older values are ignored.
    Read {
        #[serde(rename = "ch")]
        channel: Id,
        #[serde(rename = "up")]
        up_to: Id,
    },
    /// Manual presence override (e.g. the user picked "away").
    Presence {
        #[serde(rename = "p")]
        presence: Presence,
    },
    /// Liveness probe. `t` is opaque client time, echoed back for RTT math.
    Ping {
        #[serde(rename = "t")]
        t: u64,
    },
}

/// Frames sent by the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerFrame {
    /// Sent once after a successful `Hello`: everything the client needs to
    /// paint a full UI without another round trip.
    Ready {
        #[serde(rename = "me")]
        me: User,
        #[serde(rename = "ch")]
        channels: Vec<Channel>,
        /// Every user the client can currently see. A workspace-scale
        /// deployment would page this; at core-feature scope, one shot is both
        /// simpler and fewer bytes than N lookups.
        #[serde(rename = "us")]
        users: Vec<User>,
        #[serde(rename = "rs")]
        read: Vec<ReadState>,
        #[serde(rename = "on")]
        online: Vec<Id>,
        #[serde(rename = "v")]
        version: u16,
    },
    /// Confirms a [`ClientFrame::Send`]. Per-connection, never broadcast.
    Ack {
        #[serde(rename = "n")]
        nonce: u32,
        id: Id,
    },
    /// A new message. **Broadcast** — identical bytes for every subscriber.
    Msg {
        #[serde(rename = "m")]
        message: Message,
    },
    MsgEdit {
        id: Id,
        #[serde(rename = "ch")]
        channel: Id,
        #[serde(rename = "b")]
        body: String,
        #[serde(rename = "ed")]
        edited_at: u64,
    },
    MsgDel {
        id: Id,
        #[serde(rename = "ch")]
        channel: Id,
    },
    /// A per-user reaction delta. Clients fold these into local counts and
    /// decide `me` themselves — see the module note on viewer-independence.
    React {
        id: Id,
        #[serde(rename = "ch")]
        channel: Id,
        #[serde(rename = "e")]
        emoji: String,
        #[serde(rename = "u")]
        user: Id,
        #[serde(rename = "on")]
        on: bool,
    },
    /// A message was pinned or unpinned. **Broadcast** — a pin is a property of
    /// the channel, identical for everyone in it, so unlike a reaction there is
    /// no per-viewer flag to keep out of the frame.
    Pin {
        id: Id,
        #[serde(rename = "ch")]
        channel: Id,
        #[serde(rename = "by")]
        by: Id,
        #[serde(rename = "on")]
        on: bool,
    },
    Typing {
        #[serde(rename = "ch")]
        channel: Id,
        #[serde(rename = "u")]
        user: Id,
    },
    Presence {
        #[serde(rename = "u")]
        user: Id,
        #[serde(rename = "p")]
        presence: Presence,
    },
    /// Read state echo. Per-user by definition; lets a second tab or device
    /// clear its badge when you read on the first.
    Read {
        #[serde(rename = "rs")]
        read: ReadState,
    },
    /// A channel was created or its metadata changed.
    Chan {
        #[serde(rename = "c")]
        channel: Channel,
    },
    /// Membership delta. `join = false` means left/removed.
    Member {
        #[serde(rename = "ch")]
        channel: Id,
        #[serde(rename = "u")]
        user: Id,
        #[serde(rename = "j")]
        join: bool,
    },
    /// A user profile changed (display name, status, deactivation).
    UserUpd {
        #[serde(rename = "u")]
        user: User,
    },
    Pong {
        #[serde(rename = "t")]
        t: u64,
    },
    Err {
        #[serde(rename = "c")]
        code: ErrCode,
        #[serde(rename = "m")]
        msg: String,
    },
}

/// Machine-readable failure reasons. Clients switch on these; the accompanying
/// message is for humans and logs only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrCode {
    /// Token missing, malformed, or expired. The client must re-authenticate;
    /// reconnecting with the same token will not help.
    Unauthorized,
    /// Authenticated, but not a member of the target channel.
    Forbidden,
    NotFound,
    /// Malformed frame or a field that failed validation.
    BadRequest,
    /// Rate limit tripped. Back off; the connection stays open.
    RateLimited,
    /// The client fell behind and its send queue overflowed. It should
    /// reconnect and refetch history, because events were dropped.
    Overloaded,
    Internal,
}

impl ServerFrame {
    /// Encode with named-field MessagePack, the format both ends agree on.
    pub fn encode(&self) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        rmp_serde::to_vec_named(self)
    }
}

impl ClientFrame {
    pub fn decode(bytes: &[u8]) -> Result<Self, rmp_serde::decode::Error> {
        rmp_serde::from_slice(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ChannelKind;

    fn sample_message() -> Message {
        Message {
            id: Id(1 << 40),
            channel_id: Id(99),
            author_id: Id(7),
            body: "hello world".into(),
            thread_root: None,
            reply_count: 0,
            edited_at: None,
            deleted: false,
            attachments: vec![],
            reactions: vec![],
            mentions: vec![],
        }
    }

    #[test]
    fn frames_roundtrip_through_msgpack() {
        let frames = vec![
            ServerFrame::Msg {
                message: sample_message(),
            },
            ServerFrame::Ack {
                nonce: 42,
                id: Id(5),
            },
            ServerFrame::React {
                id: Id(1),
                channel: Id(2),
                emoji: "🎉".into(),
                user: Id(3),
                on: true,
            },
            ServerFrame::Err {
                code: ErrCode::RateLimited,
                msg: "slow down".into(),
            },
            ServerFrame::Chan {
                channel: Channel {
                    id: Id(2),
                    kind: ChannelKind::Public,
                    name: "general".into(),
                    topic: String::new(),
                    created_by: Id(7),
                    archived: false,
                    members: vec![],
                    last_message: Id::ZERO,
                },
            },
        ];
        for f in frames {
            let bytes = f.encode().unwrap();
            let back: ServerFrame = rmp_serde::from_slice(&bytes).unwrap();
            assert_eq!(f, back);
        }
    }

    #[test]
    fn client_frames_roundtrip() {
        let f = ClientFrame::Send {
            nonce: 9,
            channel: Id(3),
            body: "hi".into(),
            thread_root: Some(Id(4)),
            attachments: vec![Id(5)],
        };
        assert_eq!(
            ClientFrame::decode(&rmp_serde::to_vec_named(&f).unwrap()).unwrap(),
            f
        );
    }

    #[test]
    fn empty_fields_are_elided_from_the_wire() {
        // The default-heavy shape of a plain chat message is the common case;
        // skipping empty vectors and false flags is most of the framing win.
        let full = rmp_serde::to_vec_named(&ServerFrame::Msg {
            message: sample_message(),
        })
        .unwrap()
        .len();
        let mut noisy = sample_message();
        noisy.reactions = vec![crate::model::ReactionSummary {
            emoji: "x".into(),
            count: 1,
            me: false,
        }];
        let with_extras = rmp_serde::to_vec_named(&ServerFrame::Msg { message: noisy })
            .unwrap()
            .len();
        assert!(full < with_extras);
        // A plain 11-char message should stay comfortably under 100 bytes.
        assert!(full < 100, "message frame was {full} bytes");
    }

    #[test]
    fn unknown_frames_are_rejected_not_silently_accepted() {
        assert!(ClientFrame::decode(b"\x00garbage").is_err());
    }
}
