# Clock and random seams, deterministic simulator

Status: resolved
Blocked by: 01, 02, 03, 04, 05, 09

Spec: [Seams and simulator](../spec.md#seams-and-simulator).

- Inject Document ID and Catalog ID sources through `Node::open_with` as an `IdGenerator` struct.
- Add `GcPass::step()` behind `testing`; `collect_garbage()` loops over it.
- Proptest-driven single-threaded simulator in `tests/sim.rs` over `SimDisk`, interleaving Transactions, build chunks, GC steps and crashes, checking the spec's invariants after each step.
- The real-thread stress test already exists (`concurrent_threads_stress_test`).
- Update `ARCHITECTURE.md` for the changed flows and invariants.
