# v0.4.6: Pull iterators

Status: ready-for-agent

The Storage Engine's `scan` returns an iterator the caller pulls from, instead of calling a visitor, so a streaming read no longer rescans the engine in chunks. Needs v0.4.4. Design: [ADR 0018](../../docs/adr/0018-storage-engine-scans-are-pull-iterators.md), superseding ADR 0016.

## Out of scope

- Benchmarks or timings (v1.0).
- Unordered `find` and streamed Catalog listings (v0.4.7).

## Storage Engine

- `fn scan(&self, range: &KeyRange) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_>` replaces `scan(range, visit)`; `ScanVisitor` goes. `Arc<E>` forwards.
- Contract (rule replacing "`visit` must not call back into the engine"): keys come in ascending order; a key not written while the iterator is alive comes exactly once; a key written meanwhile may or may not come. The caller may write to the engine between `next` calls. An engine that freezes its view (like `fjall`) satisfies this.
- Conformance: writes and deletes between `next` calls, on keys before and after the cursor; untouched keys come exactly once, in order.

## Engines

- `MemoryEngine`: the iterator takes the read lock, copies up to 64 entries from its position into a buffer, releases the lock, and when the buffer runs out continues from just past the last key returned. No lock is held between `next` calls; no new dependency.
- `fjall`: wraps its range iterator, mapping errors to `StorageError`.
- The simulator's engine forwards.

## Database Layer

- `mvcc::scan_range_at` returns an iterator of visible `(logical key, value)` pairs at a Snapshot, built on the engine iterator.
- The chunked `Scan` in `transaction.rs` becomes a merge of that iterator with the Write Set's entries in the range (own writes overlaid, as today). `Limits::scan_chunk_keys`, `scan_chunk_bytes` and `with_scan_chunk` go.
- Other `scan` callers (GC, Catalog cache load, `mvcc` helpers, Index build, tests) become loops over the iterator. An Index build still commits per chunk under the commit lock (ADR 0013); its chunk is a count of Documents taken from the iterator.
- `Node::find` and other autocommit reads still collect into a `Vec`.

## Tests

- Existing streaming `find` tests pass without chunk limits; tests that set `with_scan_chunk` drop it.
- A `find` stream stays correct while another Transaction commits and GC runs between `next` calls.
