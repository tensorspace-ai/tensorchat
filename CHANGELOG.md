# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
