# Order-preserving key encoding, newest Version first

The Storage Engine compares keys as raw bytes, so the Database Layer encodes every key so that byte order equals logical order, and appends the inverted Commit Timestamp so the newest Version of a key sorts first. Reading a key at a Snapshot is then one forward seek. Changing this later rewrites every stored key, so the layout is fixed here.

## Layout

- **Logical key**: `namespace_id (u32 BE) ‖ collection_id (u32 BE) ‖ components…`. For Documents the single component is the encoded Document ID. The Catalog lives in Namespace 0: Collection 1 holds Namespaces (component: name), 2 holds Collections (components: Namespace ID as 4 big-endian bytes, as in every key, then name), 3 holds the ID keys of Namespaces (component: Namespace ID) and 4 those of Collections (components: Namespace ID, then Collection ID). Namespace and Collection ID keys live in separate Collections so that neither is a prefix of the other. Collection 0 holds unversioned Database Layer state: the `applied_up_to` marker (component: the bytes `applied_up_to`; value: a Commit Timestamp as u64 BE, ADR 0004). Namespace and Collection IDs are random 32-bit numbers (ADR 0006), never 0. An Index's entries live inside its Collection's range: `namespace_id ‖ collection_id ‖ 0x00 ‖ index_id (u32 BE) ‖ value component ‖ document ID component`, with the Document ID left out in a unique Index unless the value is null or missing (ADR 0011). `0x00` sorts below every Document ID tag, so a Collection's Index entries all come before its Documents and a scan of its Documents starts at `namespace_id ‖ collection_id ‖ 0x01`. Everything a Collection owns is one key range, so dropping it (and, later, garbage-collecting it) is one range delete. Index IDs are counted per Collection (ADR 0006).
- **Physical key**: `logical key ‖ !commit_ts (u64 BE)`. Bitwise NOT makes larger timestamps sort first.
- **Value**: `0x00` for a tombstone (the Document was deleted at that Version), or `0x01 ‖ BSON bytes`.

Every component is self-delimiting, so no logical key is a prefix of another and Versions of different keys never interleave.

## Component encodings

| Type | Tag | Payload |
|---|---|---|
| Null or missing (Index keys only) | `0x05` | none |
| Integer (BSON int32 and int64, one class) | `0x10` | i64 with the sign bit flipped, big-endian |
| Double (Index keys only, in `double` Indexes) | `0x11` | IEEE 754 bits, big-endian: sign bit set if positive, all bits inverted if negative; -0.0 written as 0.0 |
| String | `0x20` | UTF-8 with `0x00` escaped as `0x00 0xFF`, terminated by `0x00 0x00` |
| Binary (including UUIDs, subtype 4) | `0x30` | subtype byte, then bytes escaped and terminated as for String |
| ObjectId | `0x40` | the 12 raw bytes |
| Boolean | `0x50` | `0x00` or `0x01` |
| DateTime | `0x60` | milliseconds since epoch as for Integer |

Tags follow MongoDB's cross-type order (null < numbers < strings < binary < ObjectId < booleans < dates). One Index holds one type plus null, so integers and doubles never meet in one key range.

## Consequences

- `{_id: 1}` as int32 and as int64 is the same Document.
- **Narrows Q14** ("any BSON scalar"): doubles and Decimal128 are not accepted as Document IDs in v0.1, because a single byte order for integers mixed with doubles and Decimal128 values (including NaN and -0.0) is a project of its own. Regex, Timestamp, Symbol, JavaScript, MinKey and MaxKey are not accepted either. These can be added later with new tags without disturbing existing keys.
