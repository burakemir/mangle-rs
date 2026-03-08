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

/// Identifies the kind of a compound value. When type information is available
/// (concrete types), the kind is redundant. For `/any` or union-typed columns
/// the kind tag makes the value self-describing.
#[cfg(feature = "edge")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CompoundKind {
    List,
    Pair,
    Map,
    Struct,
}

#[cfg(feature = "edge")]
#[derive(Debug, Clone)]
pub enum Value {
    Number(i64),
    Float(f64),
    String(String),
    /// Time as nanoseconds since Unix epoch (consistent with Go implementation).
    Time(i64),
    /// Duration as nanoseconds (consistent with Go implementation).
    Duration(i64),
    /// A compound value: a flat sequence of values. The `CompoundKind` tag
    /// identifies the interpretation. For lists and pairs, elements are stored
    /// directly. For structs and maps, keys/field-names and values are
    /// interleaved: [k1, v1, k2, v2, ...].
    Compound(CompoundKind, Vec<Value>),
    Null, // Used for iteration end or missing
}

#[cfg(feature = "edge")]
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Number(a), Value::Number(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Time(a), Value::Time(b)) => a == b,
            (Value::Duration(a), Value::Duration(b)) => a == b,
            (Value::Compound(ka, a), Value::Compound(kb, b)) => ka == kb && a == b,
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
            Value::Time(t) => t.hash(state),
            Value::Duration(d) => d.hash(state),
            Value::Compound(k, v) => {
                k.hash(state);
                v.hash(state);
            }
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
            (Value::Time(a), Value::Time(b)) => a.cmp(b),
            (Value::Duration(a), Value::Duration(b)) => a.cmp(b),
            (Value::Compound(ka, a), Value::Compound(kb, b)) => ka.cmp(kb).then_with(|| a.cmp(b)),
            (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
            // Cross-variant ordering: Number/Float < String < Time < Duration < Compound < Null
            (Value::Number(_) | Value::Float(_), _) => std::cmp::Ordering::Less,
            (_, Value::Number(_) | Value::Float(_)) => std::cmp::Ordering::Greater,
            (Value::String(_), _) => std::cmp::Ordering::Less,
            (_, Value::String(_)) => std::cmp::Ordering::Greater,
            (Value::Time(_), _) => std::cmp::Ordering::Less,
            (_, Value::Time(_)) => std::cmp::Ordering::Greater,
            (Value::Duration(_), _) => std::cmp::Ordering::Less,
            (_, Value::Duration(_)) => std::cmp::Ordering::Greater,
            (Value::Compound(..), _) => std::cmp::Ordering::Less,
            (_, Value::Compound(..)) => std::cmp::Ordering::Greater,
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
            Value::Time(nanos) => write!(f, "{}", format_time_nanos(*nanos)),
            Value::Duration(nanos) => write!(f, "{}", format_duration_nanos(*nanos)),
            Value::Compound(kind, elems) => match kind {
                CompoundKind::List | CompoundKind::Pair => {
                    write!(f, "[")?;
                    for (i, e) in elems.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{e}")?;
                    }
                    write!(f, "]")
                }
                CompoundKind::Map => {
                    write!(f, "[")?;
                    for (i, pair) in elems.chunks_exact(2).enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}: {}", pair[0], pair[1])?;
                    }
                    write!(f, "]")
                }
                CompoundKind::Struct => {
                    write!(f, "{{")?;
                    for (i, pair) in elems.chunks_exact(2).enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}: {}", pair[0], pair[1])?;
                    }
                    write!(f, "}}")
                }
            },
            Value::Null => write!(f, "null"),
        }
    }
}

