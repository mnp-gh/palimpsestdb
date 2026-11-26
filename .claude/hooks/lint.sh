#!/usr/bin/env bash
# PostToolUse hook: after an edit to Rust code or a manifest, run the lints
# and hand failures back to Claude (exit 2 sends stderr to the model).
# Otherwise show Claude each comment the edit added, to check it says why.
# Output is trimmed to what Claude acts on, since every line costs tokens.
input=$(cat)
file=$(jq -r '.tool_input.file_path // empty' <<< "$input")
[[ "$file" =~ \.(rs|toml)$ ]] || exit 0
[[ "$file" == *.rs ]] && files=("$file") || files=()

if ! out=$(CARGO_TERM_COLOR=never "$CLAUDE_PROJECT_DIR"/scripts/lint.sh "${files[@]}" 2>&1); then
  # Keep each error, its location and its notes. Drop code snippets, links,
  # and clippy's advice to silence the lint, which is the wrong fix here.
  out=$(grep -E '^(error|warning)|-->|= (note|help):|^\S+\.rs:[0-9]+: ' <<< "$out" \
    | grep -vE 'for further information visit|implied by|requested on the command line|could not compile|aborting|to override|allow the lint')
  echo "$out" | head -40 >&2
  lines=$(wc -l <<< "$out")
  (( lines > 40 )) && echo "... $((lines - 40)) more lines; fix these first" >&2
  exit 2
fi

# Plain `//` comment lines in the new text that the replaced text lacked.
added=$(jq -r '
  def comments: [splits("\n") | select(test("^\\s*//($|[^/!])")) | sub("^\\s+"; "")];
  (.tool_input | [.new_string, .content, (.edits[]?.new_string)] | map(select(.)) | join("\n") | comments)
  - (.tool_input | [.old_string, (.edits[]?.old_string)] | map(select(.)) | join("\n") | comments)
  | unique | .[:5] | .[]' <<< "$input")
[[ -n "$added" ]] || exit 0
jq -n --arg comments "$added" '{hookSpecificOutput: {hookEventName: "PostToolUse", additionalContext:
  ("New comments:\n" + $comments + "\nKeep only those saying why; delete any saying what.")}}'
