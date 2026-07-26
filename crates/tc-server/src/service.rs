//! Operations that change state and then tell people about it.
//!
//! Both transports call in here. The WebSocket path uses it because that is
//! where interactive clients live; the REST path uses it so bots and
//! integrations behave identically. Keeping "write to the database" and
//! "broadcast the event" in one function is what prevents the two from drifting
//! — a message that persists but is never broadcast is invisible until reload,
//! and a broadcast without a write is a message that vanishes.

use tc_core::text::{self, Mention};
use tc_core::{Channel, ChannelKind, Id, Message, ReadState, ServerFrame, User, now_ms};
use tc_store::{NewChannel, NewMessage};

use crate::error::{ApiError, ApiResult};
use crate::state::Shared;

/// Resolve `@mentions` in a body to user ids.
///
/// `@channel`/`@here` expand to the channel's membership, so a broadcast
/// mention lights up everyone's badge without a special case downstream. The
/// expansion is bounded by the channel's size, and self-mentions are dropped by
/// the store.
async fn resolve_mentions(st: &Shared, channel: Id, body: &str) -> ApiResult<Vec<Id>> {
    let found = text::scan_mentions(body);
    if found.is_empty() {
        return Ok(Vec::new());
    }

    let handles: Vec<String> = found
        .iter()
        .filter_map(|m| match m {
            Mention::User(h) => Some(h.clone()),
            _ => None,
        })
        .collect();
    let broadcast = found
        .iter()
        .any(|m| matches!(m, Mention::Channel | Mention::Here));

    let mut ids = st
        .db(move |s| s.ids_for_handles(&handles))
        .await?
        .into_iter()
        .map(|(_, id)| id)
        .collect::<Vec<_>>();

    if broadcast {
        ids.extend(st.db(move |s| s.members(channel)).await?);
    }
    ids.sort_unstable();
    ids.dedup();
    ids.truncate(text::MAX_MENTIONS_PER_MESSAGE);
    Ok(ids)
}

/// Persist a message and broadcast it to the channel.
pub async fn post_message(
    st: &Shared,
    author: &User,
    channel: Id,
    body: &str,
    thread_root: Option<Id>,
    attachments: Vec<Id>,
) -> ApiResult<Message> {
    let Some(clean) = text::clean_body(body).map_err(|e| ApiError::BadRequest(e.into()))? else {
        return Err(ApiError::BadRequest("message is empty".into()));
    };
    let mentions = resolve_mentions(st, channel, clean).await?;

    let id = st.next_id();
    let author_id = author.id;
    let body = clean.to_string();
    let message = st
        .db(move |s| {
            s.insert_message(NewMessage {
                id,
                channel_id: channel,
                author_id,
                body: &body,
                thread_root,
                attachments: &attachments,
                mentions: &mentions,
            })
        })
        .await?;

    // One encode, N deliveries.
    st.hub.broadcast_frame(
        channel,
        &ServerFrame::Msg {
            message: message.clone(),
        },
    );
    Ok(message)
}

pub async fn edit_message(st: &Shared, author: Id, id: Id, body: &str) -> ApiResult<Message> {
    let Some(clean) = text::clean_body(body).map_err(|e| ApiError::BadRequest(e.into()))? else {
        return Err(ApiError::BadRequest("message is empty".into()));
    };
    let now = now_ms();
    let owned = clean.to_string();
    st.db(move |s| s.edit_message(id, author, &owned, now))
        .await?;

    // Hydrated: the caller replaces its local copy with this, so it must carry
    // the message's reactions and attachments, not just the edited body.
    let updated = st.db(move |s| s.message_for(id, author)).await?;
    st.hub.broadcast_frame(
        updated.channel_id,
        &ServerFrame::MsgEdit {
            id,
            channel: updated.channel_id,
            body: updated.body.clone(),
            edited_at: now,
        },
    );
    Ok(updated)
}

pub async fn delete_message(st: &Shared, author: Id, id: Id) -> ApiResult<Id> {
    let channel = st.db(move |s| s.delete_message(id, author, false)).await?;
    st.hub
        .broadcast_frame(channel, &ServerFrame::MsgDel { id, channel });
    Ok(channel)
}

/// Toggle a reaction and broadcast the delta.
///
/// The broadcast is a per-user delta, never an aggregate: an aggregate would
/// have to carry a viewer-specific `me` flag, which would make the frame
/// impossible to encode once and share.
pub async fn set_reaction(
    st: &Shared,
    user: Id,
    message: Id,
    emoji: &str,
    on: bool,
) -> ApiResult<()> {
    if emoji.is_empty() || emoji.len() > text::MAX_EMOJI_LEN {
        return Err(ApiError::BadRequest("invalid emoji".into()));
    }
    let e = emoji.to_string();
    let (channel, changed) = st
        .db(move |s| s.set_reaction(message, user, &e, on))
        .await?;
    // Re-broadcasting an unchanged toggle would double-count on every client.
    if changed {
        st.hub.broadcast_frame(
            channel,
            &ServerFrame::React {
                id: message,
                channel,
                emoji: emoji.to_string(),
                user,
                on,
            },
        );
    }
    Ok(())
}