#[cfg(feature = "edge")]
fn format_time_nanos(nanos: i64) -> String {
    let secs = nanos.div_euclid(1_000_000_000);
    let ns = nanos.rem_euclid(1_000_000_000) as u32;

    // Convert seconds since epoch to date/time components
    // Using a simplified algorithm (valid for dates from 1970 onwards)
    let days = secs.div_euclid(86400);
    let time_of_day = secs.rem_euclid(86400);
    let hour = time_of_day / 3600;
    let minute = (time_of_day % 3600) / 60;
    let second = time_of_day % 60;

    // Civil date from days since epoch (algorithm from Howard Hinnant)
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    if ns == 0 {
        format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
    } else {
        // Trim trailing zeros from fractional seconds
        let mut frac = format!("{ns:09}");
        frac = frac.trim_end_matches('0').to_string();
        format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}.{frac}Z")
    }
}

/// Formats a duration in nanoseconds to match Go's time.Duration.String() output.
/// Produces compound forms like "1h30m0s", "2.5s", "500ms", "150ns".
#[cfg(feature = "edge")]
fn format_duration_nanos(nanos: i64) -> String {
    if nanos == 0 {
        return "0s".to_string();
    }

    let mut result = String::new();
    let mut remaining = if nanos < 0 {
        result.push('-');
        nanos.unsigned_abs()
    } else {
        nanos as u64
    };

    const NANOS_PER_NS: u64 = 1;
    const NANOS_PER_US: u64 = 1_000;
    const NANOS_PER_MS: u64 = 1_000_000;
    const NANOS_PER_S: u64 = 1_000_000_000;
    const NANOS_PER_M: u64 = 60 * NANOS_PER_S;
    const NANOS_PER_H: u64 = 60 * NANOS_PER_M;

    // For durations >= 1s, Go uses compound h/m/s format
    if remaining >= NANOS_PER_S {
        let hours = remaining / NANOS_PER_H;
        remaining %= NANOS_PER_H;
        let minutes = remaining / NANOS_PER_M;
        remaining %= NANOS_PER_M;
        let seconds = remaining / NANOS_PER_S;
        let sub_second_nanos = remaining % NANOS_PER_S;

        if hours > 0 {
            result.push_str(&format!("{hours}h"));
        }
        if minutes > 0 || hours > 0 {
            result.push_str(&format!("{minutes}m"));
        }
        if sub_second_nanos == 0 {
            result.push_str(&format!("{seconds}s"));
        } else {
            // Format fractional seconds, trimming trailing zeros
            let frac = format!("{sub_second_nanos:09}");
            let frac = frac.trim_end_matches('0');
            result.push_str(&format!("{seconds}.{frac}s"));
        }
    } else if remaining >= NANOS_PER_MS {
        // Milliseconds with optional fractional part
        let ms = remaining / NANOS_PER_MS;
        let sub = remaining % NANOS_PER_MS;
        if sub == 0 {
            result.push_str(&format!("{ms}ms"));
        } else {
            let frac = format!("{sub:06}");
            let frac = frac.trim_end_matches('0');
            result.push_str(&format!("{ms}.{frac}ms"));
        }
    } else if remaining >= NANOS_PER_US {
        // Microseconds
        let us = remaining / NANOS_PER_US;
        let sub = remaining % NANOS_PER_US;
        if sub == 0 {
            result.push_str(&format!("{us}µs"));
        } else {
            let frac = format!("{sub:03}");
            let frac = frac.trim_end_matches('0');
            result.push_str(&format!("{us}.{frac}µs"));
        }
    } else {
        // Nanoseconds
        result.push_str(&format!("{}ns", remaining));
    }

    result
}

/// Abstract interface for relation storage (Edge Mode).
///
/// Tuples are currently stored as `Vec<Value>` where compound values
/// (lists, structs, maps) appear as `Value::Compound(...)`.
///
/// TODO: Support explicit table flattening. When a user annotates a type
/// declaration, compound columns should be inlined into the tuple as
/// length-prefixed sequences of scalar values. This flattening should
/// only apply to explicitly requested levels of the type tree (no
/// automatic recursive flattening). The Store would then see wider
/// tuples of scalar values instead of Compound entries.
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

