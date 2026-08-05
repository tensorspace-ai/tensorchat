# TensorChat

[![CI](https://github.com/tensorspace-ai/tensorchat/actions/workflows/ci.yml/badge.svg)](https://github.com/tensorspace-ai/tensorchat/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.95+](https://img.shields.io/badge/rust-1.95%2B-orange.svg)](https://www.rust-lang.org)

Team chat that runs on one box. A Rust server, an embedded database, and a
web client with no framework.

The goal is Slack's core, built so that a small deployment costs almost
nothing and a large one degrades gracefully: a single binary, a single SQLite
file, ~92 kB of JavaScript (32 kB gzipped) with no framework and no runtime
dependencies, and a realtime path that does one serialization per event rather
than one per recipient.

![A TensorChat channel](docs/screenshots/channel.png)

---

## Features

**Messaging** — channels (public and private), direct and group messages,
threaded replies, editing, deletion, emoji reactions with a searchable picker
and `:shortcode:` completion, file attachments, and `@mentions` with `@here` /
`@channel`. Private channels are invite-only, and any member can add or remove
people.

**Realtime** — WebSocket delivery of messages, reactions, typing indicators,
presence, membership changes, and read state, with automatic reconnection and
resynchronization.

**Reading** — unread and mention badges, a per-channel read cursor synchronized
across devices, pinned and saved messages, per-channel mute, and full-text
search with highlighted snippets, scoped to the channels you belong to. Queries
take `from:`, `in:`, `before:`, `after:` and `has:link` / `has:file` /
`has:image` operators, which also work on their own — `from:alice in:general` is
a valid search. Search hits and permalinks jump into the surrounding history
rather than dead-ending.

**Administration** — the first account is an administrator, and can promote
others or deactivate accounts. Deactivation signs the account out everywhere and
blocks sign-in while leaving its messages, mentions and threads intact.

**Invites** — with registration closed, administrators hand out invite links
that admit exactly the people they meant to. A link can be single-use or
multi-use, expiring or permanent, and is revocable at any time. The seat is
claimed in the same transaction that creates the account, so a single-use link
cannot admit two people who click it at once.

**Integrations** — bot accounts with long-lived API tokens. A token works as a
bearer credential on the whole HTTP API, and `POST /api/hooks/{token}` accepts
`{"channel": "...", "text": "..."}` for senders that cannot set headers. A bot
can only reach the channels it has been added to, so a leaked hook URL is
contained by membership like anything else.

**Notifications** — mentions and direct messages raise a notification, whether
or not a tab is open. Web Push wakes a service worker, which then fetches the
content from your own server, so message bodies never pass through Google's or
Mozilla's push infrastructure. Opt-in, and muted channels stay quiet.

**Installable** — a progressive web app: install it from the browser, launch it
standalone, and open it offline to a cached shell rather than a dinosaur.

**Client** — virtual-scrolled history that stays smooth over a hundred thousand
messages, optimistic sending, per-channel drafts that survive a reload, inline
message editing, mention autocomplete, drag-drop and paste-to-upload, a
System/Light/Dark theme toggle, and keyboard navigation.

---

## Screenshots

**Threads.** Replies live in a side pane, so a deep tangent never buries the
channel. The channel keeps a reply count and the hover bar offers quick
reactions.

![Thread pane open beside a channel](docs/screenshots/thread.png)

**Search.** Full-text search over every channel you belong to, with the match
highlighted in the snippet. `⌘K` from anywhere.

![Search results with highlighted matches](docs/screenshots/search.png)

**Light theme.** Follows the system preference by default, or pick one
explicitly in Preferences. Also visible here: `@here`, inline code, quotes, and
consecutive messages from one author collapsing into a single block.

![The incidents channel in the light theme](docs/screenshots/light.png)

**Direct messages.** One-to-one and group, in the same sidebar as channels.

![A direct message conversation](docs/screenshots/dm.png)

---

## Quick start

Requires Rust 1.95+ (2024 edition) and Node 22+.

```sh
# Build the web client
cd web && npm install && npm run build && cd ..

# Run the server
cargo run --release -p tensorchat-server
```

Open <http://127.0.0.1:8080> and create an account. The first account to
register becomes the workspace administrator. The first thing to do is make a
channel.

Upgrading an existing deployment does *not* pick an administrator for you —
promoting an arbitrary account would be a surprising grant of privilege. Choose
one deliberately:

```sh
tensorchat promote you
```

The database (`tensorchat.db`) and uploads (`blobs/`) are created on first run.
Nothing else is needed — no migration step to run, no services to start. Later
upgrades apply their schema changes on startup, so deploying a new version is
still just restarting the binary.

### From crates.io

```sh
cargo install tensorchat-server
```

That installs the `tensorchat` command. It does not install a frontend: the web
client is built by Node, and shipping a compiled bundle inside a source crate
would put an artifact nobody can audit into your dependency tree. Build it from
a checkout and point the server at it:

```sh
git clone https://github.com/tensorspace-ai/tensorchat
cd tensorchat/web && npm install && npm run build && cd ../..
TC_WEB=tensorchat/web/dist tensorchat
```

If you only want the API — a bot, a bridge, an integration — skip that and set
`TC_WEB` to any empty directory. For an actual chat deployment, prefer the
Docker image or a release archive below; both come with the frontend in place.

### With Docker

```sh
docker compose up -d
```

Same address. The database and uploads live in the `tensorchat-data` volume —
back that up and you have backed up everything. Published images are at
`ghcr.io/tensorspace-ai/tensorchat`.

The compose file binds to loopback on purpose. Put a TLS-terminating reverse
proxy in front before exposing it; the server speaks plain HTTP, and session
tokens are bearer credentials.

### Configuration

All configuration is environment variables. Everything has a working default.

| Variable | Default | Meaning |
| --- | --- | --- |
| `TC_BIND` | `127.0.0.1:8080` | Listen address. Set `0.0.0.0:8080` to accept external traffic. |
| `TC_DB` | `tensorchat.db` | SQLite database path. |
| `TC_BLOBS` | `blobs` | Directory for uploaded files. |
| `TC_WEB` | `web/dist` | Built frontend to serve. |
| `TC_NODE_ID` | `0` | Distinguishes ID generators if several instances share a database. |
| `TC_MAX_UPLOAD` | `26214400` | Maximum upload size in bytes. |
| `TC_OPEN_REGISTRATION` | `true` | Set `false` to close signups. Invite links still work — see below. |
| `TC_AUTH_BURST` | `10` | Login/register attempts allowed per client address before throttling. |
| `TC_AUTH_PER_SECOND` | `0.5` | Refill rate for that allowance. Raise both behind a proxy that hides client addresses. |
| `TC_PUBLIC_URL` | *(unset)* | How people reach this server, e.g. `https://chat.example.com`. Used for links printed by `tensorchat invite`. |
| `TC_PUSH_CONTACT` | `mailto:admin@localhost` | Contact address in VAPID tokens. Set to an empty string to disable Web Push. |
| `TC_PERMISSIVE_CORS` | `false` | Development only. |
| `TC_OIDC_ISSUER` | *(unset)* | Enables single sign-on. Issuer URL of an OpenID Connect provider — see below. |
| `TC_OIDC_CLIENT_ID` | *(unset)* | Required when `TC_OIDC_ISSUER` is set. |
| `TC_OIDC_CLIENT_SECRET` | *(unset)* | Required when `TC_OIDC_ISSUER` is set. |
| `TC_OIDC_REDIRECT_URL` | *(unset)* | Required when `TC_OIDC_ISSUER` is set. Must match what the provider has registered, exactly. |
| `TC_OIDC_SCOPES` | `openid profile` | Space-separated. Must include `openid`. |
| `TC_OIDC_LABEL` | *(issuer host)* | What the sign-in button calls the provider. |
| `RUST_LOG` | `tensorchat_server=info` | Log filter. |

### Single sign-on

Setting `TC_OIDC_ISSUER` adds a "Sign in with …" button beside the password
form. Passwords keep working; pair it with `TC_OPEN_REGISTRATION=false` if the
provider should be the only way to get an account.

Any OpenID Connect provider works — the endpoints are read from the issuer's
`/.well-known/openid-configuration`, and nothing in the code names a vendor.
Register this server as a **confidential** client with an authorization-code
grant, then:

```sh
export TC_OIDC_ISSUER=https://id.example.com
export TC_OIDC_CLIENT_ID=...
export TC_OIDC_CLIENT_SECRET=...
export TC_OIDC_REDIRECT_URL=https://chat.example.com/api/oauth/callback
tensorchat
```

The flow is an authorization code with PKCE. The redirect URL must be the one
registered with the provider, character for character, or the provider will
refuse the exchange.

Accounts are created on first sign-in. The handle comes from the provider's
`preferred_username`, reduced to the characters a handle may contain and
numbered if it is already taken — `alice`, then `alice2`. Identities are keyed
on the issuer and the subject, never on an email address: an address a provider
lets someone set unverified, or releases and later reassigns, would otherwise
be a way into an existing account. There is consequently no way to attach a
provider to an account that already exists; that account signs in with its
password.

An account created this way has no password, and deactivating it closes the
single-sign-on route too.

`https` is required unless the issuer is a loopback address, which is the
exception that lets you develop against a provider running on the same machine.
The flow's confidentiality rests on TLS: the ID token's signature is not
checked, because the subject is read directly from the provider's userinfo
endpoint over an authenticated TLS connection, which OIDC Core §3.1.3.7 permits
in place of verifying the token.

### Closing registration

`TC_OPEN_REGISTRATION=false` shuts the public sign-up form. Invite links still
work, so this is the setting most deployments want — and it can be set from the
very first boot. Mint the first link from the operator console:

```sh
TC_OPEN_REGISTRATION=false tensorchat     # start it closed, from empty
tensorchat invite                         # in another shell; prints a link
```

Whoever opens that link first picks their own handle and password and becomes
the administrator. There is no window in which the sign-up form is open to
anyone who finds the address.

An invite grants nothing but the right to create an account; the account it
makes is an ordinary member. Links are stored as a SHA-256 digest and shown
exactly once, so a lost link is reissued rather than recovered.

### Operator console

The same binary runs a few commands directly against the database, for the
things that cannot go through the API because they are what *creates* the
authority the API checks:

```sh
tensorchat invite [--uses N] [--days N] [--label TEXT] [--url ORIGIN]
tensorchat promote <handle>
tensorchat demote <handle>
tensorchat help
```

They read the same environment as the server (`TC_DB` and friends), so they act
on the same database with no extra arguments — including while it is running,
since SQLite's WAL mode allows a second writer. Under Docker:

```sh
docker compose exec tensorchat tensorchat invite
```

These require no authentication, and deliberately so: anyone who can run this
binary against the database can already read every message in it. Filesystem
access *is* the authentication, and these commands grant nothing it did not
already grant. Set `TC_PUBLIC_URL` so the printed link points at the name people
actually use rather than the bind address.

### Notifications

Web Push needs a **secure context**, which in practice means serving over HTTPS
(`localhost` is exempt, so development works unconfigured). Put the usual
reverse proxy in front and notifications start working with no further setup:
the VAPID keypair is generated into the database on first run and reused
afterwards.

Set `TC_PUSH_CONTACT` to a real address you are willing to be contacted at —
push services use it to report abuse from your server. Setting it to an empty
string turns the feature off, and clients then fall back to notifications that
only appear while a tab is open.

### Development

```sh
cd web && npm run dev     # rebuild the client on change
cargo run -p tensorchat-server    # debug server
```

---

## How it works

Three crates, one clear dependency direction:

```
tensorchat-core   domain types, wire protocol, ID generation   (no I/O)
   ↑
tensorchat-store  SQLite: schema, queries, full-text search    (no async)
   ↑
tensorchat-server axum: HTTP, WebSocket hub, authentication
```

[`ARCHITECTURE.md`](ARCHITECTURE.md) explains the design decisions in full. The
three that shape everything else:

**One encode per event, not per recipient.** Serialization dominates the
delivery path. A message to a channel with 500 connected members is encoded once
into a refcounted buffer that all 500 sockets share — so broadcasting costs one
`rmp_serde` pass and 500 atomic increments. This is only possible because every
broadcast frame is viewer-independent by construction; anything per-viewer (an
ack, a read receipt, a reaction's "did I react?" flag) is sent separately or
derived client-side from a delta.

**Snowflake IDs, used as everything.** Every entity is keyed by a time-sortable
u64. That one choice makes the primary key double as the pagination cursor and
the time index: history is `WHERE channel_id = ? AND id < ? ORDER BY id DESC`,
a pure descending index scan with no offset, no sort, and no timestamp column.
Jumping to a message is the same index read in both directions from an anchor.
Message timestamps are recovered from the ID, so they cost nothing on the wire.

**SQLite, in process.** A chat server's working set is "the last few hundred
messages in the channels I have open" — a page-cache problem, not a distributed
one. Running the database in-process removes a network hop and a serialization
round trip from every read. WAL mode gives concurrent readers alongside a
writer, which is exactly the shape of the workload.

---

## Measuring it

The load generator opens N WebSocket clients in one channel, publishes from a
few of them, and reports end-to-end latency plus the fanout amplification the
server achieved. It has no dependencies — Node's built-in WebSocket client and
the same MessagePack codec the browser uses.

```sh
node tools/loadtest.mjs --clients 250 --publishers 8 --rate 2 --seconds 12
```

On an M-series laptop, 250 connected clients in one channel:

```
  connections         250
  sends accepted      184
  deliveries observed 46,000
  delivery rate       3,678/s
  observed / expected 100.0%

  latency p50         9.8 ms
  latency p99         81.5 ms

  frames encoded      1,117
  frames delivered    72,429
  dropped consumers   0
  fanout ratio        64.8x
```

The last number is the point: each serialization served ~65 sockets. A server
that encoded per recipient would have run serde 65 times as often for the same
delivery.

Note that a single Node process decoding hundreds of sockets becomes the
bottleneck before the server does; the tool says so when it detects that, rather
than reporting its own backlog as server loss.

---

## Testing

```sh
cargo test --workspace     # 336 tests
cd web && npm test         # 164 tests
cd web && npx tsc --noEmit # type check
```

The suite covers unit behavior, HTTP integration against the real router
(authorization, rate limiting, security headers, the full message lifecycle),
and concurrency against a real file-backed database. That last group exists
because of a bug it caught: SQLite's `busy_timeout` does not apply to a
`DEFERRED` transaction upgrading from read to write, so concurrent sends failed
until every write path was switched to `IMMEDIATE`.

---

## Security

Passwords are Argon2id. Sessions are opaque 256-bit random tokens, stored only
as a SHA-256 digest, so a database dump yields no usable sessions and logout is
a `DELETE` rather than a blocklist entry. Login answers identically whether an
account exists or the password was wrong, including in timing. Changing a
password signs out every other device, because a session is a bearer token that
would otherwise outlive the password it was issued against.

Channel membership is checked inside the same transaction as every write, and
search joins against membership rather than filtering afterwards, so a query
cannot leak a private channel.

The client builds DOM nodes and never HTML strings — no `innerHTML` anywhere —
so message content cannot become markup. That is what lets the app ship a
Content-Security-Policy with no `unsafe-inline`. Uploads are served from a short
allowlist of content types and forced to download otherwise, so an uploaded
`.svg` or `.html` cannot execute on the origin.

---

## Limitations

Honest ones, since a chat server invites comparison:

- **Single node.** The hub's routing table is in-process. Multiple instances
  behind a load balancer would need a shared bus; `TC_NODE_ID` reserves ID space
  for that, but the work is not done.
- **One workspace per deployment.** Multi-tenancy would mean a workspace column
  on every table and a join on every query.
- **The user directory ships whole at connect.** Fine into the low thousands;
  beyond that it needs paging.
- **No outbound integrations.** Incoming webhooks exist; there is nothing that
  calls *out* to another service, and no slash commands.
- **No Web Push.** Desktop notifications work while a tab is open; waking a
  device with no page open would need a service worker, VAPID keys, and a
  subscription store. No voice, no calls, no bridges.
- **Search is FTS5.** Excellent for message bodies, not a relevance-tuned
  engine.

---

## Contributing

Bug reports, fixes, and features are welcome. [`CONTRIBUTING.md`](CONTRIBUTING.md)
covers the build, what CI checks, and the design constraints that are
load-bearing — read the last of those before proposing anything large.

Found a security issue? Please report it privately: see
[`SECURITY.md`](SECURITY.md).

Release notes are in [`CHANGELOG.md`](CHANGELOG.md).

## License

[MIT](LICENSE) © TensorSpace, Inc.

Provided as is, without warranty of any kind, as the license sets out. It is
pre-1.0 software that stores other people's messages — keep your own backups.
