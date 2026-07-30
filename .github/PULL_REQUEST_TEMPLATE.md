<!--
Thanks for contributing. Keep the diff to one idea — a fix and a refactor
together take several times as long to review as the two separately.
-->

## What this changes

<!-- What the change does, and why it is the right fix rather than one that
     happens to work. Link the issue it closes: "Closes #123". -->

## How it was verified

<!-- What you actually ran or clicked, beyond the test suite passing. -->

## Checklist

- [ ] `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass
- [ ] `cargo test --workspace` passes
- [ ] `npm test` and `npx tsc --noEmit` pass in `web/` (if the client changed)
- [ ] Tests cover the change — a bug fix has a test that failed before it
- [ ] `CHANGELOG.md` has an entry under `## [Unreleased]` (if user-visible)
- [ ] The README or `ARCHITECTURE.md` is updated (if behavior or design changed)

## Design constraints

<!-- Delete any that this PR does not touch. See CONTRIBUTING.md. -->

- [ ] Broadcast frames stay viewer-independent — nothing per-recipient was added to one
- [ ] No runtime dependency was added to the web client
- [ ] No `innerHTML`; the client still builds DOM nodes
- [ ] Membership checks still happen inside the write transaction
- [ ] The `tensorchat-core` ← `tensorchat-store` ← `tensorchat-server` dependency direction is unchanged
