# Contributing to TensorChat

Thanks for your interest. This document covers how to get the project building,
what the review bar is, and where the design constraints are non-negotiable.

## Getting set up

You need Rust 1.85+ (2024 edition) and Node 22+.

```sh
git clone https://github.com/tensorspace-ai/tensorchat
cd tensorchat
cd web && npm install && npm run build && cd ..
cargo run -p tc-server
```

Then open <http://127.0.0.1:8080>. The database and `blobs/` directory are
created on first run.

For iterating, run the client build in watch mode in one terminal and the server
in another:

```sh
cd web && npm run dev
cargo run -p tc-server
```

## Before you open a pull request

Everything CI checks, you can run locally:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

cd web
npm test
npx tsc --noEmit
npm run build
```

CI runs exactly these on Linux and macOS. A pull request that fails any of them
will not be reviewed until it is green.

## What makes a change easy to accept

**Explain the why in the code.** This codebase comments decisions, not
mechanics. `// increment the counter` is noise; a comment explaining why writes
use `IMMEDIATE` transactions is why the next person does not reintroduce a bug.
Match the density and tone of the surrounding file.

**Bring a test.** Bug fixes should come with a test that fails before the fix.
Behavior changes should come with a test that describes the new behavior. The
concurrency suite in `crates/tc-store/tests/` exists because a real bug slipped
past unit tests — if your change touches transaction boundaries or the hub's
routing, that is the suite to extend.

**Keep the diff to one idea.** A fix and a refactor in the same pull request take
several times as long to review as the two separately.

**Update the changelog.** Add an entry under `## [Unreleased]` in
[`CHANGELOG.md`](CHANGELOG.md) for anything user-visible.

## Design constraints

These shape the architecture and are the most common reason a change gets
pushback. [`ARCHITECTURE.md`](ARCHITECTURE.md) has the full reasoning.

**Broadcast frames stay viewer-independent.** The realtime path encodes each
event once and shares one refcounted buffer across every recipient socket. That
only works because no broadcast frame depends on who is receiving it. If a
feature seems to need a per-viewer field in a broadcast, it needs to be sent
separately or derived client-side from a delta instead.

**No runtime dependencies in the client.** The web client is TypeScript, esbuild,
and nothing else. A pull request adding a framework or a runtime package will
not be merged. `devDependencies` additions need a strong justification.

**No `innerHTML`, ever.** The client builds DOM nodes. This is what allows the
Content-Security-Policy to ship without `unsafe-inline`, and it is what makes it
structurally impossible for message content to become markup.

**Membership is checked in the transaction that writes.** Not before it, not
after it. Authorization that happens outside the write transaction is a race.

**The dependency direction is one-way.** `tc-core` (no I/O) ← `tc-store` (no
async) ← `tc-server`. Nothing flows the other way.

## Reporting bugs and requesting features

Use the [issue templates](https://github.com/tensorspace-ai/tensorchat/issues/new/choose).
For a bug, the version, the platform, and a reproduction matter more than
anything else you can write.

Security vulnerabilities are the exception — do not open a public issue. See
[`SECURITY.md`](SECURITY.md).

## Scope

Some things are deliberately out of scope; see the Limitations section of the
[README](README.md#limitations). Multi-node clustering, multi-tenancy, voice,
and protocol bridges are all large enough that they need a design discussion in
an issue before any code gets written. Open one first — it is much cheaper than
finding out after the fact.

## License

By contributing, you agree that your contributions are licensed under the
[MIT License](LICENSE) that covers the project.
