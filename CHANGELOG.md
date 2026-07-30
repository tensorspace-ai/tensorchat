# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
