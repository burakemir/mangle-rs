mod handlers;
mod query;
mod store;

use axum::Router;
use axum::routing::{get, post};
use handlers::{AppState, eval_handler, list_programs_handler, load_program_handler, query_handler};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use store::ProgramStore;

fn parse_args() -> (u16, Option<PathBuf>) {
    let args: Vec<String> = std::env::args().collect();
    let mut port: u16 = 8090;
    let mut programs_dir: Option<PathBuf> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                if i < args.len() {
                    port = args[i].parse().expect("invalid port number");
                }
            }
            "--programs-dir" => {
                i += 1;
                if i < args.len() {
                    programs_dir = Some(PathBuf::from(&args[i]));
                }
            }
            _ => {
                eprintln!("Unknown argument: {}", args[i]);
            }
        }
        i += 1;
    }

    (port, programs_dir)
}

#[tokio::main]
async fn main() {
    let (port, programs_dir) = parse_args();

    let program_store = ProgramStore::new();
    let state: AppState = Arc::new(RwLock::new(program_store));

    // Load .mg files from programs directory if specified
    if let Some(dir) = programs_dir {
        if dir.is_dir() {
            let mut entries: Vec<_> = fs::read_dir(&dir)
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
        .route("/programs", get(list_programs_handler).post(load_program_handler))
        .route("/eval", post(eval_handler))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    eprintln!("mangle-server listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
