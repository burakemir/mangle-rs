// Copyright 2024 Google LLC
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

//! # Mangle FactStore
//!
//! Defines core storage interfaces (`Store`, `Host`) and a legacy in-memory storage implementation.

use anyhow::{Result, anyhow};
use ast::Arena;
use mangle_ast as ast;

mod tablestore;
pub use tablestore::{TableConfig, TableStoreImpl, TableStoreSchema};

// --- New Interfaces (Moved from interpreter/vm to break cycles) ---

#[cfg(feature = "edge")]
#[derive(Debug, Clone)]
pub enum Value {
    Number(i64),
    Float(f64),
    String(String),
    Null, // Used for iteration end or missing
}

#[cfg(feature = "edge")]
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Number(a), Value::Number(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Null, Value::Null) => true,
            _ => false,
        }
    }
}

#[cfg(feature = "edge")]
impl Eq for Value {}

#[cfg(feature = "edge")]
impl std::hash::Hash for Value {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Value::Number(n) => n.hash(state),
            Value::Float(f) => f.to_bits().hash(state),
            Value::String(s) => s.hash(state),
            Value::Null => {}
        }
    }
}

#[cfg(feature = "edge")]
impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(feature = "edge")]
impl Ord for Value {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Value::Number(a), Value::Number(b)) => a.cmp(b),
            (Value::Float(a), Value::Float(b)) => a.total_cmp(b),
            // Cross-numeric: promote integer to float for comparison
            (Value::Number(a), Value::Float(b)) => (*a as f64).total_cmp(b),
            (Value::Float(a), Value::Number(b)) => a.total_cmp(&(*b as f64)),
            (Value::String(a), Value::String(b)) => a.cmp(b),
            (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
            // Cross-variant ordering for non-numeric pairs: Number/Float < String < Null
            (Value::Number(_) | Value::Float(_), _) => std::cmp::Ordering::Less,
            (_, Value::Number(_) | Value::Float(_)) => std::cmp::Ordering::Greater,
            (Value::String(_), _) => std::cmp::Ordering::Less,
            (_, Value::String(_)) => std::cmp::Ordering::Greater,
        }
    }
}

#[cfg(feature = "edge")]
impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Number(n) => write!(f, "{n}"),
            Value::Float(v) => write!(f, "{v}"),
            Value::String(s) => write!(f, "{s:?}"),
            Value::Null => write!(f, "null"),
        }
    }
}

/// Abstract interface for relation storage (Edge Mode).
#[cfg(feature = "edge")]
pub trait Store {
    /// Returns an iterator over all tuples in the relation.
    /// Returns an error if the relation does not exist.
    fn scan(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>>;

    /// Returns an iterator over only the new tuples added in the last iteration.
    fn scan_delta(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>>;

    /// Returns an iterator over tuples being collected for the next iteration.
    fn scan_next_delta(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>>;

    /// Returns an iterator over tuples in the relation matching a key in a column.
    fn scan_index(
        &self,
        relation: &str,
        col_idx: usize,
        key: &Value,
    ) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>>;

    /// Returns an iterator over delta tuples matching a key in a column.
    fn scan_delta_index(
        &self,
        relation: &str,
        col_idx: usize,
        key: &Value,
    ) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>>;

    /// Inserts a tuple into the relation (specifically into the delta/new set).
    /// Returns true if it was new.
    fn insert(&mut self, relation: &str, tuple: Vec<Value>) -> Result<bool>;

    /// Merges current deltas into the stable set of facts.
    fn merge_deltas(&mut self);

    /// Ensures a relation exists in the store.
    fn create_relation(&mut self, relation: &str);

    /// Removes a specific tuple from the relation's stable set.
    /// Returns true if the tuple was found and removed.
    fn retract(&mut self, relation: &str, tuple: &[Value]) -> Result<bool>;

    /// Removes all tuples from a relation (stable, delta, and next_delta).
    fn clear(&mut self, relation: &str);

    /// Returns the names of all relations in the store.
    fn relation_names(&self) -> Vec<String>;
}

/// Trait for the host environment that provides storage and data access (Server Mode).
#[cfg(feature = "server")]
pub trait Host {
    fn scan_start(&mut self, rel_id: i32) -> i32;
    fn scan_delta_start(&mut self, rel_id: i32) -> i32;
    fn scan_index_start(&mut self, rel_id: i32, col_idx: i32, val: i64) -> i32;
    fn scan_aggregate_start(&mut self, rel_id: i32, description: Vec<i32>) -> i32;
    fn scan_next(&mut self, iter_id: i32) -> i32;
    fn get_col(&mut self, tuple_ptr: i32, col_idx: i32) -> i64;
    fn insert(&mut self, rel_id: i32, val: i64);
    /// Merges deltas and returns 1 if changes occurred, 0 otherwise.
    fn merge_deltas(&mut self) -> i32;
    fn debuglog(&mut self, val: i64);
}

// --- Legacy Interfaces ---

pub trait Receiver<'a> {
    fn next(&self, item: &'a ast::Atom<'a>) -> Result<()>;
}

impl<'a, Closure: Fn(&'a ast::Atom<'a>) -> Result<()>> Receiver<'a> for Closure {
    fn next(&self, item: &'a ast::Atom<'a>) -> Result<()> {
        (*self)(item)
    }
}

/// Lifetime 'a is used for data held by this store.
pub trait ReadOnlyFactStore<'a> {
    fn arena(&'a self) -> &'a Arena;

    fn contains<'src>(&'a self, src: &'src Arena, fact: &'src ast::Atom<'src>) -> Result<bool>;

    // Sends atoms that matches query `Atom{ sym: query_sym, args: query_args}`.
    // pub sym: PredicateIndex,
    fn get<'query, R: Receiver<'a>>(
        &'a self,
        query_sym: ast::PredicateIndex,
        query_args: &'query [&'query ast::BaseTerm<'query>],
        cb: &R,
    ) -> Result<()>;

    // Invokes cb for every predicate available in this store.
    // It would be nice to use `impl Iterator` here.
    fn predicates(&'a self) -> Vec<ast::PredicateIndex>;

    // Returns approximae number of facts.
    fn estimate_fact_count(&self) -> u32;
}

/// A fact store that can be mutated.
/// Implementations must make use of interior mutability.
pub trait FactStore<'a>: ReadOnlyFactStore<'a> {
    /// Returns true if fact did not exist before.
    /// The fact is copied.
    fn add<'src>(&'a self, src: &'src Arena, fact: &'src ast::Atom<'src>) -> Result<bool>;

    /// Adds all facts from given store.
    fn merge<'src, S>(&'a self, src: &'src Arena, store: &'src S)
    where
        S: ReadOnlyFactStore<'src>;
}

/// Invokes cb for every fact in the store.
pub fn get_all_facts<'a, S, R: Receiver<'a>>(store: &'a S, cb: &R) -> Result<()>
where
    S: ReadOnlyFactStore<'a> + 'a,
{
    let arena = Arena::new_with_global_interner();
    let preds = store.predicates();

    for pred in preds {
        arena.copy_predicate_sym(store.arena(), pred);
        store.get(pred, arena.new_query(pred).args, cb)?;
    }
    Ok(())
}
