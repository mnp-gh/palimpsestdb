# CLI data directory

Status: ready-for-agent

Blocked by: 06

Spec: [../spec.md](../spec.md)

## What

Add a `--data-dir <path>` option to `psdb-cli` that opens the database with `Database::open`. Without it, the CLI keeps today's in-memory, non-durable behaviour.

## Done when

Data written through the CLI with `--data-dir` is there after restarting the CLI.
