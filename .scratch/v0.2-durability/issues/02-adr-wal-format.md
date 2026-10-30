# ADR 0010: WAL record format and commit failure

Status: ready-for-agent

Spec: [../spec.md](../spec.md)

## What

Write `docs/adr/0010-wal-record-format.md`, in the style of the existing ADRs, recording:

- The record layout from D2, big-endian throughout, and why: one byte-order rule for every integer the project writes, keys and WAL alike.
- Logical keys in records, not physical keys, so the WAL is independent of ADR 0008's Version layout.
- Torn-tail handling: replay stops at the first short or bad-CRC record and the file is truncated there.
- D5: a failed append, sync or apply marks the `Database` failed with `CommitOutcomeUnknown`; fsync is never retried.

Also amend ADR 0008 with the reserved, unversioned `applied_up_to` key in Database 0, Collection 0 (D4).

## Done when

ADR 0010 exists and ADR 0008 mentions the reserved key.
