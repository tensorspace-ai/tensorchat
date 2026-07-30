# Architecture

Why TensorChat is built the way it is. Each section states a decision, the
reasoning behind it, and what it costs.

---

## 1. Identity: Snowflake IDs

Every entity is keyed by a time-sortable 64-bit integer:

```
 63           22 21    12 11         0
+---------------+--------+-----------+
| ms since epoch| node   | sequence  |
|   42 bits     | 10 bits|  12 bits  |
+---------------+--------+-----------+
```

The alternative was UUIDv4. IDs are the primary key and the pagination cursor
for `messages`, the hottest table in the system, and that makes the choice
consequential:

- **Insertion stays at the right edge of the B-tree.** Random UUIDs scatter
  inserts across the index, splitting pages and dirtying many more of them per
  write.
- **`ORDER BY id` is free.** Chronological order is index order, so history
  needs no sort and no secondary index on a timestamp.
- **The ID *is* the timestamp.** `id >> 22` recovers creation time, so messages
  carry no separate time field on the wire or on disk.
- **A time range becomes a key range.** `Id::floor_for_ms` turns "since
  yesterday" into a primary-key scan, with no date functions.
- **8 bytes instead of 16.**

The cost is coordination: the 10-bit node field must be unique per writer, which
is what `TC_NODE_ID` is for. Monotonicity is enforced with a single CAS loop over
one `AtomicU64` holding both timestamp and sequence — no mutex, so many
connection tasks can mint IDs concurrently without a contention cliff.

**One consequence reaches all the way to the browser.** These IDs exceed
2^53, so a JSON number would be silently rounded by any JavaScript client. IDs
therefore serialize as *strings* in JSON and as native u64 in MessagePack, and
the browser's decoder returns oversized integers as exact decimal strings. Ids
are strings end to end on the client — safe as object keys, safe with `===`,
never rounded. A unit test asserts both encodings, because this is the kind of
property that is easy to break and hard to notice.

---

## 2. Storage: SQLite in process

A chat server's working set is "the last few hundred messages in the channels I
have open". That is a page-cache problem, not a distributed-systems problem.

Running the database in-process removes a network hop, a connection pool's worth
of context switches, and a serialization round trip from every read. A local
SQLite query costs microseconds where a Postgres round trip costs hundreds. In
WAL mode readers never block the writer and the writer never blocks readers —
exactly the concurrency shape of chat.

### Schema decisions

- **`STRICT` on every table.** SQLite's default affinity will happily store a
  string in an integer column; strict tables make that an error at write time
  rather than a mystery at read time.
- **`WITHOUT ROWID` on junction tables.** For `members`, `reactions`, `mentions`
  and `sessions`, the row *is* the key, so the rowid indirection is pure
  overhead.
- **`members.last_read` lives with membership.** The read cursor is 1:1 with
  membership, so co-locating it makes "my channels and where I am in each" one
  index scan rather than a join.
- **`channels.last_message` is denormalized.** Sorting the sidebar would
  otherwise be a correlated subquery over `messages` per channel.
- **`messages.mentions` is a packed blob *and* a `mentions` table.** The blob
  serves reading (render history without a join); the table serves counting (an
  unread-mention badge is a counted range scan from the read cursor). Each
  direction gets the layout that suits it.
- **Unread counts are capped at 100 rows** via `LIMIT` inside the count. The UI
  renders anything higher as "99+", so counting further is work nobody sees, and
  it bounds the cost of a badge refresh on a huge backlog.
- **Search is FTS5 with external content.** The index stores postings only and
  reads bodies back from `messages`, roughly halving search's disk cost.
  Triggers keep it in sync; a soft delete blanks the body, which correctly
  removes the row from search.

### The `IMMEDIATE` transaction rule

Every write path opens its transaction as `IMMEDIATE` rather than the default
`DEFERRED`. This is not a micro-optimization — it is a correctness requirement,
and it was found by a load test rather than by reading.

`busy_timeout` does **not** apply to a deferred transaction that reads first and
then tries to upgrade to a write. Such a transaction already holds a read
snapshot that a competing writer may have invalidated, so SQLite has no choice
but to fail it immediately with `SQLITE_BUSY`; waiting could deadlock. The
symptom was ordinary concurrent message sends failing under load with "database
is locked".

`IMMEDIATE` takes the write lock up front, so contending writers queue on the
timeout instead of erroring. `crates/tensorchat-store/tests/concurrency.rs` locks this
in against a real file-backed database, and fails if the behavior is reverted.

### Blocking

`tensorchat-store` is entirely synchronous. `AppState::db` is the single place it is
called from, wrapping each operation in `spawn_blocking`, so "never block a
reactor thread" is enforced in one location rather than remembered at every call
site.

---

## 3. Realtime: one encode per event

This is the design's center.

When a message lands in a channel with N connected members, the naive
implementation serializes it N times — once per socket — doing identical work
each time. Serialization is the most expensive step in the delivery path.

Instead the hub encodes each event **once** into a `Bytes` buffer and hands
every subscriber a clone of that handle. `Bytes` is refcounted, so a clone is an
atomic increment, not a copy. Broadcasting to 10,000 sockets costs one
`rmp_serde` pass and 10,000 pointer bumps. The buffer stays refcounted all the
way to the socket write — nothing copies it again.

**This is only sound because broadcast frames are viewer-independent.** That is
a protocol invariant, not an implementation detail, and it shapes the wire
format:

- A reaction broadcast is a **per-user delta** (`user X added 👍`), never an
  aggregate — an aggregate would need a per-viewer "did I react?" flag. Clients
  fold deltas into their own counts.
- Acks carry a client's nonce and go to that connection only.
- Read receipts go to one user's connections.

