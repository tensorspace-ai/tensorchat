/**
 * Client-side state.
 *
 * One rule governs the shape here: **the server's ordering is the truth, and
 * the client folds deltas into it.** Every mutation is either a server frame
 * being applied or an optimistic local echo that a later frame reconciles. The
 * UI never writes state directly.
 *
 * Two storage strategies, chosen by size:
 *
 * * Small collections (users, channels, read state) live in signals holding
 *   `Map`s that are replaced wholesale. At tens-to-hundreds of entries, copying
 *   the map is cheaper than the bookkeeping to avoid it.
 * * Message logs are large and append-heavy, so they are mutated in place and
 *   paired with a version counter. Readers subscribe to the counter; nothing
 *   ever copies a channel's history.
 */

import { batch, signal, type Signal } from './signals.ts';
import { idCompare } from './protocol.ts';
import type {
  Channel,
  Id,
  Message,
  Presence,
  ReadState,
  ServerFrame,
  User,
} from './protocol.ts';
import type { ConnectionState } from './ws.ts';

/** A channel's loaded history, newest last. */
export type ChannelLog = {
  /** Ascending by id. Mutated in place; watch `version` for changes. */
  messages: Message[];
  /** id -> index in `messages`, so an edit or reaction is a lookup, not a scan. */
  index: Map<Id, number>;
  /** Cursor for the next older page; null once fully loaded. */
  cursor: Id | null;
  /** True once history has been read back to the channel's first message. */
  complete: boolean;
  loading: boolean;
  /** Bumped on any mutation. */
  version: Signal<number>;
};

/** A message the user sent that the server has not acknowledged yet. */
export type PendingMessage = {
  nonce: number;
  channel: Id;
  body: string;
  threadRoot: Id | null;
  at: number;
  failed: boolean;
};

/** Shared empty set, so `pinsIn` on an unopened channel allocates nothing. */
const EMPTY_PINS: ReadonlySet<Id> = new Set<Id>();

function newLog(): ChannelLog {
  return {
    messages: [],
    index: new Map(),
    cursor: null,
    complete: false,
    loading: false,
    version: signal(0),
  };
}

export class Store {
  me = signal<User | null>(null);
  users = signal<Map<Id, User>>(new Map());
  channels = signal<Map<Id, Channel>>(new Map());
  readStates = signal<Map<Id, ReadState>>(new Map());
  presence = signal<Map<Id, Presence>>(new Map());

  /**
   * Pinned message ids, per channel.
   *
   * Only ids: the bodies are already in the message log, and a channel's pins
   * are fetched whole when it is opened rather than joined onto every history
   * page — that keeps pin lookups off the hottest read in the product.
   */
  pins = signal<Map<Id, Set<Id>>>(new Map());

  /**
   * The most recent membership delta, so open views of a channel's roster
   * update when someone is added or removed rather than going stale until the
   * pane is reopened. Null until the first one arrives.
   */
  memberChange = signal<{ ch: Id; u: Id; j: boolean } | null>(null);

  /** Currently open channel, or null on the empty state. */
  currentChannel = signal<Id | null>(null);
  /** Open thread's root message id, or null when the thread pane is closed. */
  openThread = signal<Id | null>(null);

  connection = signal<ConnectionState>('connecting');

  /**
   * Who is typing where: channel -> (user -> expiry timestamp).
   *
   * Expiry is stored rather than scheduling a timer per keystroke; readers
   * filter on read and one shared interval bumps the signal.
   */
  typing = signal<Map<Id, Map<Id, number>>>(new Map());

  /** Unacknowledged local sends, keyed by nonce. */
  pending = signal<Map<number, PendingMessage>>(new Map());

  private logs = new Map<Id, ChannelLog>();

  // -- Accessors ----------------------------------------------------------

  log(channel: Id): ChannelLog {
    let l = this.logs.get(channel);
    if (!l) {
      l = newLog();
      this.logs.set(channel, l);
    }
    return l;
  }

  user(id: Id): User | undefined {
    return this.users().get(id);
  }

  /** Display name for a user id, falling back to something renderable. */
  userName(id: Id): string {
    const u = this.users().get(id);
    return u ? u.n || u.h : 'unknown';
  }

