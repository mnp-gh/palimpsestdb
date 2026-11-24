# `find` results are unordered until sort exists

`find` streams its results (ADR 0016) and promises no order. A full scan or an `_id` range happens to come out in Document ID order and an Index range in Index order, but callers must not rely on either. Promising Document ID order forces an Index plan to collect every candidate before returning the first one, because an Index orders its entries by value first. That buffer grows with the number of matches, which is what streaming exists to avoid. Order becomes a promise only when a query asks for a sort (v0.8).

## Considered Options

- **Keep Document ID order and buffer candidate keys** (v0.1 to v0.4.3): simple, but memory and time to the first result grow with the number of matches.
- **Buffer up to a cap, then fall back to a full Collection scan**: bounded and still ordered, but wastes the Index read whenever the cap is hit and keeps a promise no client asked for.
- **Merge the single-value Index ranges by Document ID**: ordered with no buffer for equality, but a `$gt`/`$lt` range still needs the buffer.

## Consequences

- Without `$or`, an Index plan streams one range in chunks, with no buffer and no duplicates (Field Types are scalar, so a Document has at most one entry per Index).
- `$or` branches can overlap, so a Document can be found under several. It is returned under the first branch whose ranges contain it: a candidate from branch *i* is dropped if it lies in any range of branches 0..*i*-1. This needs no memory, unlike a set of returned IDs (MongoDB's way). Work per Document grows with the branch count; the fix for huge `$or`s is on the roadmap.
- Tests that compare an Index plan against a full scan compare sorted Document IDs.
- v0.8's sort is a step on top of the stream: it walks an Index when one gives the requested order, and sorts what the stream returns otherwise.
