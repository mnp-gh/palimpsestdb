# Clearing dropped Index markers

Status: needs-triage

Since issue 05, dropping an Index (by the client, a failed build, or Recovery's `drop_building_indexes`) keeps it in its Collection entry's `indexes` list with `state: "dropped"`, and Garbage Collection deletes the data of every `dropped` Index it finds. Garbage Collection never writes Versions (ADR 0012), so nothing removes the marker: every pass looks up each dropped Index's (already empty) range again, and the list grows by one item per Index ever dropped on the Collection.

Options considered in the issue 05 grilling:

- **Leave them (current)**: one lookup per dropped Index per pass.
- **The next rewrite of the entry clears them**: creating or dropping an Index, or changing the Collection Schema, leaves out markers whose data is already reclaimed. Needs a way to know that, for example Garbage Collection recording the Horizon of its last finished pass; clearing a marker before its data is reclaimed leaks that data.
- **Garbage Collection commits a Transaction to remove them**: ruled out, since it logs Garbage Collection (against ADR 0012) and aborts concurrent writers to the Collection with Collection Changed.
