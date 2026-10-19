# Own wire protocol, not MongoDB-compatible

Clients talk to PalimpsestDB over its own minimal protocol: length-prefixed frames carrying BSON request/response messages, with a first-party Rust client and CLI. MongoDB wire compatibility would give free tooling (mongosh, drivers) but would bind us to Mongo's command set, error codes and SI-only transaction model, which conflicts with offering Serializable. A Mongo-compatible adapter may be added later as a separate front end.
