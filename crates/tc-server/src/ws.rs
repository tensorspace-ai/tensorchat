//! The WebSocket endpoint: one task pair per connection.
//!
//! Each connection runs a **reader** task and a **writer** task. Splitting them
//! is what makes backpressure work: the writer owns the socket's send half and
//! drains the hub's queue, so a client that stops reading stalls only its own
//! writer while the reader keeps processing (and the hub eventually evicts it).
//! A single-task design would have to interleave both, and a blocked send would
//! stop the connection from responding at all.

use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tc_core::{ClientFrame, ErrCode, Id, PROTOCOL_VERSION, Presence, ServerFrame, User};

use crate::error::ApiError;
use crate::ratelimit::ConnLimits;
use crate::service;
use crate::state::{Shared, cookie_value};

/// How often to ping an idle connection.
const HEARTBEAT: Duration = Duration::from_secs(30);
/// A connection that has not produced any traffic for this long is dead, even
/// if the TCP socket still looks open (the classic silently-dropped NAT entry).
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Cap on a single inbound frame. Message bodies are already capped well below
/// this; anything larger is a client bug or an attack.
const MAX_FRAME_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
pub struct WsQuery {
    /// Browsers cannot set headers on a WebSocket handshake, so the token may
    /// arrive as a query parameter. It is also accepted as a cookie.
    token: Option<String>,
}

pub async fn handler(
    ws: WebSocketUpgrade,
    State(st): State<Shared>,
    Query(q): Query<WsQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    // Authenticate during the handshake so an unauthenticated peer never gets
    // an upgraded socket at all.
    let token = q
        .token
        .or_else(|| {
            headers
                .get(axum::http::header::COOKIE)
                .and_then(|v| v.to_str().ok())
                .and_then(|c| cookie_value(c, "tc_session"))
        })
        .ok_or(ApiError::Unauthorized)?;

    let hash = crate::auth::token_hash(&token);
    let now = tc_core::now_ms();
    let user = st
        .db(move |s| s.session_user(&hash, now))
        .await
        .map_err(|_| ApiError::Unauthorized)?;

    Ok(ws
        .max_message_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| run(socket, st, user)))
}

