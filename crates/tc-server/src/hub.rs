//! The realtime fanout hub.
//!
//! # The central optimization
//!
//! When a message lands in a channel with N connected members, the naive
//! implementation serializes the event N times — once per socket. Serialization
//! is the single most expensive step in the delivery path, and it is doing the
//! *identical* work N times.
//!
//! Here, an event is encoded **once** into a [`Bytes`] buffer, and every
//! subscriber receives a clone of that handle. `Bytes` is refcounted, so a
//! clone is an atomic increment — not a copy of the payload. Broadcasting to
//! 10,000 sockets costs one `rmp_serde` pass and 10,000 pointer bumps.
//!
//! This is only sound because broadcast frames are viewer-independent by
//! construction (see `tc_core::proto`). Anything per-viewer — an `Ack`, a read
//! receipt — goes through [`Hub::send_to_user`] instead, which encodes per
//! recipient because it must.
//!
//! # Backpressure
//!
//! Each connection owns a bounded queue. A client that stops reading (a
//! suspended laptop, a dead network) must not be allowed to make the server
//! buffer without limit, and must not be allowed to *block* the broadcaster and
//! stall delivery for everyone else in the channel. So sends are non-blocking:
//! if a connection's queue is full it is marked overloaded and disconnected,
//! and the client reconnects and refetches history. Dropping one slow consumer
//! is strictly better than degrading the channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::RwLock;
use tc_core::{Id, Presence, ServerFrame};
use tokio::sync::mpsc;

/// Per-connection outbound queue depth.
///
/// Sized to absorb a burst (a busy channel, a client that blinked) without
/// letting a genuinely dead peer accumulate megabytes. At ~200 bytes/frame this
/// is well under 64 KiB per connection.
const SEND_QUEUE: usize = 256;

/// Identifies one WebSocket connection. A user may have several (tabs,
/// devices), each with its own subscriptions and queue.
pub type ConnId = u64;

/// A frame that has already been encoded, ready to hand to any number of
/// sockets. Cloning is a refcount bump.
pub type Encoded = Bytes;

struct Conn {
    user: Id,
    tx: mpsc::Sender<Encoded>,
    /// Set when the connection is being torn down, so a full queue only
    /// triggers one disconnect even if several broadcasts race.
    closing: AtomicBool,
}

/// Realtime routing table: who is connected, and who is listening to what.
///
/// All maps are sharded (`DashMap`), so a broadcast to channel A does not
/// contend with a broadcast to channel B.
pub struct Hub {
    conns: DashMap<ConnId, Arc<Conn>>,
    /// channel -> connections currently subscribed.
    channel_subs: DashMap<Id, Arc<RwLock<Vec<ConnId>>>>,
    /// user -> their connections. Backs per-user sends and presence.
    user_conns: DashMap<Id, Arc<RwLock<Vec<ConnId>>>>,
    /// Manual presence overrides ("away"); absence means Online while
    /// connected.
    away: DashMap<Id, ()>,
    next_conn: AtomicU64,
    metrics: Metrics,
}

