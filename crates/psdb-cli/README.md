# psdb-cli

`psdb`: an interactive shell over an embedded PalimpsestDB. Values are MongoDB relaxed Extended JSON. With `--data-dir <path>`, every commit is durable in a WAL in that directory; without it, all data is lost on exit.

```
$ cargo run -p psdb-cli -- --data-dir ./data
psdb> create-db shop
psdb> create-coll shop.orders
psdb> insert shop.orders {"_id": 1, "item": "book", "qty": 2}
psdb> find shop.orders {"qty": {"$gt": 1}}
psdb> begin
psdb(txn)> delete shop.orders 1
psdb(txn)> abort
```

Type `help` for every command.

**Depends on:** `psdb-core`, `psdb-storage-memory`. This is where the Storage Engine is chosen.
