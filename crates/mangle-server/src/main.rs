use mangle_server::app::build_app;
use mangle_server::config::ServerConfig;
use mangle_server::handlers::AppState;
use mangle_server::mutations::MutationLog;
use mangle_server::store::ProgramStore;
use std::fs;
use std::sync::Arc;
use std::sync::RwLock;

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
    if let Some(ref edb_dir) = config.edb_dir {
        if !config.persist_edb.is_empty() {
            let log = MutationLog::new(edb_dir.clone(), config.persist_edb.clone());
            program_store = program_store.with_mutation_log(log);
        }
    }
    let state: AppState = Arc::new(RwLock::new(program_store));

    // Load .mg files from programs directory if specified
    if let Some(ref dir) = config.programs_dir {
        if dir.is_dir() {
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
        } else {
            eprintln!("Warning: programs-dir {:?} is not a directory", dir);
        }
    }

    let app = build_app(state);

    let addr = format!("0.0.0.0:{}", config.port);
    eprintln!("mangle-server listening on {addr}");
    eprintln!("  Connect RPC API: /mangle.MangleService/ (Connect, gRPC, gRPC-Web)");
    eprintln!("  deprecated JSON HTTP API: /query, /programs, /eval");
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
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => eprintln!("received SIGTERM, shutting down"),
        _ = sigint.recv() => eprintln!("received SIGINT, shutting down"),
    }
}
