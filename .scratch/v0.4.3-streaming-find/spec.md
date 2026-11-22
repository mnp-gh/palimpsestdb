# v0.4.3: Streaming find

Status: resolved

`find` stops loading a whole Collection into memory. Independent of v0.4.1 and v0.4.2. Design: [ADR 0016](../../docs/adr/0016-streaming-reads-rescan-in-chunks.md).

## Out of scope

- Changing the Storage Engine trait to pull iterators (ADR 0016).
- Streaming from `Node::find` (autocommit): it keeps returning `Vec`, collected inside its Transaction. A cursor owning its Transaction waits for v0.6.
- Catalog `scan` (listings of Namespaces, Collections, Indexes): small and usually served by the Catalog cache, so it stays a `BTreeMap`.
- Sorting or ordering options (v0.8).

## Change

- `Transaction::find` resolves the Collection entry with `&mut self`, then returns `impl Iterator<Item = Result<Document>>` borrowing `&Transaction`, so the Write Set cannot change while it lives. `scan_range` (Document and Index ranges) streams the same way. Code at `transaction.rs:171-230`, `:335-375`.
- Chunks: each is one engine `scan` (via `mvcc::scan_range_at`) at the Snapshot, stopping at whichever comes first of `Limits::scan_chunk_keys` Documents returned (default 1,000) or `Limits::scan_chunk_bytes` bytes returned (default 4 MB); the first Document of a chunk is always taken, however large. A chunk ends on a whole Document; the next starts past all its Versions (`physical_key(last, 0)` plus one byte). Each chunk releases the engine's lock.
- The Transaction's own Write Set is merged into the stream in key order (own value wins, own delete hides).
- Results stay in Document ID order. Full scans and `_id` ranges stream directly; Index plans read their Index ranges chunk by chunk into a `BTreeSet` of candidate Document keys (deduplicated across ranges), then stream Documents in that order.
- Errors: a failed chunk yields one `Err`, then the iterator returns `None`.
- Keep the scan state (resume key, Write Set position) in its own struct that takes the Transaction per step, so an owning wrapper for `Node::find` is a small addition later.
- The strict-field check (`schema.rs:209`) uses the streaming scan.
- Callers of `Transaction::find` (CLI, tests) collect where they need a `Vec`.

## Tests

- With chunk limits set to 2, a Collection larger than several chunks returns every Document once, in ID order, with the Transaction's own inserts, updates and deletes applied, including Documents with several Versions and deleted Documents at chunk boundaries.
- A Document larger than `scan_chunk_bytes` is still returned.
- An Index plan with overlapping ranges (`$in`) returns each Document once.
- A commit by another Transaction between chunks is not seen.
