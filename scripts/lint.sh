#!/usr/bin/env bash
# The lint checks, shared by the Claude Code hook and CI. Clippy runs twice:
# with tests, whose dev-dependencies turn on `testing`, and on the plain
# library, where code exported only for tests must not look public.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo clippy --workspace --all-targets --quiet -- -D warnings
cargo clippy --workspace --quiet -- -D warnings
scripts/check-crates.sh
scripts/check-comments.sh
