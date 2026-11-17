#!/usr/bin/env bash
# Fill a data directory with mock shop data through the CLI.
# Usage: scripts/seed.sh [data-dir] [users]   (defaults: data 100)
# Fails if the directory already holds a `shop` Namespace.
set -euo pipefail

dir=${1:-data}
users=${2:-100}
statuses=(open paid shipped cancelled)

{
  echo "create-ns shop"
  echo "create-coll shop.users"
  echo "create-coll shop.orders"
  echo "set-type shop.users email string strict"
  echo "create-index shop.users email unique"
  echo "set-type shop.orders user int"
  echo "create-index shop.orders user"
  echo "set-type shop.orders total int"
  echo "create-index shop.orders total"
  # One Transaction per Collection: one fsync instead of one per Document.
  echo "begin"
  for ((u = 1; u <= users; u++)); do
    echo "insert shop.users {\"_id\": $u, \"name\": \"User $u\", \"email\": \"user$u@example.com\", \"age\": $((18 + u * 7 % 60))}"
  done
  echo "commit"
  echo "begin"
  for ((o = 1; o <= users * 3; o++)); do
    echo "insert shop.orders {\"user\": $((o * 13 % users + 1)), \"total\": $((o * 37 % 500 + 5)), \"status\": \"${statuses[o % 4]}\"}"
  done
  echo "commit"
} | cargo run -q -p psdb-cli -- --data-dir "$dir" >/dev/null

echo "Seeded $dir: $users users, $((users * 3)) orders in shop."
