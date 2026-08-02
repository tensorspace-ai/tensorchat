#!/bin/sh
# The quality gate. Run this before every commit.
#
# It runs the same checks CI does, in the same order, so a green run here means
# a green run there. Keep the two in step: if you add a check to
# .github/workflows/ci.yml, add it here.
set -eu

cd "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"

echo "==> fmt"
cargo fmt --all --check

echo "==> clippy"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> test"
cargo test --workspace

echo "==> web"
cd web
[ -d node_modules ] || npm ci
# The script rather than `tsc` directly: the service worker is checked under its
# own config, and invoking the compiler by hand would silently skip it.
npm run typecheck
npm test
npm run build

echo
echo "all checks passed"
