use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use mangle_common::Value;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};

use crate::store::{ProgramStore, eval_source};

// --- Insert / Retract request types ---

#[derive(Deserialize)]
pub struct InsertRequest {
    pub relation: String,
    pub tuple: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct RetractRequest {
    pub relation: String,
    pub tuple: Vec<serde_json::Value>,
}

fn json_to_values(arr: &[serde_json::Value]) -> Vec<Value> {
    arr.iter()
        .map(|v| match v {
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Number(i)
                } else if let Some(f) = n.as_f64() {
                    Value::Float(f)
                } else {
                    Value::Number(0)
                }
            }
            serde_json::Value::String(s) => Value::String(s.clone()),
            serde_json::Value::Null => Value::Null,
            _ => Value::String(v.to_string()),
        })
        .collect()
}

pub type AppState = Arc<RwLock<ProgramStore>>;

// --- Request / Response types ---

#[derive(Deserialize)]
pub struct QueryRequest {
    pub program: String,
    pub query: String,
}

#[derive(Deserialize)]
pub struct LoadProgramRequest {
    pub name: String,
    pub source: String,
}

#[derive(Deserialize)]
pub struct EvalRequest {
    pub source: String,
    pub query: Option<String>,
}

#[derive(Serialize)]
pub struct QueryResponse {
    pub results: Vec<Vec<serde_json::Value>>,
}

#[derive(Serialize)]
pub struct ProgramResponse {
    pub name: String,
    pub predicates: Vec<String>,
}

#[derive(Serialize)]
pub struct ProgramDetailResponse {
    pub name: String,
    pub source: String,
    pub predicates: Vec<String>,
}

#[derive(Serialize)]
pub struct ProgramsListResponse {
    pub programs: Vec<ProgramResponse>,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

// --- Value → JSON conversion ---

fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Number(n) => serde_json::Value::Number((*n).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Name(s) => serde_json::Value::String(s.clone()),
        Value::Time(t) => serde_json::Value::String(format!("{}", Value::Time(*t))),
        Value::Duration(d) => serde_json::Value::String(format!("{}", Value::Duration(*d))),
        Value::Compound(_, elems) => {
            serde_json::Value::Array(elems.iter().map(value_to_json).collect())
        }
        Value::Null => serde_json::Value::Null,
    }
}

fn tuples_to_json(tuples: Vec<Vec<Value>>) -> Vec<Vec<serde_json::Value>> {
    tuples
        .iter()
        .map(|tuple| tuple.iter().map(value_to_json).collect())
        .collect()
}

// --- Handlers ---

pub async fn query_handler(
    State(state): State<AppState>,
    Json(req): Json<QueryRequest>,
) -> impl IntoResponse {
    let store = state.read().unwrap();
    match store.execute_query(&req.program, &req.query) {
        Ok(tuples) => (
            StatusCode::OK,
            Json(serde_json::json!(QueryResponse {
                results: tuples_to_json(tuples),
            })),
        ),
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (
                status,
                Json(serde_json::json!(ErrorResponse { error: msg })),
            )
        }
    }
}

pub async fn list_programs_handler(State(state): State<AppState>) -> impl IntoResponse {
    let store = state.read().unwrap();
    let programs: Vec<ProgramResponse> = store
        .list()
        .into_iter()
        .map(|p| ProgramResponse {
            name: p.name,
            predicates: p.predicates,
        })
        .collect();
    Json(serde_json::json!(ProgramsListResponse { programs }))
}

pub async fn load_program_handler(
    State(state): State<AppState>,
    Json(req): Json<LoadProgramRequest>,
) -> impl IntoResponse {
    let mut store = state.write().unwrap();
    match store.load(&req.name, &req.source) {
        Ok(info) => (
            StatusCode::OK,
            Json(serde_json::json!(ProgramResponse {
                name: info.name,
                predicates: info.predicates,
            })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!(ErrorResponse {
                error: e.to_string(),
            })),
        ),
    }
}

pub async fn eval_handler(Json(req): Json<EvalRequest>) -> impl IntoResponse {
    match eval_source(&req.source, req.query.as_deref()) {
        Ok(tuples) => (
            StatusCode::OK,
            Json(serde_json::json!(QueryResponse {
                results: tuples_to_json(tuples),
            })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!(ErrorResponse {
                error: e.to_string(),
            })),
        ),
    }
}

// --- Admin handlers ---

/// GET /programs/{name} — get program details (source + predicates).
pub async fn get_program_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let store = state.read().unwrap();
    match store.get(&name) {
        Some(prog) => (
            StatusCode::OK,
            Json(serde_json::json!(ProgramDetailResponse {
                name: name.clone(),
                source: prog.source.clone(),
                predicates: prog.predicates.clone(),
            })),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!(ErrorResponse {
                error: format!("program '{}' not found", name),
            })),
        ),
    }
}

/// DELETE /programs/{name} — remove a program from memory.
pub async fn delete_program_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let mut store = state.write().unwrap();
    if store.remove(&name) {
        (
            StatusCode::OK,
            Json(serde_json::json!(MessageResponse {
                message: format!("program '{}' removed", name),
            })),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!(ErrorResponse {
                error: format!("program '{}' not found", name),
            })),
        )
    }
}

/// POST /programs/{name}/reload — reload a program from disk.
pub async fn reload_program_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let mut store = state.write().unwrap();
    match store.reload(&name) {
        Ok(info) => (
            StatusCode::OK,
            Json(serde_json::json!(ProgramResponse {
                name: info.name,
                predicates: info.predicates,
            })),
        ),
        Err(e) => {
            let status =
                if e.to_string().contains("not found") || e.to_string().contains("cannot read") {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::BAD_REQUEST
                };
            (
                status,
                Json(serde_json::json!(ErrorResponse {
                    error: e.to_string(),
                })),
            )
        }
    }
}

/// POST /admin/reload-all — reload all programs from programs_dir.
pub async fn reload_all_handler(State(state): State<AppState>) -> impl IntoResponse {
    let mut store = state.write().unwrap();
    match store.reload_all() {
        Ok(loaded) => {
            let programs: Vec<ProgramResponse> = loaded
                .into_iter()
                .map(|p| ProgramResponse {
                    name: p.name,
                    predicates: p.predicates,
                })
                .collect();
            (
                StatusCode::OK,
                Json(serde_json::json!(ProgramsListResponse { programs })),
            )
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!(ErrorResponse {
                error: e.to_string(),
            })),
        ),
    }
}

/// POST /programs/{name}/insert — insert a fact into a program's database.
pub async fn insert_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<InsertRequest>,
) -> impl IntoResponse {
    let store = state.read().unwrap();
    let tuple = json_to_values(&req.tuple);
    match store.insert_fact(&name, &req.relation, tuple) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!(MessageResponse {
                message: format!("inserted into {}.{}", name, req.relation),
            })),
        ),
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (
                status,
                Json(serde_json::json!(ErrorResponse { error: msg })),
            )
        }
    }
}

/// POST /programs/{name}/retract — retract a fact from a program's database.
pub async fn retract_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<RetractRequest>,
) -> impl IntoResponse {
    let store = state.read().unwrap();
    let tuple = json_to_values(&req.tuple);
    match store.retract_fact(&name, &req.relation, &tuple) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!(MessageResponse {
                message: format!("retracted from {}.{}", name, req.relation),
            })),
        ),
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (
                status,
                Json(serde_json::json!(ErrorResponse { error: msg })),
            )
        }
    }
}
