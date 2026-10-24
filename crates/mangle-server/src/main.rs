use std::fs;
use std::sync::Arc;
use std::sync::RwLock;

use mangle_proto::{Command, LoadProgramCommand, MutateCommand};
use mangle_raft::Engine;
use mangle_server::app::build_app;
use mangle_server::config::ServerConfig;
use mangle_server::fsm::{FsmStore, mutations_to_ops};
use mangle_server::mutations::MutationLog;
use mangle_server::store::{AppState, ProgramStore};
use mangle_wal::RaftLog;

#[tokio::main]
async fn main() {
    let config = match ServerConfig::from_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error loading config: {}", e);
            std::process::exit(1);
        }
    };

    let mut program_store = ProgramStore::new();
    if let Some(ref dir) = config.programs_dir {
        program_store = program_store.with_programs_dir(dir.clone());
    }
    if let Some(ref dir) = config.edb_dir {
        program_store = program_store.with_edb_dir(dir.clone());
    }
    if let Some(ref dir) = config.idb_cache_dir {
        program_store = program_store.with_idb_cache_dir(dir.clone());
    }

    // In replicated (raft) mode the Raft log + snapshots provide
    // durability; the legacy MutationLog is disabled and migrated once
    // (see below).
    if config.raft_dir.is_none()
        && let Some(ref edb_dir) = config.edb_dir
        && !config.persist_edb.is_empty()
    {
        let log = MutationLog::new(edb_dir.clone(), config.persist_edb.clone());
        program_store = program_store.with_mutation_log(log);
    }
    let state: AppState = Arc::new(RwLock::new(program_store));

    // --- Replicated (raft) mode -----------------------------------------
    //
    // Startup: restore the FSM from the snapshot on disk (if any), open
    // the WAL, start the Raft engine (which replays committed-but-
    // unapplied log entries), then either bootstrap a fresh cluster
    // (importing programs_dir and migrating any legacy mutation logs as
    // commands) or wait for the node to become leader.
    let engine = match config.raft_dir {
        Some(ref raft_dir) => {
            match open_engine(state.clone(), &config, raft_dir).await {
                Ok(engine) => Some(Arc::new(engine)),
                Err(e) => {
                    eprintln!("Error opening raft node: {}", e);
                    std::process::exit(1);
                }
            }
        }
        None => {
            // Pre-replication mode: load .mg files from programs_dir.
            load_programs_dir(state.clone(), &config);
            None
        }
    };

    let app = build_app(state, engine.clone());

    let addr = format!("0.0.0.0:{}", config.port);
    eprintln!("mangle-server listening on {addr}");
    eprintln!("  Connect RPC API: /mangle.MangleService/ (Connect, gRPC, gRPC-Web)");
    if let Some(ref engine) = engine {
        eprintln!(
            "  replicated mode: node {} (raft dir: {})",
            engine.id(),
            config.raft_dir.as_ref().unwrap().display()
        );
    }
    eprintln!("  config: {}", config.config_path.display());
    if let Some(ref dir) = config.programs_dir {
        eprintln!("  programs-dir: {}", dir.display());
    }
    if let Some(ref dir) = config.edb_dir {
        eprintln!("  edb-dir: {}", dir.display());
    }
    if let Some(ref dir) = config.idb_cache_dir {
        eprintln!("  idb-cache-dir: {}", dir.display());
    }
    if !config.persist_edb.is_empty() {
        eprintln!("  persist-edb: {:?}", config.persist_edb);
    }
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(engine))
        .await
        .unwrap();
}

/// Open the raft node: FSM restore → WAL → engine → bootstrap or replay.
async fn open_engine(
    state: AppState,
    config: &ServerConfig,
    raft_dir: &std::path::Path,
) -> anyhow::Result<Engine> {
    let fsm = FsmStore::new(state.clone(), raft_dir);
    let restored = fsm.restore()?;
    if restored {
        eprintln!(
            "[raft] restored state from snapshot in {}",
            raft_dir.display()
        );
    }

    let log = RaftLog::open(raft_dir.join("raft.redb"))?;
    let (first, last) = log.log_range()?;
    let fresh = first.is_none() && !restored;

    let engine = Engine::new(
        config.node_id,
        Engine::raft_config(8192),
        log,
        fsm,
    )
    .await?;

    if fresh {
        if !config.raft_bootstrap {
            anyhow::bail!(
                "raft dir {} has no cluster state; set config_schema.raft_bootstrap(1) once \
                 to bootstrap a new cluster",
                raft_dir.display()
            );
        }
        let addr = format!("127.0.0.1:{}", config.port);
        engine.bootstrap_single(&addr).await?;
        wait_for_leader(&engine).await?;
        eprintln!("[raft] bootstrapped single-node cluster (node {})", config.node_id);

        // Import programs_dir as LoadProgram commands: in replicated mode
        // the registry lives in the log, and programs_dir is consulted
        // exactly once, here (DISTRIBUTED_PLAN.md §3.1).
        import_programs_dir(&engine, config).await?;
        // Migrate any legacy MutationLog data as Mutate commands.
        migrate_mutation_logs(&engine, config).await?;
    } else {
        if config.raft_bootstrap {
            anyhow::bail!(
                "raft dir {} already holds cluster state; remove raft_bootstrap(1) from \
                 the config",
                raft_dir.display()
            );
        }
        eprintln!(
            "[raft] log entries: first={:?} last={:?}; waiting for leader",
            first, last
        );
        wait_for_leader(&engine).await?;
    }
    Ok(engine)
}