/// Drive one connection to completion.
async fn run(socket: WebSocket, st: Shared, user: User) {
    let (conn_id, mut rx) = st.hub.connect(user.id);
    let (mut sink, mut stream) = socket.split();

    // Writer: the only task that touches the send half.
    let writer = tokio::spawn(async move {
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                frame = rx.recv() => match frame {
                    // `Bytes` all the way to the socket: no copy between the
                    // hub's shared buffer and the write call.
                    Some(bytes) => {
                        if sink.send(WsMessage::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                _ = heartbeat.tick() => {
                    if sink.send(WsMessage::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink.close().await;
    });

    // Presence transition only on the *first* connection — opening a second
    // tab should not re-announce a user who is already online.
    if st.hub.conns_for_user(user.id) == 1 {
        broadcast_presence(&st, user.id, Presence::Online).await;
    }

    send_ready(&st, conn_id, &user).await;

    let mut limits = ConnLimits::default();
    let reader = async {
        loop {
            let next = tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await;
            let msg = match next {
                Err(_) => {
                    tracing::debug!(conn = conn_id, "idle timeout");
                    break;
                }
                Ok(None) => break,
                Ok(Some(Err(e))) => {
                    tracing::debug!(conn = conn_id, error = %e, "socket error");
                    break;
                }
                Ok(Some(Ok(m))) => m,
            };

            match msg {
                WsMessage::Binary(data) => {
                    let Ok(frame) = ClientFrame::decode(&data) else {
                        st.hub.send_to_conn(
                            conn_id,
                            &ServerFrame::Err {
                                code: ErrCode::BadRequest,
                                msg: "undecodable frame".into(),
                            },
                        );
                        continue;
                    };
                    if !handle(&st, conn_id, &user, frame, &mut limits).await {
                        break;
                    }
                }
                // Text frames are not part of this protocol; the wire format is
                // MessagePack, and accepting JSON here would mean maintaining
                // two decoders.
                WsMessage::Text(_) => {
                    st.hub.send_to_conn(
                        conn_id,
                        &ServerFrame::Err {
                            code: ErrCode::BadRequest,
                            msg: "this endpoint speaks binary MessagePack".into(),
                        },
                    );
                }
                WsMessage::Close(_) => break,
                // Pong/Ping are handled by the transport; receiving them just
                // proves liveness, which the timeout above already tracks.
                WsMessage::Ping(_) | WsMessage::Pong(_) => {}
            }
        }
    };
    reader.await;

    // Teardown: remove from the hub, then let the writer finish.
    let last = st.hub.disconnect(conn_id);
    writer.abort();
    if last {
        broadcast_presence(&st, user.id, Presence::Offline).await;
    }
    tracing::debug!(conn = conn_id, user = %user.id, "connection closed");
}

/// Tell everyone who shares a channel with this user about a presence change.
///
/// Presence is broadcast per channel rather than workspace-wide so a user only
/// learns about people they can actually see.
async fn broadcast_presence(st: &Shared, user: Id, presence: Presence) {
    let Ok(channels) = st.db(move |s| s.channels_for_user(user)).await else {
        return;
    };
    let frame = ServerFrame::Presence { user, presence };
    // Encode once for the whole fanout, not once per channel.
    let payload = st.hub.encode(&frame);
    for c in channels {
        st.hub.broadcast(c.id, &payload, None);
    }
}

/// Send the initial snapshot: everything needed to paint a full UI.
async fn send_ready(st: &Shared, conn: crate::hub::ConnId, user: &User) {
    let uid = user.id;
    let loaded = st
        .db(move |s| {
            let channels = s.channels_for_user(uid)?;
            let users = s.all_users()?;
            let read = s.read_states(uid)?;
            Ok((channels, users, read))
        })
        .await;

    let (channels, users, read) = match loaded {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "failed to load ready snapshot");
            st.hub.send_to_conn(
                conn,
                &ServerFrame::Err {
                    code: ErrCode::Internal,
                    msg: "failed to load workspace".into(),
                },
            );
            return;
        }
    };

    // Auto-subscribe to every channel the user belongs to. A client would
    // otherwise have to send a Sub frame immediately, and could miss messages
    // in the gap.
    let ids: Vec<Id> = channels.iter().map(|c| c.id).collect();
    st.hub.subscribe(conn, &ids);

    st.hub.send_to_conn(
        conn,
        &ServerFrame::Ready {
            me: user.clone(),
            channels,
            users,
            read,
            online: st.hub.online_users(),
            version: PROTOCOL_VERSION,
        },
    );
}

/// Handle one decoded client frame. Returns false to close the connection.
async fn handle(
    st: &Shared,
    conn: crate::hub::ConnId,
    user: &User,
    frame: ClientFrame,
    limits: &mut ConnLimits,
) -> bool {
    macro_rules! fail {
        ($code:expr, $msg:expr) => {{
            st.hub.send_to_conn(
                conn,
                &ServerFrame::Err {
                    code: $code,
                    msg: $msg.into(),
                },
            );
            return true;
        }};
    }

    /// Send an error frame for a failed operation.
    ///
    /// Internal failures carry a detail that must not reach the client — but
    /// discarding it entirely leaves a bug with no trace anywhere, since only
    /// the HTTP path's `IntoResponse` logs it. So log here too.
    macro_rules! fail_err {
        ($err:expr) => {{
            let e = $err;
            if let ApiError::Internal(detail) = &e {
                tracing::error!(conn, user = %user.id, detail, "internal error on websocket");
            }
            fail!(e.code(), e.to_string())
        }};
    }

    match frame {
        // Already authenticated during the handshake. A second Hello is a
        // confused client, not an attack; ignore it.
        ClientFrame::Hello { .. } => {}

        ClientFrame::Ping { t } => {
            if !limits.misc.allow() {
                fail!(ErrCode::RateLimited, "slow down");
            }
            st.hub.send_to_conn(conn, &ServerFrame::Pong { t });
        }

        ClientFrame::Sub { channels } => {
            if !limits.misc.allow() {
                fail!(ErrCode::RateLimited, "slow down");
            }
            // Subscribing is an authorization decision: filter to channels the
            // user is actually in, or a client could listen anywhere.
            let uid = user.id;
            let wanted = channels.clone();
            let Ok(allowed) = st
                .db(move |s| {
                    Ok(wanted
                        .into_iter()
                        .filter(|c| s.is_member(*c, uid).unwrap_or(false))
                        .collect::<Vec<_>>())
                })
                .await
            else {
                fail!(ErrCode::Internal, "subscription failed");
            };
            if allowed.len() != channels.len() {
                st.hub.send_to_conn(
                    conn,
                    &ServerFrame::Err {
                        code: ErrCode::Forbidden,
                        msg: "some channels were not subscribable".into(),
                    },
                );
            }
            st.hub.subscribe(conn, &allowed);
        }

        ClientFrame::Unsub { channels } => {
            st.hub.unsubscribe(conn, &channels);
        }

        ClientFrame::Send {
            nonce,
            channel,
            body,
            thread_root,
            attachments,
        } => {
            if !limits.messages.allow() {
                fail!(ErrCode::RateLimited, "sending too fast");
            }
            match service::post_message(st, user, channel, &body, thread_root, attachments).await {
                Ok(m) => {
                    // The Ack is per-connection: it carries the client's nonce,
                    // which is meaningless to anyone else.
                    st.hub
                        .send_to_conn(conn, &ServerFrame::Ack { nonce, id: m.id });
                }
                Err(e) => fail_err!(e),
            }
        }

        ClientFrame::Edit { id, body } => {
            if !limits.messages.allow() {
                fail!(ErrCode::RateLimited, "slow down");
            }
            if let Err(e) = service::edit_message(st, user.id, id, &body).await {
                fail_err!(e);
            }
        }

        ClientFrame::Del { id } => {
            if !limits.misc.allow() {
                fail!(ErrCode::RateLimited, "slow down");
            }
            if let Err(e) = service::delete_message(st, user.id, id).await {
                fail_err!(e);
            }
        }

        ClientFrame::React { id, emoji, on } => {
            if !limits.misc.allow() {
                fail!(ErrCode::RateLimited, "slow down");
            }
            if let Err(e) = service::set_reaction(st, user.id, id, &emoji, on).await {
                fail_err!(e);
            }
        }

        ClientFrame::Typing { channel } => {
            // Typing indicators are dropped silently when over quota: an error
            // frame would cost more than the indicator it is refusing.
            if !limits.typing.allow() {
                return true;
            }
            let uid = user.id;
            if st
                .db(move |s| s.is_member(channel, uid))
                .await
                .unwrap_or(false)
            {
                let payload = st.hub.encode(&ServerFrame::Typing { channel, user: uid });
                // `except` the sender: you already know you are typing.
                st.hub.broadcast(channel, &payload, Some(conn));
            }
        }

        ClientFrame::Read { channel, up_to } => {
            if !limits.misc.allow() {
                fail!(ErrCode::RateLimited, "slow down");
            }
            if let Err(e) = service::mark_read(st, user.id, channel, up_to).await {
                fail_err!(e);
            }
        }

        ClientFrame::Presence { presence } => {
            if !limits.misc.allow() {
                fail!(ErrCode::RateLimited, "slow down");
            }
            if let Some(effective) = st.hub.set_presence(user.id, presence) {
                broadcast_presence(st, user.id, effective).await;
            }
        }
    }
    true
}

/// Values here are protocol constants the frontend also encodes; a mismatch
/// would show up as spurious disconnects, so they are asserted rather than
/// left as prose.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_fires_well_inside_the_idle_timeout() {
        // Otherwise a healthy but quiet connection would be reaped between
        // pings.
        assert!(
            HEARTBEAT * 2 < IDLE_TIMEOUT,
            "need at least two heartbeats within the idle window"
        );
    }

    /// A maximum-length message plus its framing must fit inside the inbound
    /// frame cap. Both are compile-time constants, so this is checked at build
    /// time rather than by running a test.
    const _: () = assert!(MAX_FRAME_BYTES > tc_core::text::MAX_BODY_BYTES * 2);
}