`proto.rs` states the rule where a future contributor will read it: *if two
users would receive different bytes, it does not belong on a broadcast frame.*

### Backpressure

Each connection owns a bounded queue (256 frames). Sends are non-blocking: if a
queue is full, that connection is evicted and its client reconnects and refetches.

The alternative — blocking the broadcaster — would let one suspended laptop
stall delivery for everyone else in the channel. Dropping one slow consumer is
strictly better than degrading the channel. `dropped_slow` is exported in
`/api/metrics` so this is visible rather than silent.

### One task pair per connection

A reader task and a writer task. The split is what makes backpressure work: the
writer owns the socket's send half and drains the hub queue, so a client that
stops reading stalls only its own writer while the reader keeps processing. A
single task would have to interleave both, and a blocked send would stop the
connection responding at all.

### Subscription is server-side

When a user gains access to a channel — creates it, joins it, is added to it,
opens a DM — the server subscribes all of that user's live connections.

This was originally left to the client, which called `subscribe` after creating
a channel through the UI. Every other path was broken: a channel created from
another tab, by a bot, or by someone else adding you would arrive as a `Chan`
frame with no subscription behind it, and you would receive the channel but none
of its messages until you reloaded. A browser test found it. Doing it
server-side makes it impossible for a client to forget.

---

## 4. Wire format

MessagePack with **named** fields, not positional.

Positional encoding is more compact, but it couples Rust field *order* to the
TypeScript decoder with no shared schema — a silent-corruption footgun for a few
bytes. Named encoding keeps both ends self-describing; field names are shortened
to one or two characters (`ch`, `au`, `b`) so framing overhead stays negligible
next to message text, and empty collections and false flags are elided
entirely. A plain chat message frame is under 100 bytes.

The REST API uses JSON, because a login does not need those bytes back and JSON
is what a person debugging with `curl` expects.

---

## 5. Client: no framework

The client is TypeScript with no runtime dependencies. 52 kB of JavaScript,
minified, for the entire application.

### Reactivity

`signals.ts` is a fine-grained reactive core: signals, computeds, and effects
with automatic dependency tracking, glitch-free propagation, and batching.
Roughly 400 lines. Writes mark downstream computeds dirty without recomputing
them (recomputation stays lazy) and collect affected effects into a deduplicated
flush queue, so a diamond dependency runs its effect once.

### Two update strategies, chosen by size

- **Small collections** (users, channels, read state) live in signals holding
  `Map`s that are replaced wholesale. At tens-to-hundreds of entries, copying
  the map is cheaper than the bookkeeping to avoid it.
- **Message logs** are mutated in place and paired with a version counter.
  Readers subscribe to the counter; a channel's history is never copied.

The sidebar is rebuilt wholesale on change — tens of rows, well under a frame,
and diffing would buy nothing but bugs. The message list gets the opposite
treatment.

### The virtual list

`virtual-list.ts` mounts only the visible window plus overscan, recycles row
elements through a keyed pool, and locates the visible slice with a **binary
search over prefix-summed row offsets** rather than a linear scan. Variable row
heights are measured after mount and folded back into the offsets.

Three behaviors matter more than raw speed, all of them chat-specific:

1. **Anchored prepends.** Loading older history inserts content *above* the
   viewport. The scroll offset is restored against a stable anchor row so what
   the reader is looking at does not jump. This is the single most important
   behavior in the component.
2. **Pinning.** New messages auto-scroll only if the reader was already at the
   bottom.
3. **Height-change compensation.** When a row above the viewport changes height
   (an image finishes loading), `scrollTop` is adjusted so visible content stays
   put.

The CSS is written against this: rows are absolutely positioned by JS, so the
stylesheet never sets `position`, `height`, or vertical margins on a row, and
uses padding for spacing — collapsing margins would corrupt height measurement.

### Rendering is DOM nodes, never HTML

There is no `innerHTML` in the codebase. Message bodies, search snippets, and
every piece of server data become text nodes and elements built with
`document.createElement`.

This is a security decision with a structural payoff: message content *cannot*
become markup, so the app needs no HTML sanitizer, and the server can ship a
CSP with no `unsafe-inline` for scripts. Search highlighting uses control-
character sentinels (U+0002/U+0003) that the client converts to `<mark>`
elements after the text is already inert — a message containing `<mark>` renders
it literally.

---

## 6. Authentication

Sessions are opaque 256-bit random tokens, not signed claims.

A JWT cannot be revoked before expiry without server-side state anyway, so the
signature buys nothing here — the database is the source of truth and logout is
a `DELETE`. Only a SHA-256 of each token is stored, so a database dump yields no
usable sessions. SHA-256 rather than Argon2 for the token specifically, because
the input is already 256 bits of uniform randomness: there is nothing to
brute-force, and this runs on every authenticated request.

Passwords use Argon2id with per-password salts. A failed login for a
nonexistent account runs a dummy verification so response timing does not reveal
which handles exist, and both cases return the identical response body.

Tokens are accepted from `Authorization: Bearer` or an `HttpOnly` cookie. The
cookie exists because browsers cannot set headers on a WebSocket handshake or on
an `<img>` request for an attachment.

---

## 7. What was deliberately left out

- **Multi-node.** The hub's routing table is in-process. Scaling out needs a
  shared bus; the node field in the ID scheme reserves room for it.
- **Multi-workspace.** Would put a workspace column on every table and a join in
  every query, for no benefit to a single-team deployment.
- **A CRDT or operational transform.** Chat messages are immutable-ish records
  with a total order supplied by the ID scheme. Concurrent editing is a
  different product.
- **Read receipts per message.** A per-channel cursor is what people actually
  use, at a fraction of the write volume.
