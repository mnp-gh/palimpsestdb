# `applied_up_to` key

Status: ready-for-agent

Spec: [../spec.md](../spec.md)

## What

In `crates/psdb-core/src/keys.rs`, add a reserved Collection ID 0 in the Catalog Database and an `applied_up_to_key()` built from `collection_prefix(CATALOG_DATABASE, 0)`. The key is unversioned: it is written directly, never through `physical_key`, and its value is the big-endian Commit Timestamp. Add helpers to read and encode it.

## Tests

- The key does not start with any Catalog prefix (`databases_prefix`, `collections_prefix`, the two ID-key prefixes) or any Document prefix of a Collection with a non-zero ID.
- No `scan_at` over an existing prefix ever visits it.

## Done when

The key and its helpers exist with tests; nothing writes it yet.
