# v0.3: Indexes

Status: ready-for-human (implemented on `v0.3-indexes`)

Single-field secondary Indexes, typed by a Collection Schema, kept consistent with their Documents at every Snapshot, and used by `find` when they fit the filter. Terms follow [GLOSSARY.md](../../GLOSSARY.md); the design is [ADR 0010](../../docs/adr/0010-index-builds-commit-in-chunks.md) and [ADR 0011](../../docs/adr/0011-field-types-live-in-collection-schema.md).

## Out of scope

- Compound and multikey Indexes, a cost-based planner, and Index hints (v0.8 and Unscheduled).
- Background Index builds; the v0.3 build holds the commit lock (v0.4).
- Garbage collection of orphaned or dropped Index entries (v0.4).
- Range comparisons on binary values, and Decimal128 in Indexes (Unscheduled).
- Validation beyond types, such as required fields (v0.8, in the same Collection Schema).

## Collection Schema

- Lives in the Collection's Catalog entry as `fields: [{ path, type, strict }]`. Entries written before v0.3 read as having none.
- Field Types: `int`, `double`, `string`, `date`, `bool`, `objectId`, `binary`.
- Paths are dotted, with non-empty segments, none starting with `$`, and not `_id`.
- A Strict Field rejects a write whose value does not convert exactly to its type with Type Mismatch; missing and null are allowed. A strict `int` accepts `30.0` and rejects `30.5`; a strict `double` rejects NaN and integers beyond 2⁵³.
- Making a field strict reads every Document and fails with Type Mismatch if any breaks the rule. A Document committed concurrently makes the change abort with Collection Changed.
- While an Index on a field exists, its type cannot change and the declaration cannot be removed; a field with a unique Index cannot stop being strict.

## Indexes

- At most one per field path, listed in the Catalog entry as `indexes: [{ path, id, unique }]`. The ID is counted per Collection (`nextIndexId` in its Catalog entry) and never reused; taking one also skips IDs that hold data from a failed build.
- Creating one needs the field's type declared; a unique one needs a Strict Field.
- Entries are versioned like Documents: `namespace_id ‖ collection_id ‖ 0x00 ‖ index_id ‖ component ‖ document ID`, inside the Collection's range and below its Documents, value the encoded Document ID. A unique Index drops the Document ID from the key unless the value is null or missing, so concurrent duplicates write the same key and the second commit gets a Write Conflict.
- Components: null and missing share one, below every type. `int` files doubles under their floor (saturating); `double` files integers as the nearest double. Values of other types, and NaN, are left out.
- Every write recomputes each Index's component for the old and new Version. An unchanged component writes nothing. A changed one writes a tombstone for the old entry and the new entry. A unique Index fails the write with Duplicate Value if another Document owns the new key. Nothing is written if any check fails.
- Dropping an Index removes it from the Catalog entry; its entries stay behind.

## Building

- `Node::create_index` holds the commit lock, reads the Collection at its Snapshot in chunks of at most 1024 Documents or 16 MiB of Documents, whichever comes first, and commits each chunk's entries as an ordinary fsynced commit. A final commit adds the Index to the Catalog entry. A duplicate in a unique build fails with Duplicate Value; committed chunks stay as orphans.
- `Transaction::create_index_on_empty` works only on a Collection that is empty in the Transaction's view, as an ordinary Catalog write; otherwise it fails with Collection Not Empty, checked before anything is written so a refusal writes nothing. A Document committed concurrently makes it abort with Collection Changed.

## Queries

- `find` uses an Index for an equality or range comparison that must hold for the filter to match (at the top level or inside `$and`), on an indexed field, against a value the indexed field's Field Type compares with. `{f: null}` uses the null component. Other range comparisons on the chosen field narrow the range.
- An `$or` whose every branch can use an Index reads the union of the branches' ranges, planned recursively; a Document found through several ranges is read once. One branch that cannot use an Index makes that `$or` unusable, since that branch's Documents would be missed.
- A comparison on `_id` reads a range of the Collection's own keys, which are sorted by `_id` (integers as in an Index on an `int` field, so a double bound works), with no Index lookup. It plans like an Index comparison, and an `_id` equality beats every other equality.
- Choice: an equality, else an indexed `$or`, else the first range comparison; ties go to the first in the filter. `$ne`, binary ranges and everything else scan the whole Collection.
- Bounds are inclusive, because coercion files different values under one key. Every candidate is fetched at the Snapshot, with own writes overlaid, and the whole filter is rechecked on raw BSON. Results stay in Document ID order.
- Filters compare integers and doubles exactly, without converting the integer.

## Write Set cap

- `Limits::max_write_set_bytes`, default 96 MB (replacing a 16 MB Document, with the old and new Index entries on its large fields, must fit; a key escapes NUL bytes, so it can be twice the field's size), counts the bytes of every key and value in the Write Set, Index entries included. A write that would cross it fails with Transaction Too Large and is not made.

## Tests

- A property test: random writes of mixed types, with the Index built before, during or after them, give the same `find` results as an unindexed Collection, for every comparison operator, `$or`, `$or` inside `$and`, and an `$or` with an unindexed branch.
- Unit tests for coercion, Strict Field conversions, key ordering of doubles and null, and the planner's choices.
- Integration tests for each rule above, including concurrent duplicates, races with DDL, a failed unique build leaving no Index, and an Index surviving reopen.

## CLI

`schema`, `set-type <ns> <path> <type> [strict]`, `unset-type`, `indexes`, `create-index <ns> <path> [unique]` (builds when no transaction is open) and `drop-index`.
