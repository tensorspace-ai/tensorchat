# Agent instructions

Rules for any AI agent working in this repository. Read this before changing
anything.

## Quality gate — required before every commit

```sh
./run-tests.sh
```

**Do not commit if it fails.** It runs the same checks as CI, in the same order:
`cargo fmt --check`, clippy with warnings denied, the Rust suite, then the web
client's type check, tests and build.

There are no exceptions for "small" or "obvious" changes. The concurrency suite
in `crates/tensorchat-store/tests/` exists because a real bug walked past the
unit tests; if you touch transaction boundaries or the hub's routing, extend it.

If a test fails and you believe the test is wrong, say so explicitly and explain
why. Do not delete, skip or weaken a test to make a change pass.

## Commit policy

Human contributors should follow [CONTRIBUTING.md](CONTRIBUTING.md), which
describes the pull request route.

- **Commit as you go**, at each coherent unit of work, rather than one batch at
  the end. A commit should leave the tree green.
- **Do not push.** Committing is the agent's job; publishing is the
  maintainer's, and it is theirs to time.
- **Commit to `main`.** No feature branches for routine work.
- Write a message that explains *why*, not just what changed. Match the style of
  the existing history.
- Never force-push, amend or squash unless asked.
- Add an entry under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md) for
  anything user-visible.

## Verify behaviour, not just compilation

A green build is not evidence that a feature works. For anything with a runtime
surface, drive it:

```sh
cargo run -p tensorchat-server     # then open http://127.0.0.1:8080
```

## Invariants that must not be broken

[ARCHITECTURE.md](ARCHITECTURE.md) has the full reasoning for each.

1. **Membership is checked in the transaction that writes.** Not before, not
   after. Authorization outside the write transaction is a race.
2. **Writes use `IMMEDIATE` transactions.** SQLite upgrades a deferred
   transaction mid-flight and can fail it with `SQLITE_BUSY` after work is
   already done.
3. **Broadcast frames stay viewer-independent.** Each event is encoded once and
   the buffer is shared across every recipient socket. A per-viewer field in a
   broadcast breaks that; send it separately or derive it client-side.
4. **The dependency direction is one-way**: `tensorchat-core` (no I/O) ←
   `tensorchat-store` (no async) ← `tensorchat-server`. Nothing flows back.
5. **No `innerHTML`, ever.** The client builds DOM nodes. This is what lets the
   CSP ship without `unsafe-inline`, and what makes it structurally impossible
   for message content to become markup.
6. **No runtime dependencies in the client.** TypeScript, esbuild, nothing else.
   A `devDependencies` addition needs a strong justification.
7. **A request type refuses a field it does not define.** Serde drops unknown
   fields by default, and most request types pair that with an `Option` or a
   default — so a misspelling used to deserialize as "not supplied" and the
   handler answered success having done nothing. New request and query types
   need `#[serde(deny_unknown_fields)]`.

## Style

- Formatting is whatever `cargo fmt` produces. There is no custom config.
- Comments explain *why*, not mechanics. Match the surrounding density.
- Keep the README honest. Its quantitative claims are checked against the
  source — if you change a test count or a bundle size, update the text.

## Repository layout

```
crates/tensorchat-core/    ids, model, wire types — no I/O
crates/tensorchat-store/   SQLite persistence — no async
crates/tensorchat-server/  axum + WebSockets + the realtime hub
web/                       TypeScript client, built with esbuild
```
