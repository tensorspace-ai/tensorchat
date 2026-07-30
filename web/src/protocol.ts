/**
 * The wire protocol, mirrored from `crates/tc-core/src/proto.rs`.
 *
 * Field names are the short forms the Rust side serializes (`ch`, `au`, `b`).
 * They are terse because every key is repeated on every frame; the types here
 * give them names again so nothing downstream has to remember what `au` means.
 *
 * ## Why every id is a `string`
 *
 * Ids are u64 Snowflakes that exceed `Number.MAX_SAFE_INTEGER`. Our MessagePack
 * decoder returns oversized integers as exact decimal strings rather than lossy
 * numbers, and the REST API encodes them as JSON strings for the same reason.
 * So ids are strings end to end — safe as object keys, safe with `===`, and
 * never silently rounded. The Rust decoder accepts either form on the way back.
 */

export type Id = string;

export type Presence = 'online' | 'away' | 'offline';
export type ChannelKind = 'public' | 'private' | 'dm' | 'group';

export type User = {
  id: Id;
  h: string; // handle
  n: string; // display name
  st?: string; // status text
  bot?: boolean;
  d?: boolean; // deactivated
  adm?: boolean; // workspace administrator
};

export type Channel = {
  id: Id;
  k: ChannelKind;
  n?: string; // name; absent for dm/group
  t?: string; // topic
  cb: Id; // created by
  arc?: boolean; // archived
  m?: Id[]; // members, populated for dm/group only
  last?: Id; // newest message id
};

export type Attachment = {
  id: Id;
  n: string;
  mt: string; // mime type
  sz: number;
  w?: number;
  hh?: number;
};

export type Reaction = {
  e: string; // emoji
  c: number; // count
  me?: boolean;
};

export type Message = {
  id: Id;
  ch: Id;
  au: Id; // author
  b: string; // body
  th?: Id; // thread root
  rc?: number; // reply count
  ed?: number; // edited at (ms)
  del?: boolean;
  at?: Attachment[];
  rx?: Reaction[];
  mn?: Id[]; // mentioned user ids
};

export type ReadState = {
  ch: Id;
  lr: Id; // last read
  u: number; // unread
  mn: number; // unread mentions
  mu?: boolean; // muted — suppress the unread badge, but not mentions
};

export type SearchHit = { m: Message; sn: string };

export type ApiToken = {
  id: Id;
  label: string;
  created_at: number;
  last_used?: number | null;
  /** Only present in the response that created it. */
  secret?: string;
};

export type Invite = {
  id: Id;
  label: string;
  created_at: number;
  /** Null never expires. */
  expires_at?: number | null;
  /** Zero is unlimited. */
  max_uses: number;
  uses: number;
  /** Whether it would still be accepted. Computed server-side. */
  live: boolean;
  /** Only present in the response that created it. */
  token?: string;
};

/**
 * Sentinels wrapping matched terms in a search snippet.
 *
 * Control characters rather than HTML tags: the client escapes the snippet
 * text first and only then converts sentinels into elements, so a message body
 * can never inject markup into its own search result.
 */
export const HL_START = '\u0002';
export const HL_END = '\u0003';

export type ErrCode =
  | 'unauthorized'
  | 'forbidden'
  | 'not_found'
  | 'bad_request'
  | 'rate_limited'
  | 'overloaded'
  | 'internal';

/**
 * Frames the server sends. Externally tagged: exactly one key, naming the
 * variant.
 */
export type ServerFrame =
  | { ready: { me: User; ch: Channel[]; us: User[]; rs: ReadState[]; on: Id[]; v: number } }
  | { ack: { n: number; id: Id } }
  | { msg: { m: Message } }
  | { msg_edit: { id: Id; ch: Id; b: string; ed: number } }
  | { msg_del: { id: Id; ch: Id } }
  | { react: { id: Id; ch: Id; e: string; u: Id; on: boolean } }
  | { pin: { id: Id; ch: Id; by: Id; on: boolean } }
  | { saved: { id: Id; on: boolean } }
  | { typing: { ch: Id; u: Id } }
  | { presence: { u: Id; p: Presence } }
  | { read: { rs: ReadState } }
  | { chan: { c: Channel } }
  | { member: { ch: Id; u: Id; j: boolean } }
  | { user_upd: { u: User } }
  | { pong: { t: number } }
  | { err: { c: ErrCode; m: string } };

/** Frames the client sends. */
export type ClientFrame =
  | { hello: { tk: string; v: number } }
  | { sub: { ch: Id[] } }
  | { unsub: { ch: Id[] } }
  | { send: { n: number; ch: Id; b: string; th?: Id | null; at?: Id[] } }
  | { edit: { id: Id; b: string } }
  | { del: { id: Id } }
  | { react: { id: Id; e: string; on: boolean } }
  | { typing: { ch: Id } }
  | { read: { ch: Id; up: Id } }
  | { presence: { p: Presence } }
  | { ping: { t: number } };

export const PROTOCOL_VERSION = 1;

/** Custom epoch of the Snowflake id scheme (2024-01-01T00:00:00Z). */
const TC_EPOCH_MS = 1704067200000n;

/**
 * Recover the creation time of an entity from its id.
 *
 * Ids embed their own timestamp, so message times need no separate field on the
 * wire — this is where those bytes were saved.
 */
export function idToDate(id: Id): Date {
  try {
    return new Date(Number((BigInt(id) >> 22n) + TC_EPOCH_MS));
  } catch {
    return new Date(0);
  }
}

/** Compare two ids chronologically. Ids are decimal strings of differing length,
 *  so a lexicographic compare would be wrong; compare by length first. */
export function idCompare(a: Id, b: Id): number {
  if (a.length !== b.length) return a.length - b.length;
  return a < b ? -1 : a > b ? 1 : 0;
}

/** Normalize whatever the decoder produced into a string id. */
export function asId(v: unknown): Id {
  return typeof v === 'string' ? v : String(v);
}
