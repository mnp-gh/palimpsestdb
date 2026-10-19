# MVCC lives in the Document Layer, not the Storage Engine

The Storage Engine is a plain ordered key-value store (get, put, delete, ordered range scan, atomic write batch). The Document Layer encodes Versions into keys and owns transactions, conflict detection and garbage collection of old Versions. We chose this so that Storage Engines are truly swappable without changing isolation semantics, at the cost of not using engine-native transactions (e.g. RocksDB's) and paying some key-encoding overhead.
