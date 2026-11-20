# v0.4.3: Streaming find

Status: ready-for-agent

`find` stops loading a whole Collection into memory. Independent of v0.4.1 and v0.4.2.

## Out of scope

- Changing the Storage Engine trait to pull iterators.
- Sorting or ordering options (v0.8).

## Change

- `Transaction::scan`, `scan_range` and `find` return iterators instead of a `BTreeMap` or `Vec` (`transaction.rs:170-193`, `:339-375`).
- Built from chunked rescans: each chunk is one engine `scan` at the Snapshot of up to N keys, remembering the last key and resuming after it. Safe because a Snapshot's Versions cannot change while it is registered. Each chunk releases the engine's lock, so writers are not held up for a whole scan. N is a new `Limits` field.
- The Transaction's own Write Set is merged into the stream in key order.
- Results stay in Document ID order. Full scans stream directly; Index plans collect candidate Document keys (deduplicated across ranges) in a `BTreeSet`, then stream Documents in that order.
- Other whole-Collection scans (the strict-schema check, `schema.rs:209`) move to the iterator where it is a small change.

## Tests

- A Collection larger than several chunks returns every Document once, in ID order, with the Transaction's own inserts, updates and deletes applied.
- An Index plan with overlapping ranges (`$in`) returns each Document once.
- A commit by another Transaction between chunks is not seen.
