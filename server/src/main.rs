mod handlers;
mod query;
mod store;

use axum::Router;
use axum::routing::{get, post};
use handlers::{
    AppState, delete_program_handler, eval_handler, get_program_handler, insert_handler,
    list_programs_handler, load_program_handler, query_handler, reload_all_handler,
    reload_program_handler, retract_handler,
};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use store::ProgramStore;

struct ServerConfig {
    port: u16,
    programs_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    edb_dir: Option<PathBuf>,
    idb_cache_dir: Option<PathBuf>,
}

fn parse_args() -> ServerConfig {
    let args: Vec<String> = std::env::args().collect();
    let mut config = ServerConfig {
        port: 8090,
        programs_dir: None,
        data_dir: None,
        edb_dir: None,
        idb_cache_dir: None,
    };

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                if i < args.len() {
                    config.port = args[i].parse().expect("invalid port number");
                }
            }
            "--programs-dir" => {
                i += 1;
                if i < args.len() {
                    config.programs_dir = Some(PathBuf::from(&args[i]));
                }
            }
            "--data-dir" => {
                i += 1;
                if i < args.len() {
                    config.data_dir = Some(PathBuf::from(&args[i]));
                }
            }
            "--edb-dir" => {
                i += 1;
                if i < args.len() {
                    config.edb_dir = Some(PathBuf::from(&args[i]));
                }
            }
            "--idb-cache-dir" => {
                i += 1;
                if i < args.len() {
                    config.idb_cache_dir = Some(PathBuf::from(&args[i]));
                }
            }
            _ => {
                eprintln!("Unknown argument: {}", args[i]);
            }
        }
        i += 1;
    }

    // Derive defaults from data_dir
    if let Some(ref data_dir) = config.data_dir {
        if config.edb_dir.is_none() {
            config.edb_dir = Some(data_dir.join("edb"));
        }
        if config.idb_cache_dir.is_none() {
            config.idb_cache_dir = Some(data_dir.join("idb-cache"));
        }
    }

    config
}

#[tokio::main]
async fn main() {
    let config = parse_args();

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
    let state: AppState = Arc::new(RwLock::new(program_store));

    // Load .mg files from programs directory if specified
    if let Some(ref dir) = config.programs_dir {
        if dir.is_dir() {
            let mut entries: Vec<_> = fs::read_dir(dir)
                .expect("cannot read programs directory")
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.path()
                        .extension()
                        .is_some_and(|ext| ext == "mg")
                })
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
            eprintln!("Warning: --programs-dir {:?} is not a directory", dir);
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
    if let Some(ref dir) = config.programs_dir {
        eprintln!("  programs-dir: {}", dir.display());
    }
    if let Some(ref dir) = config.edb_dir {
        eprintln!("  edb-dir: {}", dir.display());
    }
    if let Some(ref dir) = config.idb_cache_dir {
        eprintln!("  idb-cache-dir: {}", dir.display());
    }
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => eprintln!("received SIGTERM, shutting down"),
        _ = sigint.recv() => eprintln!("received SIGINT, shutting down"),
    }
}
