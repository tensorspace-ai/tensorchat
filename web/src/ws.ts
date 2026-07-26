/**
 * The realtime connection.
 *
 * Responsibilities, in order of how much they matter:
 *
 * 1. Reconnect reliably. Networks drop; a chat client that needs a manual
 *    refresh after a tunnel or a laptop lid is broken. Backoff is exponential
 *    with jitter, and resets on a successful session.
 * 2. Resynchronize after a gap. Events that occurred while disconnected are
 *    gone — the server does not replay. On reconnect we refetch, rather than
 *    pretending the local state is still correct.
 * 3. Stay off the main thread's critical path. Frames are binary MessagePack,
 *    decoded in one pass, and handed straight to the store.
 */

import { decode, encode } from './msgpack.ts';
import type { ClientFrame, Id, Presence, ServerFrame } from './protocol.ts';

export type ConnectionState = 'connecting' | 'open' | 'offline';

type Handlers = {
  onFrame: (frame: ServerFrame) => void;
  onState: (state: ConnectionState) => void;
  /** Fired after a reconnect that lost events, so callers can refetch. */
  onResync: () => void;
};

/** Reconnect backoff bounds. */
const BASE_DELAY_MS = 500;
const MAX_DELAY_MS = 15000;
/** Ping cadence. The server also pings; this measures round-trip time. */
const PING_INTERVAL_MS = 25000;

export class Connection {
  private url: string;
  private handlers: Handlers;
  private socket: WebSocket | null = null;
  private attempt = 0;
  private closed = false;
  private everConnected = false;
  private reconnectTimer: number | undefined;
  private pingTimer: number | undefined;
  private nextNonce = 1;
  /** Frames queued while the socket is down, replayed on reconnect. */
  private outbox: ClientFrame[] = [];

  /** Round-trip time of the last ping, in milliseconds. */
  rtt = 0;

  constructor(handlers: Handlers) {
    this.handlers = handlers;
    const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
    // No token in the URL: the session cookie authenticates the handshake, and
    // a token in a query string ends up in proxy logs and browser history.
    this.url = `${scheme}//${location.host}/ws`;
  }

  connect(): void {
    if (this.closed) return;
    this.clearTimers();
    this.handlers.onState('connecting');

    let socket: WebSocket;
    try {
      socket = new WebSocket(this.url);
    } catch {
      this.scheduleReconnect();
      return;
    }
    socket.binaryType = 'arraybuffer';
    this.socket = socket;

    socket.onopen = () => {
      const reconnected = this.everConnected;
      this.everConnected = true;
      this.attempt = 0;
      this.handlers.onState('open');

      // Flush anything typed while offline.
      const pending = this.outbox;
      this.outbox = [];
      for (const frame of pending) this.rawSend(frame);

      this.startPing();
      // A reconnect means we may have missed events; the caller refetches.
      if (reconnected) this.handlers.onResync();
    };

    socket.onmessage = (ev: MessageEvent) => {
      if (!(ev.data instanceof ArrayBuffer)) return;
      let frame: ServerFrame;
      try {
        frame = decode(new Uint8Array(ev.data)) as ServerFrame;
      } catch (err) {
        // A frame we cannot parse is a protocol mismatch, not a reason to drop
        // the connection — log it and keep going.
        console.warn('undecodable frame', err);
        return;
      }
      if ('pong' in frame) {
        this.rtt = Date.now() - frame.pong.t;
        return;
      }
      this.handlers.onFrame(frame);
    };

    socket.onclose = () => {
      this.socket = null;
      this.clearTimers();
      if (!this.closed) {
        this.handlers.onState('offline');
        this.scheduleReconnect();
      }
    };

    socket.onerror = () => {
      // `onclose` always follows, and handles the retry. Closing here as well
      // would double-schedule the reconnect.
    };
  }

  /** Close permanently. Used on logout. */
  close(): void {
    this.closed = true;
    this.clearTimers();
    this.socket?.close();
    this.socket = null;
  }

  private scheduleReconnect(): void {
    // Exponential backoff with full jitter. Jitter matters: without it, every
    // client disconnected by one server restart reconnects in lockstep and
    // knocks it over again.
    const ceiling = Math.min(MAX_DELAY_MS, BASE_DELAY_MS * 2 ** this.attempt);
    const delay = Math.random() * ceiling;
    this.attempt = Math.min(this.attempt + 1, 10);
    this.reconnectTimer = setTimeout(() => this.connect(), delay) as unknown as number;
  }

  private startPing(): void {
    this.pingTimer = setInterval(() => {
      this.rawSend({ ping: { t: Date.now() } });
    }, PING_INTERVAL_MS) as unknown as number;
  }

  private clearTimers(): void {
    if (this.reconnectTimer !== undefined) clearTimeout(this.reconnectTimer);
    if (this.pingTimer !== undefined) clearInterval(this.pingTimer);
    this.reconnectTimer = undefined;
    this.pingTimer = undefined;
  }

  private rawSend(frame: ClientFrame): boolean {
    const s = this.socket;
    if (!s || s.readyState !== WebSocket.OPEN) return false;
    try {
      s.send(encode(frame));
      return true;
    } catch {
      return false;
    }
  }

  /**
   * Send a frame, queueing it if the socket is down.
   *
   * `queue` is false for frames that are only meaningful right now — a typing
   * indicator delivered thirty seconds late is worse than none at all.
   */
  send(frame: ClientFrame, queue = true): boolean {
    if (this.rawSend(frame)) return true;
    if (queue) {
      // Bound the outbox: a long offline period should not accumulate forever.
      if (this.outbox.length < 200) this.outbox.push(frame);
    }
    return false;
  }

  /** Allocate a nonce for correlating an optimistic send with its ack. */
  allocNonce(): number {
    return this.nextNonce++;
  }

  // -- Typed conveniences -------------------------------------------------

  sendMessage(channel: Id, body: string, threadRoot?: Id | null, attachments: Id[] = []): number {
    const n = this.allocNonce();
    this.send({ send: { n, ch: channel, b: body, th: threadRoot ?? null, at: attachments } });
    return n;
  }

  editMessage(id: Id, body: string): void {
    this.send({ edit: { id, b: body } });
  }

  deleteMessage(id: Id): void {
    this.send({ del: { id } });
  }

  react(id: Id, emoji: string, on: boolean): void {
    this.send({ react: { id, e: emoji, on } });
  }

  typing(channel: Id): void {
    // Ephemeral: never queued.
    this.send({ typing: { ch: channel } }, false);
  }

  markRead(channel: Id, upTo: Id): void {
    this.send({ read: { ch: channel, up: upTo } });
  }

  setPresence(p: Presence): void {
    this.send({ presence: { p } });
  }

  subscribe(channels: Id[]): void {
    if (channels.length) this.send({ sub: { ch: channels } });
  }
}
