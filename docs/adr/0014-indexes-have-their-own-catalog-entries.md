# Each Index has its own Catalog entry, keyed by Index ID

An Index's definition (`{ path, unique, state }`) lives in its own Catalog entry, in Catalog Collection 5 under `namespace_id ‖ collection_id ‖ index_id`, instead of in an `indexes` list inside its Collection's entry. Dropping an Index writes a Tombstone on that entry, so Garbage Collection finds a dead Index the way it finds a dead Namespace or Collection: the pass that deletes the Tombstone reclaims the ID's data. One rule covers every kind of dead ID, and nothing has to be kept in a list forever just to say an Index is gone.

## Considered Options

- **Find dead Indexes by `nextIndexId`** (ADR 0013): an Index is dead when its ID is below the counter at the Horizon and the Collection entry no longer lists it. No layout change, but Indexes then need a rule of their own, unlike Namespaces and Collections.
- **Keep dropped Indexes in `indexes` as `state: "dropped"`**: GC finds them, but nothing can remove the marker safely. Garbage Collection writes no Versions (ADR 0012), and a later logged rewrite that clears it would, after Recovery replays the WAL, leave the replayed data with nothing naming it as dead.
- **Key by Field path**, like Collections by name: dropping and recreating an Index on one field would stack Versions of different Indexes on one key, and creates on one field already conflict on `nextIndexId`.

## Consequences

- `nextIndexId` stays in the Collection entry, so creating an Index still rewrites it: concurrent creates conflict, and writers whose Snapshot predates a Building commit still abort with Collection Changed (ADR 0006, ADR 0013).
- A writer records a read of every Index Catalog entry it maintains, so dropping an Index or marking it Ready aborts concurrent writers to the Collection with Collection Changed through the existing check, and no Index entry is written under an ID after its drop. Dropping an Index no longer rewrites the Collection entry.
- Marking an Index Ready, or removing a failed build, writes only that Index's entry, so it can no longer undo a concurrent change to another Index on the same Collection.
- A dead Collection takes every Index Catalog entry under it, deleted after its data like the other Catalog deletes.
- The Catalog cache must list one Collection's Indexes, so it is ordered by key.
- Collection entries with an `indexes` list (written before this ADR) are not supported.