/// Opaque handle to a value in the host's value store.
/// In WASM, these are represented as `externref`.
#[cfg(feature = "server")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostVal(pub u32);

/// Trait for the host environment that provides storage and data access (Server Mode).
///
/// All Mangle values (numbers, floats, strings, compounds) are represented as
/// opaque `HostVal` handles. In WASM, these map to `externref` — the WASM module
/// never inspects values directly; all operations go through host calls.
#[cfg(feature = "server")]
pub trait Host {
    // --- Iterator/scan (control flow) ---
    fn scan_start(&mut self, rel_id: i32) -> i32;
    fn scan_delta_start(&mut self, rel_id: i32) -> i32;
    fn scan_next(&mut self, iter_id: i32) -> i32;
    /// Merges deltas and returns 1 if changes occurred, 0 otherwise.
    fn merge_deltas(&mut self) -> i32;
    fn scan_aggregate_start(&mut self, rel_id: i32, description: Vec<i32>) -> i32;
    fn scan_index_start(&mut self, rel_id: i32, col_idx: i32, val: HostVal) -> i32;

    // --- Value access ---
    fn get_col(&mut self, tuple_ptr: i32, col_idx: i32) -> HostVal;

    // --- Multi-column insertion ---
    fn insert_begin(&mut self, rel_id: i32);
    fn insert_push(&mut self, val: HostVal);
    fn insert_end(&mut self);

    // --- Constants ---
    fn const_number(&mut self, n: i64) -> HostVal;
    fn const_float(&mut self, bits: i64) -> HostVal;
    fn const_string(&mut self, id: i32) -> HostVal;
    fn const_name(&mut self, id: i32) -> HostVal;
    fn const_time(&mut self, nanos: i64) -> HostVal;
    fn const_duration(&mut self, nanos: i64) -> HostVal;

    // --- Arithmetic (handles int/float promotion internally) ---
    fn val_add(&mut self, a: HostVal, b: HostVal) -> HostVal;
    fn val_sub(&mut self, a: HostVal, b: HostVal) -> HostVal;
    fn val_mul(&mut self, a: HostVal, b: HostVal) -> HostVal;
    fn val_div(&mut self, a: HostVal, b: HostVal) -> HostVal;
    fn val_sqrt(&mut self, a: HostVal) -> HostVal;

    // --- Comparisons (return 0 or 1) ---
    fn val_eq(&mut self, a: HostVal, b: HostVal) -> i32;
    fn val_neq(&mut self, a: HostVal, b: HostVal) -> i32;
    fn val_lt(&mut self, a: HostVal, b: HostVal) -> i32;
    fn val_le(&mut self, a: HostVal, b: HostVal) -> i32;
    fn val_gt(&mut self, a: HostVal, b: HostVal) -> i32;
    fn val_ge(&mut self, a: HostVal, b: HostVal) -> i32;

    // --- String operations ---
    fn str_concat(&mut self, a: HostVal, b: HostVal) -> HostVal;
    fn str_replace(&mut self, s: HostVal, old: HostVal, new: HostVal, count: HostVal) -> HostVal;
    fn val_to_string(&mut self, val: HostVal) -> HostVal;

    // --- Compound operations ---
    /// Begin building a compound value. kind: 0=List, 1=Pair, 2=Map, 3=Struct.
    fn compound_begin(&mut self, kind: i32);
    fn compound_push(&mut self, val: HostVal);
    fn compound_end(&mut self) -> HostVal;
    /// Get element by index (list) or value by key (map/struct).
    fn compound_get(&mut self, compound: HostVal, key: HostVal) -> HostVal;
    /// Get length/size of compound, returned as a Number HostVal.
    fn compound_len(&mut self, compound: HostVal) -> HostVal;
    fn pair_first(&mut self, compound: HostVal) -> HostVal;
    fn pair_second(&mut self, compound: HostVal) -> HostVal;

    // --- Debug ---
    fn debuglog(&mut self, val: HostVal);
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
