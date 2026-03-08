// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Browser WASM target for the Mangle interpreter.
//!
//! Compiles the Mangle edge-mode interpreter to `wasm32-unknown-unknown` for
//! use in browsers. Provides two usage modes:
//!
//! ## Dynamic mode
//!
//! Supply both the program and initial facts at runtime:
//!
//! ```js
//! const result = run_mangle(
//!   'edge(1,2). edge(2,3). reachable(Y) :- edge(1,Y). reachable(Z) :- reachable(Y), edge(Y,Z).',
//!   '{}'
//! );
//! ```
//!
//! ## Bundled mode (partial evaluation)
//!
//! Bake a Mangle program into the WASM at compile time via the
//! `MANGLE_PROGRAM` environment variable, then call `run_bundled` with only
//! the initial facts:
//!
//! ```sh
//! MANGLE_PROGRAM='reachable(Y) :- edge(1,Y). reachable(Z) :- reachable(Y), edge(Y,Z).' \
//!   wasm-pack build --target web crates/mangle-wasm
//! ```
//!
//! ```js
//! const result = run_bundled('{"edge": [[1,2],[2,3],[3,4]]}');
//! ```

use mangle_ast::Arena;
use mangle_common::{Store, Value};
use mangle_driver::{compile, execute};
use mangle_interpreter::MemStore;
use serde_json;
use wasm_bindgen::prelude::*;

/// Run a Mangle program with optional initial facts (JSON).
///
/// Returns a JSON object mapping relation names to arrays of tuples.
///
/// # Facts format
///
/// ```json
/// {
///   "edge": [[1, 2], [2, 3]],
///   "name": [["alice"], ["bob"]]
/// }
/// ```
///
/// Values can be: integers, floats, strings. Omit or pass `"{}"` for no
/// initial facts.
#[wasm_bindgen]
pub fn run_mangle(source: &str, facts_json: &str) -> Result<String, JsError> {
    let arena = Arena::new_with_global_interner();
    let (mut ir, stratified) =
        compile(source, &arena).map_err(|e| JsError::new(&e.to_string()))?;

    let mut store = MemStore::new();
    load_facts(&mut store, facts_json)?;
    // Move inserted facts from next_delta → delta → stable so they're
    // visible to scan() during execution.
    store.merge_deltas();
    store.merge_deltas();

    let interpreter =
        execute(&mut ir, &stratified, Box::new(store)).map_err(|e| JsError::new(&e.to_string()))?;

    dump_results(interpreter.store())
}

/// Run the compile-time bundled program with initial facts.
///
/// The Mangle source is baked in via `MANGLE_PROGRAM` env var at build time.
/// This enables "partial evaluation": the program is fixed, only the data
/// varies at runtime.
#[wasm_bindgen]
pub fn run_bundled(facts_json: &str) -> Result<String, JsError> {
    const PROGRAM: &str = match option_env!("MANGLE_PROGRAM") {
        Some(s) => s,
        None => "",
    };
    if PROGRAM.is_empty() {
        return Err(JsError::new(
            "No bundled program. Rebuild with MANGLE_PROGRAM env var set.",
        ));
    }
    run_mangle(PROGRAM, facts_json)
}

/// Parse JSON facts and insert them into the store.
fn load_facts(store: &mut MemStore, json: &str) -> Result<(), JsError> {
    let map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(json).map_err(|e| JsError::new(&e.to_string()))?;

    for (rel, tuples) in &map {
        store.create_relation(rel);
        let rows = tuples
            .as_array()
            .ok_or_else(|| JsError::new(&format!("expected array for relation '{rel}'")))?;
        for row in rows {
            let cols = row
                .as_array()
                .ok_or_else(|| JsError::new(&format!("expected array for tuple in '{rel}'")))?;
            let tuple: Vec<Value> = cols.iter().map(json_to_value).collect();
            store
                .insert(rel, tuple)
                .map_err(|e| JsError::new(&e.to_string()))?;
        }
    }
    Ok(())
}

fn json_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Number(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::Null
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone()),
        _ => Value::Null,
    }
}

fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Number(n) => serde_json::Value::Number((*n).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Time(t) => serde_json::Value::Number((*t).into()),
        Value::Duration(d) => serde_json::Value::Number((*d).into()),
        Value::Compound(_, elems) => {
            serde_json::Value::Array(elems.iter().map(value_to_json).collect())
        }
        Value::Null => serde_json::Value::Null,
    }
}

/// Serialize all relations in the store to JSON.
fn dump_results(store: &dyn Store) -> Result<String, JsError> {
    let mut map = serde_json::Map::new();
    for name in store.relation_names() {
        let tuples: Vec<serde_json::Value> = store
            .scan(&name)
            .map_err(|e| JsError::new(&e.to_string()))?
            .map(|row| serde_json::Value::Array(row.iter().map(value_to_json).collect()))
            .collect();
        map.insert(name, serde_json::Value::Array(tuples));
    }
    serde_json::to_string(&map).map_err(|e| JsError::new(&e.to_string()))
}

// Internal API usable without wasm_bindgen (for native tests and embedding).

/// Run a Mangle program with initial facts, returning results as a JSON string.
pub fn run(source: &str, facts_json: &str) -> anyhow::Result<String> {
    let arena = Arena::new_with_global_interner();
    let (mut ir, stratified) = compile(source, &arena)?;

    let mut store = MemStore::new();
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(facts_json)?;
    for (rel, tuples) in &map {
        store.create_relation(rel);
        if let Some(rows) = tuples.as_array() {
            for row in rows {
                if let Some(cols) = row.as_array() {
                    let tuple: Vec<Value> = cols.iter().map(json_to_value).collect();
                    store.insert(rel, tuple)?;
                }
            }
        }
    }
    // Move inserted facts from next_delta → delta → stable so they're
    // visible to scan() during execution.
    store.merge_deltas();
    store.merge_deltas();

    let interpreter = execute(&mut ir, &stratified, Box::new(store))?;
    let st = interpreter.store();
    let mut result_map = serde_json::Map::new();
    for name in st.relation_names() {
        let tuples: Vec<serde_json::Value> = st
            .scan(&name)?
            .map(|row| serde_json::Value::Array(row.iter().map(value_to_json).collect()))
            .collect();
        result_map.insert(name, serde_json::Value::Array(tuples));
    }
    Ok(serde_json::to_string(&result_map)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_rules() {
        let result = run("p(1). p(2). q(X) :- p(X).", "{}").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let q = parsed["q"].as_array().unwrap();
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn test_with_initial_facts() {
        let result = run(
            "q(X) :- p(X).",
            r#"{"p": [[10], [20], [30]]}"#,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let q = parsed["q"].as_array().unwrap();
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn test_reachability() {
        let result = run(
            r#"
                reachable(Y) :- edge(1, Y).
                reachable(Z) :- reachable(Y), edge(Y, Z).
            "#,
            r#"{"edge": [[1,2],[2,3],[3,4],[4,5]]}"#,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let r = parsed["reachable"].as_array().unwrap();
        let mut values: Vec<i64> = r.iter().map(|t| t[0].as_i64().unwrap()).collect();
        values.sort();
        assert_eq!(values, vec![2, 3, 4, 5]);
    }

    #[test]
    fn test_string_facts() {
        let result = run(
            r#"q(X) :- p(X), X = "hello"."#,
            r#"{"p": [["hello"], ["world"]]}"#,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let q = parsed["q"].as_array().unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0][0].as_str().unwrap(), "hello");
    }

    #[test]
    fn test_inline_facts() {
        let result = run(
            r#"
                edge(1, 2). edge(2, 3). edge(3, 4).
                reachable(Y) :- edge(1, Y).
                reachable(Z) :- reachable(Y), edge(Y, Z).
            "#,
            "{}",
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let r = parsed["reachable"].as_array().unwrap();
        let mut values: Vec<i64> = r.iter().map(|t| t[0].as_i64().unwrap()).collect();
        values.sort();
        assert_eq!(values, vec![2, 3, 4]);
    }
}
