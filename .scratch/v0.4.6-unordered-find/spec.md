# v0.4.6: Unordered find, streamed listings

Status: ready-for-agent

Follows v0.4.3, which streams `find` but still buffers every candidate key of an Index plan to keep Document ID order, and leaves Catalog listings unstreamed. This drops the order promise so nothing is buffered ([ADR 0017](../../docs/adr/0017-find-results-are-unordered.md)), and streams listings so a Namespace can hold ~1M Collections.

## Out of scope

- Sorting or ordering options (v0.8).
- Read Markers for a stream the caller stops reading partway (v0.5).
- Shrinking the Catalog cache, which still holds every Catalog entry in memory (Unscheduled).
- Bloom filters or planner changes for huge `$or`s (Unscheduled).

## Change

- `find` promises no order (ADR 0017). `candidate_keys` and its `BTreeSet` go (`transaction.rs:389`): a full scan or `_id` range streams Documents as today; an Index range streams its entries through `scan_range` and reads each Document by key.
- `$or` plans: `index::plan` returns ranges grouped by branch instead of one flat list (`index.rs:182-186`). Branches are read in turn; a candidate from branch *i* is returned only if it matches the whole filter and lies in no range of branches 0..*i*-1 (its Document key, or its Index entry key built from the Document, against each range). Nested `$or` applies the rule recursively. Add a `shortcut:` comment: work grows with the square of the branch count; see the Unscheduled roadmap item.
- `Transaction::list_namespaces` and `list_collections` return `Result<impl Iterator<Item = Result<String>> + '_>`, like `find`. `Node::list_namespaces` and `Node::list_collections` stay autocommit and collect into a `Vec`, like `Node::find`.
- Catalog listings stream through the same chunked `Scan`, with the same `scan_chunk_keys` and `scan_chunk_bytes` limits. Each chunk takes the Catalog cache's read lock, copies up to a chunk of live entries and releases it; a chunk that meets an entry newer than the Snapshot reads that chunk from the engine instead (today `CatalogCache::scan_at` gives up on the whole listing). The Index listing inside `collection_entry` (`catalog.rs:376`) collects, since a Collection has few Indexes. Add a `shortcut:` comment on `CatalogCache` that it holds the whole Catalog in RAM.
- Doc comments that promise Document ID order (`Transaction::find`, `Node::find`) say results are unordered.

## Tests

- An Index plan returns the same Documents as a full scan (compared as sorted IDs), across several chunks.
- An `$or` with overlapping branches (`{$or: [{age: 30}, {_id: X}]}`, X has age 30) returns each Document once.
- `list_collections` over several chunks returns every name once, and still matches the Snapshot when a Collection is created after it partway through.
- Existing `find` tests that assume Document ID order compare sorted IDs.
