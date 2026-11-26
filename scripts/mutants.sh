#!/usr/bin/env bash
# Mutation testing on the code this branch changes (docs/CONVENTIONS.md):
# run before opening a pull request that changes non-test code. A mutant
# no test notices is a changed line no test would catch being wrong.
# Usage: scripts/mutants.sh [base], base defaulting to origin/main.
set -euo pipefail
cd "$(dirname "$0")/.."

# The pinned toolchain, as scripts/lint.sh uses.
if command -v rustup > /dev/null; then
  PATH="$(dirname "$(rustup which cargo)"):$PATH"
fi

# Against the working tree, so uncommitted changes are tested too and the
# diff matches the source cargo-mutants reads.
diff=$(mktemp)
trap 'rm -f "$diff"' EXIT
git diff "$(git merge-base "${1:-origin/main}" HEAD)" -- crates > "$diff"

# Each mutant reruns the tests; fewer property-test cases keep it quick.
PROPTEST_CASES="${PROPTEST_CASES:-32}" cargo mutants --in-diff "$diff" --jobs "${JOBS:-4}"