async fn wait_for_leader(engine: &Engine) -> anyhow::Result<()> {
    // A single-node cluster elects itself quickly; poll metrics for a
    // bounded time rather than fail on the first check.
    for _ in 0..600 {
        if engine.is_leader().await {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::bail!("node did not become leader within 30s")
}

/// Propose each `{programs_dir}/{name}.mg` as a LoadProgram command.
async fn import_programs_dir(engine: &Engine, config: &ServerConfig) -> anyhow::Result<()> {
    let Some(ref dir) = config.programs_dir else {
        return Ok(());
    };
    if !dir.is_dir() {
        eprintln!("Warning: programs-dir {:?} is not a directory", dir);
        return Ok(());
    }
    let mut entries: Vec<_> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "mg"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let source = fs::read_to_string(&path)?;
        let cmd = Command::LoadProgram(LoadProgramCommand { name, source });
        match engine.write(cmd).await {
            Ok((_, Ok(_))) => eprintln!("[raft] imported program {}", path.display()),
            Ok((_, Err(e))) => eprintln!("[raft] failed to import {}: {}", path.display(), e),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// One-time migration of the retired MutationLog format (DISTRIBUTED_PLAN
/// §Phase 1): each `{edb_dir}/{program}/mutations.mg` becomes one
/// MutateCommand; the file is then renamed so the file EDB source stops
/// reading it (the replicated ledger supersedes it).
async fn migrate_mutation_logs(engine: &Engine, config: &ServerConfig) -> anyhow::Result<()> {
    let Some(ref edb_dir) = config.edb_dir else {
        return Ok(());
    };
    for program in &config.persist_edb {
        let path = edb_dir.join(program).join("mutations.mg");
        if !path.is_file() {
            continue;
        }
        let source = fs::read_to_string(&path)?;
        let ops = mutations_to_ops(&source)?;
        if ops.is_empty() {
            fs::rename(&path, path.with_extension("mg.migrated"))?;
            continue;
        }
        let cmd = Command::Mutate(MutateCommand {
            program: program.clone(),
            ops,
        });
        match engine.write(cmd).await {
            Ok((_, Ok(_))) => {}
            Ok((_, Err(e))) => {
                return Err(anyhow::anyhow!(
                    "mutation log migration failed for {}: {}",
                    path.display(),
                    e
                ));
            }
            Err(e) => return Err(e),
        }
        fs::rename(&path, path.with_extension("mg.migrated"))?;
        eprintln!(
            "[raft] migrated mutation log {} into the raft log",
            path.display()
        );
    }
    Ok(())
}

/// Pre-replication mode: load .mg files directly from programs_dir.
fn load_programs_dir(state: AppState, config: &ServerConfig) {
    let Some(ref dir) = config.programs_dir else {
        return;
    };
    if !dir.is_dir() {
        eprintln!("Warning: programs-dir {:?} is not a directory", dir);
        return;
    }
    let mut entries: Vec<_> = fs::read_dir(dir)
        .expect("cannot read programs directory")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "mg"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let source = fs::read_to_string(&path).expect("cannot read program file");
        let mut store = state.write().unwrap();
        match store.load(&name, &source) {
            Ok(info) => {
                eprintln!(
                    "Loaded program '{}' with predicates: {:?}",
                    info.name, info.predicates
                );
            }
            Err(e) => {
                eprintln!("Failed to load '{}': {}", name, e);
            }
        }
    }
}

async fn shutdown_signal(engine: Option<Arc<Engine>>) {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => eprintln!("received SIGTERM, shutting down"),
        _ = sigint.recv() => eprintln!("received SIGINT, shutting down"),
    }
    if let Some(engine) = engine
        && let Err(e) = engine.shutdown().await
    {
        eprintln!("raft shutdown error: {}", e);
    }
}
