#!/usr/bin/env bash
# PostToolUse hook: after an edit to Rust code or a manifest, run the lints
# and hand any failures back to Claude (exit 2 sends stderr to the model).
file=$(jq -r '.tool_input.file_path // empty')
[[ "$file" =~ \.(rs|toml)$ ]] || exit 0

if ! out=$(CARGO_TERM_COLOR=never "$CLAUDE_PROJECT_DIR"/scripts/lint.sh 2>&1); then
  echo "$out" >&2
  exit 2
fi
