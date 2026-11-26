#!/usr/bin/env bash
# Checks the crate rules in docs/CONVENTIONS.md: names, READMEs, and
# dependencies pointing down. Prints violations and exits 1 if any.
set -euo pipefail
cd "$(dirname "$0")/.."

# Captured first, so a failing cargo or jq stops the script (set -e).
dependency_problems=$(cargo metadata --no-deps --format-version 1 | jq -r '
  .packages[]
  | .name as $name
  # The psdb crates each crate may name as a normal dependency; null means
  # any. Tests (dev-dependencies) and binaries may name concrete engines.
  | (if $name == "psdb-storage" then []
     elif ($name | startswith("psdb-storage-")) or $name == "psdb-core" then ["psdb-storage"]
     else null end) as $allowed
  | (if ($name | startswith("psdb-")) then empty
     else "\($name): crate names start with psdb-" end),
    (if $allowed == null then empty else
       .dependencies[]
       | select(.kind == null and (.name | startswith("psdb-")))
       | select(.name | IN($allowed[]) | not)
       | "\($name): may not depend on \(.name) (dependencies point down)"
     end)
')
problems=()
while IFS= read -r problem; do
  if [[ -n "$problem" ]]; then
    problems+=("$problem")
  fi
done <<< "$dependency_problems"
for dir in crates/*/; do
  [[ -f "$dir/README.md" ]] || problems+=("${dir%/}: missing README.md")
done

if (( ${#problems[@]} > 0 )); then
  printf '%s\n' "${problems[@]}"
  exit 1
fi