/// Counters for the `/api/metrics` endpoint. Relaxed ordering throughout: these
/// are observability, not synchronization.
#[derive(Default)]
pub struct Metrics {
    pub frames_encoded: AtomicU64,
    pub frames_delivered: AtomicU64,
    pub bytes_encoded: AtomicU64,
    pub dropped_slow: AtomicU64,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Hub {
    pub fn new() -> Hub {
        Hub {
            conns: DashMap::new(),
            channel_subs: DashMap::new(),
            user_conns: DashMap::new(),
            away: DashMap::new(),
            next_conn: AtomicU64::new(1),
            metrics: Metrics::default(),
        }
    }

    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Register a connection. The returned receiver is the socket's outbound
    /// queue; the writer task drains it.
    pub fn connect(&self, user: Id) -> (ConnId, mpsc::Receiver<Encoded>) {
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(SEND_QUEUE);
        self.conns.insert(
            id,
            Arc::new(Conn {
                user,
                tx,
                closing: AtomicBool::new(false),
            }),
        );
        self.user_conns
            .entry(user)
            .or_insert_with(|| Arc::new(RwLock::new(Vec::new())))
            .write()
            .push(id);
        (id, rx)
    }

    /// Tear down a connection and remove it from every subscription list.
    ///
    /// Returns `true` if this was the user's last connection, i.e. they just
    /// went offline and presence should be broadcast.
    pub fn disconnect(&self, conn: ConnId) -> bool {
        let Some((_, c)) = self.conns.remove(&conn) else {
            return false;
        };
        // Retain rather than remove-by-index: subscription lists are small and
        // this keeps the operation O(len) without index bookkeeping.
        self.channel_subs.retain(|_, subs| {
            subs.write().retain(|&id| id != conn);
            !subs.read().is_empty()
        });

        let mut last = false;
        if let Some(list) = self.user_conns.get(&c.user) {
            let mut w = list.write();
            w.retain(|&id| id != conn);
            last = w.is_empty();
        }
        if last {
            self.user_conns.remove(&c.user);
            self.away.remove(&c.user);
        }
        last
    }

    /// Subscribe a connection to channels. Idempotent.
    pub fn subscribe(&self, conn: ConnId, channels: &[Id]) {
        for ch in channels {
            let subs = self
                .channel_subs
                .entry(*ch)
                .or_insert_with(|| Arc::new(RwLock::new(Vec::new())))
                .clone();
            let mut w = subs.write();
            if !w.contains(&conn) {
                w.push(conn);
            }
        }
    }

    /// Subscribe every live connection belonging to `user` to `channel`.
    ///
    /// Called whenever someone gains access to a channel — they created it,
    /// joined it, were added to it, or opened a DM. Doing this server-side is
    /// what makes it reliable: a client that learns about a channel from a
    /// `Chan` frame would otherwise have to remember to subscribe, and any
    /// path that forgot (another tab, a bot, being added by someone else)
    /// would leave that user silently receiving no messages until reload.
    pub fn subscribe_user(&self, user: Id, channel: Id) {
        let Some(list) = self.user_conns.get(&user) else {
            return;
        };
        let conns: Vec<ConnId> = list.read().clone();
        drop(list);
        for c in conns {
            self.subscribe(c, &[channel]);
        }
    }

    pub fn unsubscribe(&self, conn: ConnId, channels: &[Id]) {
        for ch in channels {
            if let Some(subs) = self.channel_subs.get(ch) {
                subs.write().retain(|&id| id != conn);
            }
        }
    }

    /// The inverse of [`Hub::subscribe_user`]: drop every one of a user's
    /// connections from a channel.
    ///
    /// Called whenever someone *loses* access — they left, or were removed.
    /// Doing this server-side is not cosmetic: a subscription outlives
    /// membership until the socket reconnects, so without this a removed member
    /// keeps receiving a private channel's messages for as long as their tab
    /// stays open.
    pub fn unsubscribe_user(&self, user: Id, channel: Id) {
        let Some(list) = self.user_conns.get(&user) else {
            return;
        };
        let conns: Vec<ConnId> = list.read().clone();
        drop(list);
        for c in conns {
            self.unsubscribe(c, &[channel]);
        }
    }

    /// Encode a frame once. Callers that fan out to several destinations should
    /// encode here and pass the result around, rather than re-encoding.
    pub fn encode(&self, frame: &ServerFrame) -> Encoded {
        let bytes = frame.encode().unwrap_or_else(|e| {
            // Encoding a frame we constructed ourselves cannot fail on
            // well-formed data; if it somehow does, degrade to an error frame
            // rather than dropping the connection.
            tracing::error!(error = %e, "failed to encode frame");
            ServerFrame::Err {
                code: tc_core::ErrCode::Internal,
                msg: "encode failed".into(),
            }
            .encode()
            .unwrap_or_default()
        });
        self.metrics.frames_encoded.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .bytes_encoded
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Bytes::from(bytes)
    }

    /// Deliver an already-encoded frame to every subscriber of `channel`.
    ///
    /// Returns the number of connections it reached. `except` skips one
    /// connection — used so a sender does not receive an echo of a typing
    /// indicator it just sent.
    pub fn broadcast(&self, channel: Id, payload: &Encoded, except: Option<ConnId>) -> usize {
        let Some(subs) = self.channel_subs.get(&channel) else {
            return 0;
        };
        // Copy the subscriber ids out under a short read lock, then release it
        // before sending. Holding it across sends would let one slow queue
        // block every subsequent subscribe/unsubscribe on this channel.
        let targets: Vec<ConnId> = subs.read().clone();
        drop(subs);

        let mut sent = 0;
        for conn in targets {
            if Some(conn) == except {
                continue;
            }
            if self.deliver(conn, payload) {
                sent += 1;
            }
        }
        self.metrics
            .frames_delivered
            .fetch_add(sent as u64, Ordering::Relaxed);
        sent
    }

    /// Convenience for a frame with exactly one destination channel.
    pub fn broadcast_frame(&self, channel: Id, frame: &ServerFrame) -> usize {
        let payload = self.encode(frame);
        self.broadcast(channel, &payload, None)
    }

    /// Send to every connection belonging to one user (all their tabs).
    pub fn send_to_user(&self, user: Id, frame: &ServerFrame) -> usize {
        let Some(list) = self.user_conns.get(&user) else {
            return 0;
        };
        let targets: Vec<ConnId> = list.read().clone();
        drop(list);
        if targets.is_empty() {
            return 0;
        }
        // Still encode once — one user with five tabs is five deliveries.
        let payload = self.encode(frame);
        let mut sent = 0;
        for conn in targets {
            if self.deliver(conn, &payload) {
                sent += 1;
            }
        }
        self.metrics
            .frames_delivered
            .fetch_add(sent as u64, Ordering::Relaxed);
        sent
    }

    /// Send to a single connection.
    pub fn send_to_conn(&self, conn: ConnId, frame: &ServerFrame) -> bool {
        let payload = self.encode(frame);
        self.deliver(conn, &payload)
    }

    /// Non-blocking enqueue. A full queue means the peer is not draining, so we
    /// close the connection rather than buffer or stall the broadcaster.
    fn deliver(&self, conn: ConnId, payload: &Encoded) -> bool {
        // Clone the Arc and release the map guard immediately: the slow-consumer
        // path below removes from this same map, which would deadlock if a read
        // guard were still held.
        let c = match self.conns.get(&conn) {
            Some(r) => Arc::clone(&*r),
            None => return false,
        };

        match c.tx.try_send(payload.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Flag first, so concurrent broadcasters do not each count a
                // drop for the same connection.
                if !c.closing.swap(true, Ordering::AcqRel) {
                    self.metrics.dropped_slow.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(conn, user = %c.user, "dropping slow consumer");
                    // Dropping the map's Arc drops the only long-lived Sender.
                    // Once our local `c` goes out of scope the receiver sees a
                    // closed channel, the writer task ends, and it runs the
                    // normal `disconnect` cleanup path.
                    self.conns.remove(&conn);
                }
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Users with at least one live connection.
    pub fn online_users(&self) -> Vec<Id> {
        self.user_conns.iter().map(|e| *e.key()).collect()
    }

    pub fn presence_of(&self, user: Id) -> Presence {
        if !self.user_conns.contains_key(&user) {
            Presence::Offline
        } else if self.away.contains_key(&user) {
            Presence::Away
        } else {
            Presence::Online
        }
    }

    /// Record a manual presence override. Returns the resulting presence, or
    /// `None` if it did not change (so the caller can skip a broadcast).
    pub fn set_presence(&self, user: Id, presence: Presence) -> Option<Presence> {
        let before = self.presence_of(user);
        match presence {
            Presence::Away => {
                self.away.insert(user, ());
            }
            _ => {
                self.away.remove(&user);
            }
        }
        let after = self.presence_of(user);
        (after != before).then_some(after)
    }

    pub fn connection_count(&self) -> usize {
        self.conns.len()
    }

    pub fn user_count(&self) -> usize {
        self.user_conns.len()
    }

    /// How many live connections one user has. Used to decide whether a
    /// connect/disconnect is an actual presence transition or just another tab.
    pub fn conns_for_user(&self, user: Id) -> usize {
        self.user_conns.get(&user).map_or(0, |l| l.read().len())
    }

    /// Connections currently subscribed to a channel. Test/observability hook.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn subscriber_count(&self, channel: Id) -> usize {
        self.channel_subs
            .get(&channel)
            .map_or(0, |s| s.read().len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tc_core::{ErrCode, Id};

    fn frame(msg: &str) -> ServerFrame {
        ServerFrame::Err {
            code: ErrCode::Internal,
            msg: msg.into(),
        }
    }

    #[tokio::test]
    async fn broadcast_reaches_every_subscriber() {
        let hub = Hub::new();
        let ch = Id(1);
        let (a, mut ra) = hub.connect(Id(10));
        let (b, mut rb) = hub.connect(Id(20));
        hub.subscribe(a, &[ch]);
        hub.subscribe(b, &[ch]);

        assert_eq!(hub.broadcast_frame(ch, &frame("hi")), 2);
        assert!(ra.recv().await.is_some());
        assert!(rb.recv().await.is_some());
    }

    #[tokio::test]
    async fn every_subscriber_receives_the_very_same_buffer() {
        // The core claim of this module: one encode, N refcount bumps. If this
        // ever regresses to per-connection encoding, the pointers will differ.
        let hub = Hub::new();
        let ch = Id(1);
        let (a, mut ra) = hub.connect(Id(10));
        let (b, mut rb) = hub.connect(Id(20));
        hub.subscribe(a, &[ch]);
        hub.subscribe(b, &[ch]);

        let before = hub.metrics().frames_encoded.load(Ordering::Relaxed);
        hub.broadcast_frame(ch, &frame("shared"));
        let after = hub.metrics().frames_encoded.load(Ordering::Relaxed);
        assert_eq!(after - before, 1, "a broadcast must encode exactly once");

        let ga = ra.recv().await.unwrap();
        let gb = rb.recv().await.unwrap();
        assert_eq!(ga, gb);
        assert_eq!(
            ga.as_ptr(),
            gb.as_ptr(),
            "subscribers must share one refcounted buffer, not copies"
        );
    }

    #[tokio::test]
    async fn subscribe_is_idempotent_and_delivers_once() {
        let hub = Hub::new();
        let ch = Id(1);
        let (a, mut ra) = hub.connect(Id(10));
        hub.subscribe(a, &[ch]);
        hub.subscribe(a, &[ch]);
        assert_eq!(hub.subscriber_count(ch), 1);
        assert_eq!(hub.broadcast_frame(ch, &frame("x")), 1);
        assert!(ra.recv().await.is_some());
        assert!(ra.try_recv().is_err(), "must not be delivered twice");
    }

    #[tokio::test]
    async fn unsubscribe_and_disconnect_stop_delivery() {
        let hub = Hub::new();
        let ch = Id(1);
        let (a, _ra) = hub.connect(Id(10));
        let (b, _rb) = hub.connect(Id(20));
        hub.subscribe(a, &[ch]);
        hub.subscribe(b, &[ch]);

        hub.unsubscribe(a, &[ch]);
        assert_eq!(hub.broadcast_frame(ch, &frame("x")), 1);

        hub.disconnect(b);
        assert_eq!(hub.broadcast_frame(ch, &frame("x")), 0);
        assert_eq!(hub.subscriber_count(ch), 0);
    }

    #[tokio::test]
    async fn except_skips_the_originating_connection() {
        let hub = Hub::new();
        let ch = Id(1);
        let (a, mut ra) = hub.connect(Id(10));
        let (b, mut rb) = hub.connect(Id(20));
        hub.subscribe(a, &[ch]);
        hub.subscribe(b, &[ch]);

        let payload = hub.encode(&frame("typing"));
        assert_eq!(hub.broadcast(ch, &payload, Some(a)), 1);
        assert!(ra.try_recv().is_err(), "sender should not echo to itself");
        assert!(rb.recv().await.is_some());
    }

    #[tokio::test]
    async fn a_user_with_several_tabs_gets_the_frame_on_each() {
        let hub = Hub::new();
        let user = Id(7);
        let (_, mut r1) = hub.connect(user);
        let (_, mut r2) = hub.connect(user);
        assert_eq!(hub.send_to_user(user, &frame("read")), 2);
        assert!(r1.recv().await.is_some());
        assert!(r2.recv().await.is_some());
    }

    #[tokio::test]
    async fn a_slow_consumer_is_dropped_not_buffered() {
        let hub = Hub::new();
        let ch = Id(1);
        let (slow, _rx_slow) = hub.connect(Id(10));
        let (fast, mut rx_fast) = hub.connect(Id(20));
        hub.subscribe(slow, &[ch]);
        hub.subscribe(fast, &[ch]);

        // Never read from _rx_slow: overflow its queue.
        for _ in 0..(SEND_QUEUE + 20) {
            hub.broadcast_frame(ch, &frame("flood"));
            // Keep the fast consumer drained so only the slow one backs up.
            let _ = rx_fast.try_recv();
        }
        assert_eq!(
            hub.metrics().dropped_slow.load(Ordering::Relaxed),
            1,
            "a slow consumer should be flagged exactly once"
        );
        // The healthy connection kept receiving throughout.
        assert!(hub.broadcast_frame(ch, &frame("still alive")) >= 1);
    }

    #[tokio::test]
    async fn presence_tracks_connections_and_manual_override() {
        let hub = Hub::new();
        let user = Id(7);
        assert_eq!(hub.presence_of(user), Presence::Offline);

        let (c1, _r1) = hub.connect(user);
        let (c2, _r2) = hub.connect(user);
        assert_eq!(hub.presence_of(user), Presence::Online);

        assert_eq!(hub.set_presence(user, Presence::Away), Some(Presence::Away));
        // Setting the same value again is not a change, so no broadcast.
        assert_eq!(hub.set_presence(user, Presence::Away), None);
        assert_eq!(hub.presence_of(user), Presence::Away);

        // Closing one of two tabs does not make the user offline.
        assert!(!hub.disconnect(c1));
        assert_eq!(hub.presence_of(user), Presence::Away);
        assert!(
            hub.disconnect(c2),
            "last connection closing reports offline"
        );
        assert_eq!(hub.presence_of(user), Presence::Offline);
        assert!(hub.online_users().is_empty());
    }

    #[tokio::test]
    async fn subscribing_a_user_covers_every_tab_they_have_open() {
        // Guards a real bug: a user added to a channel by someone else used to
        // receive the channel but none of its messages, because only the tab
        // that performed the action subscribed itself.
        let hub = Hub::new();
        let (user, ch) = (Id(7), Id(1));
        let (_c1, mut r1) = hub.connect(user);
        let (_c2, mut r2) = hub.connect(user);

        hub.subscribe_user(user, ch);
        assert_eq!(hub.subscriber_count(ch), 2);
        assert_eq!(hub.broadcast_frame(ch, &frame("welcome")), 2);
        assert!(r1.recv().await.is_some());
        assert!(r2.recv().await.is_some());

        // Idempotent, and a no-op for someone who is not connected.
        hub.subscribe_user(user, ch);
        assert_eq!(hub.subscriber_count(ch), 2);
        hub.subscribe_user(Id(999), ch);
        assert_eq!(hub.subscriber_count(ch), 2);
    }

    #[tokio::test]
    async fn unsubscribing_a_user_stops_delivery_to_every_tab() {
        // The mirror of the bug above: someone removed from a private channel
        // kept receiving it in every tab, because membership was revoked in the
        // database while the hub's routing table still listed their sockets.
        let hub = Hub::new();
        let (user, other, ch) = (Id(7), Id(8), Id(1));
        let (_c1, mut r1) = hub.connect(user);
        let (_c2, mut r2) = hub.connect(user);
        let (_c3, _r3) = hub.connect(other);
        hub.subscribe_user(user, ch);
        hub.subscribe_user(other, ch);
        assert_eq!(hub.subscriber_count(ch), 3);

        hub.unsubscribe_user(user, ch);
        assert_eq!(hub.subscriber_count(ch), 1, "only the other user remains");
        assert_eq!(hub.broadcast_frame(ch, &frame("after removal")), 1);
        assert!(r1.try_recv().is_err());
        assert!(r2.try_recv().is_err());

        // Idempotent, and harmless for someone who is not connected.
        hub.unsubscribe_user(user, ch);
        hub.unsubscribe_user(Id(999), ch);
        assert_eq!(hub.subscriber_count(ch), 1);
    }

    #[tokio::test]
    async fn broadcasting_to_an_empty_or_unknown_channel_is_a_no_op() {
        let hub = Hub::new();
        assert_eq!(hub.broadcast_frame(Id(999), &frame("void")), 0);
    }

    #[tokio::test]
    async fn disconnect_cleans_up_every_subscription() {
        let hub = Hub::new();
        let (a, _ra) = hub.connect(Id(10));
        let channels: Vec<Id> = (1..=50).map(Id).collect();
        hub.subscribe(a, &channels);
        hub.disconnect(a);
        for ch in &channels {
            assert_eq!(hub.subscriber_count(*ch), 0);
        }
        assert_eq!(hub.connection_count(), 0);
        assert_eq!(hub.user_count(), 0);
    }

    #[tokio::test]
    async fn concurrent_broadcasts_do_not_lose_frames() {
        let hub = Arc::new(Hub::new());
        let ch = Id(1);
        let (a, mut ra) = hub.connect(Id(10));
        hub.subscribe(a, &[ch]);

        let mut tasks = Vec::new();
        for i in 0..8 {
            let hub = hub.clone();
            tasks.push(tokio::spawn(async move {
                for j in 0..10 {
                    hub.broadcast_frame(ch, &frame(&format!("{i}-{j}")));
                }
            }));
        }
        // Drain concurrently so the queue never fills.
        let drain = tokio::spawn(async move {
            let mut n = 0;
            while ra.recv().await.is_some() {
                n += 1;
                if n == 80 {
                    break;
                }
            }
            n
        });
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(drain.await.unwrap(), 80);
    }
}