/// Advance a read cursor and echo the new state to that user's other tabs.
pub async fn mark_read(st: &Shared, user: Id, channel: Id, up_to: Id) -> ApiResult<ReadState> {
    let state = st.db(move |s| s.mark_read(channel, user, up_to)).await?;
    st.hub
        .send_to_user(user, &ServerFrame::Read { read: state });
    Ok(state)
}

/// Create a named channel and announce it to its initial members.
pub async fn create_channel(
    st: &Shared,
    creator: &User,
    name: &str,
    kind: ChannelKind,
    topic: &str,
    members: Vec<Id>,
) -> ApiResult<Channel> {
    if kind.is_direct() {
        return Err(ApiError::BadRequest(
            "use the direct-message endpoint for DMs".into(),
        ));
    }
    text::validate_channel_name(name).map_err(|e| ApiError::BadRequest(e.into()))?;
    if topic.len() > text::MAX_TOPIC_LEN {
        return Err(ApiError::BadRequest("topic is too long".into()));
    }

    let id = st.next_id();
    let (name_o, topic_o) = (name.to_string(), topic.to_string());
    let creator_id = creator.id;
    let member_list = members.clone();
    let channel = st
        .db(move |s| {
            s.create_channel(NewChannel {
                id,
                kind,
                name: &name_o,
                topic: &topic_o,
                created_by: creator_id,
                created_at: now_ms(),
                members: member_list,
            })
        })
        .await?;

    // Nobody is subscribed to a channel that did not exist a moment ago, so
    // this goes to each user rather than to the channel — and each of their
    // live connections is subscribed here, server-side, so they start
    // receiving messages immediately rather than after a reload.
    let frame = ServerFrame::Chan {
        channel: channel.clone(),
    };
    for m in members.iter().chain(std::iter::once(&creator.id)) {
        st.hub.subscribe_user(*m, channel.id);
        st.hub.send_to_user(*m, &frame);
    }
    Ok(channel)
}

/// Find or create a direct conversation.
pub async fn open_dm(st: &Shared, creator: &User, with: Vec<Id>) -> ApiResult<Channel> {
    if with.is_empty() || with.len() > 8 {
        return Err(ApiError::BadRequest(
            "a direct message needs between one and eight other people".into(),
        ));
    }
    let id = st.next_id();
    let creator_id = creator.id;
    let channel = st
        .db(move |s| s.open_dm(id, creator_id, with, now_ms()))
        .await?;

    let frame = ServerFrame::Chan {
        channel: channel.clone(),
    };
    for m in &channel.members {
        st.hub.subscribe_user(*m, channel.id);
        st.hub.send_to_user(*m, &frame);
    }
    Ok(channel)
}

/// Join a public channel. Private channels are invite-only, so this refuses
/// them rather than letting anyone who knows an id walk in.
pub async fn join_channel(st: &Shared, user: &User, channel: Id) -> ApiResult<Channel> {
    let target = st.db(move |s| s.channel(channel)).await?;
    if target.kind != ChannelKind::Public {
        return Err(ApiError::Forbidden);
    }
    if target.archived {
        return Err(ApiError::BadRequest("channel is archived".into()));
    }

    let user_id = user.id;
    let joined = st
        .db(move |s| s.join_channel(channel, user_id, now_ms()))
        .await?;
    if joined {
        // Subscribe before broadcasting, so the joiner sees their own arrival
        // and every message that follows it.
        st.hub.subscribe_user(user_id, channel);
        st.hub.broadcast_frame(
            channel,
            &ServerFrame::Member {
                channel,
                user: user_id,
                join: true,
            },
        );
        st.hub.send_to_user(
            user_id,
            &ServerFrame::Chan {
                channel: target.clone(),
            },
        );
    }
    Ok(target)
}

pub async fn leave_channel(st: &Shared, user: &User, channel: Id) -> ApiResult<()> {
    let target = st.db(move |s| s.channel(channel)).await?;
    if target.kind.is_direct() {
        return Err(ApiError::BadRequest(
            "direct messages cannot be left".into(),
        ));
    }
    let user_id = user.id;
    if st.db(move |s| s.leave_channel(channel, user_id)).await? {
        // Broadcast before dropping their subscription, so the departing
        // client sees its own departure confirmed.
        st.hub.broadcast_frame(
            channel,
            &ServerFrame::Member {
                channel,
                user: user_id,
                join: false,
            },
        );
    }
    Ok(())
}
