mod config;
mod handlers;
mod mutations;
mod query;
mod store;

use axum::Router;
use axum::routing::{get, post};
use handlers::{
    AppState, delete_program_handler, eval_handler, get_program_handler, insert_handler,
    list_programs_handler, load_program_handler, query_handler, reload_all_handler,
    reload_program_handler, retract_handler,
};
use mutations::MutationLog;
use std::fs;
use std::sync::{Arc, RwLock};
use store::ProgramStore;

#[tokio::main]
async fn main() {
    let config = match config::ServerConfig::from_args() {
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

    let app = Router::new()
        .route("/query", post(query_handler))
        .route(
            "/programs",
            get(list_programs_handler).post(load_program_handler),
        )
        .route(
            "/programs/{name}",
            get(get_program_handler).delete(delete_program_handler),
        )
        .route("/programs/{name}/reload", post(reload_program_handler))
        .route("/programs/{name}/insert", post(insert_handler))
        .route("/programs/{name}/retract", post(retract_handler))
        .route("/admin/reload-all", post(reload_all_handler))
        .route("/eval", post(eval_handler))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", config.port);
    eprintln!("mangle-server listening on {addr}");
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
