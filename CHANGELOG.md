# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Theme toggle** — System / Light / Dark in Preferences, replacing a light
  theme that could only follow `prefers-color-scheme`. "System" remains the
  default and keeps tracking the OS live. The light palette moved from a media
  query to `:root[data-theme='light']` so it exists exactly once; a small
  classic script stamps the resolved theme before the first paint, since the
  main bundle is a module and therefore deferred, and the CSP has no
  `unsafe-inline` to put it in the page.
- **Inline message editing** — editing happens in the message row instead of a
  native `prompt()`, with Enter to save, Shift+Enter for a newline and Escape
  to cancel. Up-arrow on an empty composer opens your last message; the hook
  for that existed but had never been connected. The editor survives the
  virtual list recycling its row mid-edit, caret position included.
- **Drafts survive a reload** — unsent text moved from an in-memory `Map` to
  `localStorage`. Writes are debounced so typing never blocks on a synchronous
  disk write, and flushed on `pagehide` and on the tab being hidden so a reload
  moments after the last keystroke keeps it. Drafts are pruned by age and
  count, truncated to the server's body limit, and cleared on sign-out.
- **Invite links** — administrators mint links from Preferences → Invite
  people, or `POST /api/admin/invites`. A link may be single-use or multi-use,
  expiring or permanent, and is revocable. `/api/register` accepts an `invite`
  field that admits an account even when `TC_OPEN_REGISTRATION` is false,
  closing the gap where a closed workspace had no way at all to add its second
  person. Recipients open `#/join/{token}`, which pre-checks the link and opens
  the sign-up form. Only a SHA-256 of the token is stored, and the seat is
  claimed inside the same transaction that creates the account, so a single-use
  link cannot admit two people who race each other.
- **Bots and API tokens** — administrators create bot accounts and mint
  long-lived tokens for them. A token authenticates the whole HTTP API as a
  bearer credential, and `POST /api/hooks/{token}` posts for senders that
  cannot set headers. A bot reaches exactly the channels it is a member of.
- **Jump to a message** — search hits, saved items and permalinks open the
  channel positioned on the message, via `?around=` on the history endpoint.
  A "Copy link to message" action produces `#/c/{channel}/{message}` links, and
  a bar offers "Jump to latest" while viewing a historical window.
- **Administrators** — the first account to register becomes one, and can
  promote others or deactivate accounts via `PATCH /api/admin/users/{id}` and a
  "Manage people" dialog. `User.deactivated` existed in the model but nothing
  had ever set it.
- **Emoji picker and `:shortcode:` completion** — a searchable picker on the
  composer and the message hover bar, and inline completion sharing the
  mention popover. Shortcodes expand on send, so what is stored is the emoji.
- **Desktop notifications** — mentions and direct messages raise a browser
  notification when you are not already looking at them. Opt-in from
  Preferences. Client-side only; real Web Push is still not implemented.
- **Channel mute** — `POST /api/channels/{id}/mute`, with a toggle in the
  channel header. A muted channel loses its unread emphasis but keeps its
  mention badge, and the counts are still reported truthfully rather than
  zeroed.
- **Saved messages** — a private, cross-channel collection reachable from the
  sidebar. `POST /api/messages/{id}/save`, `GET /api/saved`, and a per-user
  `saved` frame so a save syncs across tabs.
- **Pinned messages** — any member can pin a message to a channel; pins appear
  in a side pane, are marked in the main scroll, and are capped at 100 per
  channel. `POST /api/messages/{id}/pin`, `GET /api/channels/{id}/pins`, and a
  `pin` broadcast frame.
- **Schema migrations** — the database is upgraded in place on startup. Fresh
  installs get `schema.sql`; existing ones replay the numbered migrations newer
  than their `user_version`. A test asserts the two paths produce an identical
  schema.
- **Password changes** — `POST /api/me/password`, which re-verifies the current
  password and then revokes every *other* session, plus
  `DELETE /api/me/sessions` to do the revocation on its own. Both are exposed in
  Preferences. There was previously no way to change a password at all.
- **Channel invitations** — `POST /api/channels/{id}/members` and
  `DELETE /api/channels/{id}/members/{user}`, with an "Add people" control in the
  member pane. Private channels previously had no way in after creation: `join`
  refuses them by design, and nothing else could grant membership.

### Fixed

- A reaction, pin, or edit landing on a message that was already on screen now
  repaints it. `VirtualList` deliberately leaves an already-mounted,
  still-visible row alone so that scrolling does not rebuild stationary rows —
  correct for scrolling, but it also meant a change to a visible message's
  *content* was invisible until the reader happened to scroll it out of the
  window and back. A new `VirtualList.refresh()` repaints mounted rows, and the
  message list calls it whenever the data behind them changes.
- Login no longer succeeds for a deactivated account. It previously issued a
  token that then failed on every authenticated request, because `session_user`
  filtered deactivated accounts but `user_for_login` did not.
- Leaving or being removed from a channel now drops the hub subscription as well
  as the membership row. A subscription used to outlive membership until the
  socket reconnected, so a departed member kept receiving a private channel's
  messages for as long as their tab stayed open.

## [0.1.0] — 2026-07-29

Initial release.

### Added

- **Messaging** — public and private channels, direct and group messages,
  threaded replies, edit and delete, emoji reactions, file attachments, and
  `@mentions` with `@here` / `@channel`.
- **Realtime** — WebSocket delivery of messages, reactions, typing indicators,
  presence, membership changes, and read state, with automatic reconnection and
  resynchronization. Each event is serialized once and shared across all
  recipient sockets.
- **Reading** — unread and mention badges, a per-channel read cursor
  synchronized across devices, and FTS5 full-text search with highlighted
  snippets, scoped to the channels the caller belongs to.
- **Client** — virtual-scrolled history, optimistic sending, per-channel drafts,
  mention autocomplete, drag-drop and paste-to-upload, light and dark themes,
  and keyboard navigation. No framework and no runtime dependencies.
- **Operations** — a single binary serving its own frontend, a single SQLite
  file created on first run, and configuration entirely through environment
  variables.
- **Security** — Argon2id password hashing, opaque session tokens stored only as
  SHA-256 digests, membership checks inside the same transaction as every write,
  a Content-Security-Policy with no `unsafe-inline`, and a content-type
  allowlist for uploads.

[Unreleased]: https://github.com/tensorspace-ai/tensorchat/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/tensorspace-ai/tensorchat/releases/tag/v0.1.0
