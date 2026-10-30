# Crash test harness

Status: ready-for-agent

Blocked by: 05, 06

Spec: [../spec.md](../spec.md)

## What

- **Simulated crashes** (proptest): run a random workload of inserts, replaces, deletes and DDL over `FaultyLogFile`, crash at a random point (optionally tearing the last write), reopen over a fresh `MemoryEngine`, and check the recovered state equals a model containing exactly the acknowledged commits.
- **Failure injection**: fail an append or sync mid-workload; check the `Database` is failed, then that reopening recovers either with or without that commit, never part of it.
- **Real process**: a test that spawns a child process committing in a loop and printing each acknowledged timestamp, sends it `SIGKILL` at a random moment, reopens the directory, and checks every printed timestamp survived.

## Done when

All three run in `cargo test` (the real-process test may be `#[ignore]`d by default if slow, with a note in the crate README on how to run it).
