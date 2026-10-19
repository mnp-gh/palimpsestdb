# Order-preserving key encoding, newest Version first

The Storage Engine compares keys as raw bytes, so the Document Layer encodes every key so that byte order equals logical order, and appends the inverted Commit Timestamp so the newest Version of a key sorts first. Reading a key at a Snapshot is then one forward seek. Changing this later rewrites every stored key, so the layout is fixed here.

## Layout

- **Logical key**: `database_id (u64 BE) ‖ collection_id (u64 BE) ‖ components…`. For Documents the single component is the encoded Document ID. The Catalog lives in Database 0: Collection 0 holds the ID counter, 1 holds Databases (component: name), 2 holds Collections (components: database ID, name).
- **Physical key**: `logical key ‖ !commit_ts (u64 BE)`. Bitwise NOT makes larger timestamps sort first.
- **Value**: `0x00` for a tombstone (the Document was deleted at that Version), or `0x01 ‖ BSON bytes`.

Every component is self-delimiting, so no logical key is a prefix of another and Versions of different keys never interleave.

## Component encodings

| Type | Tag | Payload |
|---|---|---|
| Integer (BSON int32 and int64, one class) | `0x10` | i64 with the sign bit flipped, big-endian |
| String | `0x20` | UTF-8 with `0x00` escaped as `0x00 0xFF`, terminated by `0x00 0x00` |
| Binary | `0x30` | subtype byte, then bytes escaped and terminated as for String |
| ObjectId | `0x40` | the 12 raw bytes |
| Boolean | `0x50` | `0x00` or `0x01` |
| DateTime | `0x60` | milliseconds since epoch as for Integer |

Tags follow MongoDB's cross-type order (numbers < strings < binary < ObjectId < booleans < dates).

## Consequences

- `{_id: 1}` as int32 and as int64 is the same Document.
- **Narrows Q14** ("any BSON scalar"): doubles and Decimal128 are not accepted as Document IDs in v0.1, because a single byte order for mixed integer and floating-point values (including NaN and -0.0) is a project of its own. Regex, Timestamp, Symbol, JavaScript, MinKey and MaxKey are not accepted either. These can be added later with new tags without disturbing existing keys.
