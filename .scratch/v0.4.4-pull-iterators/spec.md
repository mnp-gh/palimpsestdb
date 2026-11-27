# v0.4.4: Pull iterators

Status: resolved

The Storage Engine's `scan` returns an iterator the caller pulls from, instead of calling a visitor, so a streaming read no longer rescans the engine in chunks. Comes before the `fjall` engine (v0.4.5), so that engine is written once against this trait. Design: [ADR 0018](../../docs/adr/0018-storage-engine-scans-are-pull-iterators.md), superseding ADR 0016.

## Out of scope

- Benchmarks or timings (v1.0).
- Unordered `find` and streamed Catalog listings (v0.4.7).

## Storage Engine

- An associated type `type Scan<'a>: Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> where Self: 'a;` and `fn scan(&self, range: &KeyRange) -> Self::Scan<'_>` replace `scan(range, visit)`; `ScanVisitor` goes. `Arc<E>` forwards with `type Scan<'a> = E::Scan<'a>`. Nothing uses `dyn StorageEngine`, so losing object safety costs nothing.
- Contract (rule replacing "`visit` must not call back into the engine"): keys come in ascending order; a key not written while the iterator is alive comes exactly once, with its value; a key written meanwhile has no promise (it may come with its new value, its old value, or not at all). After an `Err` the iterator yields `None`. The caller may write to the engine between `next` calls. An engine that freezes its view satisfies this.
- Conformance: more than 64 keys, so `MemoryEngine` refills its buffer mid-scan; writes and deletes between `next` calls, on keys before and after the cursor; untouched keys come exactly once, in order, with their values. Nothing is asserted about written keys.

## Engines

- `MemoryEngine`: the iterator takes the read lock, copies up to 64 entries (a fixed `const`, not in `Limits`) from its position into a buffer, releases the lock, and when the buffer runs out continues from just past the last key returned. No lock is held between `next` calls; no new dependency.
- The simulator's engine forwards.

## Database Layer

- `mvcc::scan_range_at` returns an iterator of visible `(logical key, value)` pairs at a Snapshot, built on the engine iterator.
- The chunked `Scan` in `transaction.rs` becomes a merge of that iterator with the Write Set's entries in the range (own writes overlaid, as today). `Limits::scan_chunk_keys`, `scan_chunk_bytes` and `with_scan_chunk` go.
- Other `scan` callers (GC, Catalog cache load, `mvcc` helpers, Index build, tests) become loops over the iterator. Garbage Collection keeps its shape: each `GcPass::step` opens a fresh iterator at `next_start` and deletes one batch, so under `fjall` no iterator outlives a step. An Index build still commits per chunk under the commit lock (ADR 0013), and opens a fresh engine iterator per chunk, like GC. (Changed in implementation: the spec first kept one iterator open for the whole build, but each chunk reads at a newer Commit Timestamp, so the loose view could hand it a Version a writer replaced between chunks, leaving a stale Index entry.)
- `Node::find` and other autocommit reads still collect into a `Vec`.

## Tests

- Existing streaming `find` tests pass without chunk limits; tests that set `with_scan_chunk` drop it.
- A `find` stream stays correct while another Transaction commits and GC runs between `next` calls.
