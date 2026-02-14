use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use mangle_factstore::Value;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};

use crate::store::{ProgramStore, eval_source};

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
pub struct ProgramsListResponse {
    pub programs: Vec<ProgramResponse>,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

// --- Value → JSON conversion ---

fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Number(n) => serde_json::Value::Number((*n).into()),
        Value::String(s) => serde_json::Value::String(s.clone()),
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
                Json(serde_json::json!(ErrorResponse {
                    error: msg,
                })),
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
