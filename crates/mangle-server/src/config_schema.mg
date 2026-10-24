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
  descr [doc("1 = bootstrap a new cluster (exactly once, on a fresh raft_dir)")]
  bound [/number].

Decl raft_addr(Addr)
  descr [doc("Address other nodes use to reach this node. Default 127.0.0.1:{server_port}")]
  bound [/string].

Decl raft_bootstrap_node(Id, Addr)
  descr [doc("Additional member of the cluster being bootstrapped (with raft_bootstrap(1)); all bootstrap nodes declare the same set")]
  bound [/number, /string].

Decl raft_join(Addr)
  descr [doc("Address of an existing cluster node to join through (fresh raft_dir, no raft_bootstrap)")]
  bound [/string].
