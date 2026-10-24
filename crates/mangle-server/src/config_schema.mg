Package config_schema !

Decl server_port(Port)
  descr [doc("TCP port for the HTTP server")]
  bound [/number].

Decl programs_dir(Path)
  descr [doc("Directory containing .mg program files to load at startup")]
  bound [/string].

Decl edb_dir(Path)
  descr [doc("Base directory for EDB data storage (per-program subdirectories)")]
  bound [/string].

Decl idb_cache_dir(Path)
  descr [doc("Directory for caching IDB computation results")]
  bound [/string].

Decl persist_edb(ProgramName)
  descr [doc("Programs whose API-inserted EDB facts are persisted to disk")]
  bound [/string].

Decl raft_dir(Path)
  descr [doc("Raft data directory (WAL + snapshots). Setting this enables replicated (Raft) mode")]
  bound [/string].

Decl node_id(Id)
  descr [doc("Stable Raft node id. Must not change across restarts. Default 1")]
  bound [/number].

Decl raft_bootstrap(Flag)
  descr [doc("1 = bootstrap a new single-node cluster (exactly once, on a fresh raft_dir)")]
  bound [/number].
