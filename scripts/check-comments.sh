#!/usr/bin/env bash
# The mechanical half of "comment only the why" (docs/CONVENTIONS.md):
# no TODO-style markers and no commented-out code. Whether a comment says
# why is judgement; the Claude Code hook asks about each new one.
set -euo pipefail
cd "$(dirname "$0")/.."

status=0
if grep -rnE '//.*\b(TODO|FIXME|XXX|HACK)\b' crates --include='*.rs'; then
  echo "^ write \`// shortcut: <the limit>, <when to upgrade>\` or a docs/ROADMAP.md item instead"
  status=1
fi
# A comment line shaped like Rust: a keyword start ending in `;` or `{`, a
# call ending `);`, or a lone brace. The space after `//` leaves doc
# comments (`///`, `//!`) out.
if grep -rnE '^\s*// +((let|fn|use|pub|impl|struct|enum|match|if|for|while|return)\b.*[;{]|.*\);|[{}])\s*$' \
  crates --include='*.rs'; then
  echo "^ delete commented-out code; git keeps the history"
  status=1
fi
exit $status