  presenceOf(id: Id): Presence {
    return this.presence().get(id) ?? 'offline';
  }

  /**
   * A channel's display title. DMs and group DMs have no name of their own —
   * they are titled by whoever else is in them.
   */
  channelTitle(c: Channel): string {
    if (c.k === 'public' || c.k === 'private') return c.n ?? '';
    const meId = this.me()?.id;
    const others = (c.m ?? []).filter((id) => id !== meId);
    if (others.length === 0) return 'You';
    return others.map((id) => this.userName(id)).join(', ');
  }

  unread(channel: Id): ReadState | undefined {
    return this.readStates().get(channel);
  }

  /** The pinned ids in a channel. Empty until the channel has been opened. */
  pinsIn(channel: Id): ReadonlySet<Id> {
    return this.pins().get(channel) ?? EMPTY_PINS;
  }

  isPinned(channel: Id, message: Id): boolean {
    return this.pins().get(channel)?.has(message) ?? false;
  }

  /** Replace a channel's pin set wholesale, from a fetch. */
  setPins(channel: Id, ids: Id[]): void {
    this.pins.update((prev) => new Map(prev).set(channel, new Set(ids)));
  }

  /** Fold one pin delta in, from a `pin` frame or an optimistic toggle. */
  setPinned(channel: Id, message: Id, on: boolean): void {
    this.pins.update((prev) => {
      const current = prev.get(channel);
      if (on ? current?.has(message) : !current?.has(message)) return prev;
      const next = new Set(current ?? []);
      if (on) next.add(message);
      else next.delete(message);
      return new Map(prev).set(channel, next);
    });
  }

  typingIn(channel: Id): Id[] {
    const now = Date.now();
    const forChannel = this.typing().get(channel);
    if (!forChannel) return [];
    const meId = this.me()?.id;
    const out: Id[] = [];
    for (const [user, expires] of forChannel) {
      if (expires > now && user !== meId) out.push(user);
    }
    return out;
  }

  // -- Message log mutation ----------------------------------------------

  /**
   * Insert a message, keeping the log ordered and free of duplicates.
   *
   * The common case is an append (a new message has the largest id), so that is
   * checked first; out-of-order arrivals fall back to a binary search.
   */
  addMessage(m: Message, bump = true): void {
    const l = this.log(m.ch);
    const existing = l.index.get(m.id);
    if (existing !== undefined) {
      l.messages[existing] = m;
      if (bump) l.version.update((v) => v + 1);
      return;
    }

    const last = l.messages[l.messages.length - 1];
    if (!last || idCompare(m.id, last.id) > 0) {
      l.index.set(m.id, l.messages.length);
      l.messages.push(m);
    } else {
      const at = lowerBound(l.messages, m.id);
      l.messages.splice(at, 0, m);
      // Indices after the insertion point all shifted by one.
      for (let i = at; i < l.messages.length; i++) l.index.set(l.messages[i].id, i);
    }
    if (bump) l.version.update((v) => v + 1);
  }

  /** Prepend an older page. Input may be in any order. */
  prependPage(channel: Id, page: Message[], cursor: Id | null): void {
    const l = this.log(channel);
    const fresh = page.filter((m) => !l.index.has(m.id));
    if (fresh.length) {
      fresh.sort((a, b) => idCompare(a.id, b.id));
      l.messages = fresh.concat(l.messages);
      l.index.clear();
      for (let i = 0; i < l.messages.length; i++) l.index.set(l.messages[i].id, i);
    }
    l.cursor = cursor;
    l.complete = cursor === null;
    l.loading = false;
    l.version.update((v) => v + 1);
  }

  private patchMessage(channel: Id, id: Id, patch: (m: Message) => Message): void {
    const l = this.log(channel);
    const at = l.index.get(id);
    if (at === undefined) return;
    l.messages[at] = patch(l.messages[at]);
    l.version.update((v) => v + 1);
  }

  // -- Frame application --------------------------------------------------

