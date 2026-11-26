#!/usr/bin/env bash
# PostToolUse hook: after an edit to Rust code or a manifest, run the lints
# and hand any failures back to Claude (exit 2 sends stderr to the model).
# Otherwise show Claude each comment the edit added, to check it says why.
input=$(cat)
file=$(jq -r '.tool_input.file_path // empty' <<< "$input")
[[ "$file" =~ \.(rs|toml)$ ]] || exit 0

if ! out=$(CARGO_TERM_COLOR=never "$CLAUDE_PROJECT_DIR"/scripts/lint.sh 2>&1); then
  echo "$out" >&2
  exit 2
fi

# Plain `//` comment lines in the new text that the replaced text lacked.
added=$(jq -r '
  def comments: [splits("\n") | select(test("^\\s*//($|[^/!])")) | sub("^\\s+"; "")];
  (.tool_input | [.new_string, .content, (.edits[]?.new_string)] | map(select(.)) | join("\n") | comments)
  - (.tool_input | [.old_string, (.edits[]?.old_string)] | map(select(.)) | join("\n") | comments)
  | unique | .[]' <<< "$input")
[[ -n "$added" ]] || exit 0
jq -n --arg comments "$added" '{hookSpecificOutput: {hookEventName: "PostToolUse", additionalContext:
  ("You added these comments:\n" + $comments + "\n\nKeep a comment only if it says why, which the code cannot: delete any that say what the code does, and if one justifies a workaround, check the fix is right instead (docs/CONVENTIONS.md).")}}'
