# Clock and random seams, deterministic simulator

Status: ready-for-agent
Blocked by: 01, 02, 03, 04, 05

Spec: [Seams and simulator](../spec.md#seams-and-simulator).

- Inject the clock (UUIDv7) and random source (Catalog IDs) through `Node::open_with`.
- A seeded single-threaded scheduler over `SimDisk` interleaving Transactions, build chunks and Garbage Collection passes, checking the spec's invariants after each step.
- Update `ARCHITECTURE.md` for the changed flows and invariants.