  /**
   * Fold one server frame into state.
   *
   * Wrapped in `batch` so a frame that touches several signals — a message that
   * also moves a channel's `last` pointer and its unread count — triggers a
   * single render pass rather than three.
   */
  apply(frame: ServerFrame): void {
    batch(() => this.applyInner(frame));
  }

  private applyInner(frame: ServerFrame): void {
    if ('ready' in frame) {
      const r = frame.ready;
      this.me.set(r.me);
      this.users.set(new Map(r.us.map((u) => [u.id, u])));
      this.channels.set(new Map(r.ch.map((c) => [c.id, c])));
      this.readStates.set(new Map(r.rs.map((s) => [s.ch, s])));
      this.presence.set(new Map(r.on.map((id) => [id, 'online' as Presence])));
      return;
    }

    if ('msg' in frame) {
      const m = frame.msg.m;
      this.addMessage(m);
      this.bumpChannelLast(m.ch, m.id);

      // Reconcile an optimistic echo: same author, same channel, same text.
      const meId = this.me()?.id;
      if (m.au === meId) this.clearMatchingPending(m);

      // Unread accounting is local so the badge updates without a round trip.
      // Your own messages never count, and neither does the channel you are
      // looking at.
      if (m.au !== meId && this.currentChannel() !== m.ch) {
        const mentionsMe = !!(meId && m.mn?.includes(meId));
        this.readStates.update((prev) => {
          const next = new Map(prev);
          const cur = next.get(m.ch) ?? { ch: m.ch, lr: '0', u: 0, mn: 0 };
          next.set(m.ch, {
            ...cur,
            u: cur.u + 1,
            mn: cur.mn + (mentionsMe ? 1 : 0),
          });
          return next;
        });
      }
      return;
    }

    if ('msg_edit' in frame) {
      const e = frame.msg_edit;
      this.patchMessage(e.ch, e.id, (m) => ({ ...m, b: e.b, ed: e.ed }));
      return;
    }

    if ('msg_del' in frame) {
      const d = frame.msg_del;
      this.patchMessage(d.ch, d.id, (m) => ({ ...m, b: '', del: true, at: [], rx: [] }));
      return;
    }

    if ('react' in frame) {
      const r = frame.react;
      const meId = this.me()?.id;
      // Deltas, not totals: the server cannot send a per-viewer `me` flag on a
      // broadcast frame, so each client folds the change itself.
      this.patchMessage(r.ch, r.id, (m) => {
        const rx = (m.rx ?? []).map((x) => ({ ...x }));
        const at = rx.findIndex((x) => x.e === r.e);
        if (r.on) {
          if (at === -1) rx.push({ e: r.e, c: 1, me: r.u === meId });
          else {
            rx[at].c += 1;
            if (r.u === meId) rx[at].me = true;
          }
        } else if (at !== -1) {
          rx[at].c -= 1;
          if (r.u === meId) rx[at].me = false;
          if (rx[at].c <= 0) rx.splice(at, 1);
        }
        return { ...m, rx };
      });
      return;
    }

    if ('typing' in frame) {
      const t = frame.typing;
      this.typing.update((prev) => {
        const next = new Map(prev);
        const forChannel = new Map(next.get(t.ch) ?? []);
        // Indicators expire on their own; no per-keystroke timer needed.
        forChannel.set(t.u, Date.now() + 5000);
        next.set(t.ch, forChannel);
        return next;
      });
      return;
    }

    if ('pin' in frame) {
      const p = frame.pin;
      this.setPinned(p.ch, p.id, p.on);
      return;
    }

    if ('presence' in frame) {
      const p = frame.presence;
      this.presence.update((prev) => new Map(prev).set(p.u, p.p));
      return;
    }

    if ('read' in frame) {
      const rs = frame.read.rs;
      this.readStates.update((prev) => new Map(prev).set(rs.ch, rs));
      return;
    }

    if ('chan' in frame) {
      const c = frame.chan.c;
      this.channels.update((prev) => new Map(prev).set(c.id, c));
      return;
    }

    if ('member' in frame) {
      const mem = frame.member;
      const meId = this.me()?.id;
      if (mem.u === meId && !mem.j) {
        // We left: drop the channel and its history.
        this.channels.update((prev) => {
          const next = new Map(prev);
          next.delete(mem.ch);
          return next;
        });
        this.logs.delete(mem.ch);
        if (this.currentChannel() === mem.ch) this.currentChannel.set(null);
      }
      // Direct conversations carry their roster on the channel itself, so keep
      // that copy in step; named channels are fetched on demand and watch
      // `memberChange` instead.
      const channel = this.channels().get(mem.ch);
      if (channel?.m) {
        const next = mem.j
          ? channel.m.includes(mem.u)
            ? channel.m
            : [...channel.m, mem.u]
          : channel.m.filter((id) => id !== mem.u);
        if (next !== channel.m) {
          this.channels.update((prev) => new Map(prev).set(mem.ch, { ...channel, m: next }));
        }
      }
      this.memberChange.set({ ch: mem.ch, u: mem.u, j: mem.j });
      return;
    }

    if ('user_upd' in frame) {
      const u = frame.user_upd.u;
      this.users.update((prev) => new Map(prev).set(u.id, u));
      return;
    }

    if ('ack' in frame) {
      // The message itself arrives as a separate `msg` frame; the ack only
      // confirms the nonce, so clear the placeholder here.
      const nonce = frame.ack.n;
      this.pending.update((prev) => {
        if (!prev.has(nonce)) return prev;
        const next = new Map(prev);
        next.delete(nonce);
        return next;
      });
      return;
    }

    if ('err' in frame) {
      console.warn('server error', frame.err.c, frame.err.m);
      // A rejected send should stop looking like it is still in flight.
      if (frame.err.c === 'rate_limited' || frame.err.c === 'forbidden') {
        this.pending.update((prev) => {
          const next = new Map(prev);
          for (const [k, v] of next) next.set(k, { ...v, failed: true });
          return next;
        });
      }
    }
  }

