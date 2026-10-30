# Docs housekeeping

Status: ready-for-agent

Spec: [../spec.md](../spec.md)

## What

- `docs/ROADMAP.md`: mark v0.1 Done and v0.2 In progress. Under v0.2, note that the WAL grows without bound until checkpoints exist (D7). Move checkpoints from v0.6 to v0.4, alongside the `fjall` engine (D8); v0.6 keeps group commit.
- `docs/adr/0004-document-layer-owns-wal.md`: replace "group commit and checkpoints in v0.3+" with "checkpoints with the first durable engine (v0.4), group commit with the server (v0.6)".
- `GLOSSARY.md`: add **WAL Record**, **Recovery** and **Commit Outcome Unknown**.

## Done when

The roadmap, ADR 0004 and the glossary agree with the spec's D7 and D8.