  private bumpChannelLast(channel: Id, message: Id): void {
    this.channels.update((prev) => {
      const c = prev.get(channel);
      if (!c || (c.last && idCompare(c.last, message) >= 0)) return prev;
      return new Map(prev).set(channel, { ...c, last: message });
    });
  }

  /** Drop the optimistic placeholder that this server message fulfils. */
  private clearMatchingPending(m: Message): void {
    this.pending.update((prev) => {
      let hit: number | undefined;
      for (const [nonce, p] of prev) {
        if (p.channel === m.ch && p.body === m.b) {
          hit = nonce;
          break;
        }
      }
      if (hit === undefined) return prev;
      const next = new Map(prev);
      next.delete(hit);
      return next;
    });
  }

  // -- Local (optimistic) mutations ---------------------------------------

  addPending(p: PendingMessage): void {
    this.pending.update((prev) => new Map(prev).set(p.nonce, p));
  }

  /** Clear a channel's unread badge locally, ahead of the server's echo. */
  clearUnread(channel: Id): void {
    this.readStates.update((prev) => {
      const cur = prev.get(channel);
      if (!cur || (cur.u === 0 && cur.mn === 0)) return prev;
      return new Map(prev).set(channel, { ...cur, u: 0, mn: 0 });
    });
  }

  /** Total unread mentions, for the document title badge. */
  totalMentions(): number {
    let n = 0;
    for (const s of this.readStates().values()) n += s.mn;
    return n;
  }

  /** Channels sorted for the sidebar: named channels first, then DMs. */
  sortedChannels(): Channel[] {
    const all = [...this.channels().values()].filter((c) => !c.arc);
    const named = all.filter((c) => c.k === 'public' || c.k === 'private');
    const direct = all.filter((c) => c.k === 'dm' || c.k === 'group');
    named.sort((a, b) => (a.n ?? '').localeCompare(b.n ?? ''));
    // Most recently active DM first — that is how people look for them.
    direct.sort((a, b) => idCompare(b.last ?? '0', a.last ?? '0'));
    return [...named, ...direct];
  }
}

/** First index whose message id is >= `id`. */
function lowerBound(messages: Message[], id: Id): number {
  let lo = 0;
  let hi = messages.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (idCompare(messages[mid].id, id) < 0) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

export const store = new Store();
