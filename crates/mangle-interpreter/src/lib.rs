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

//! # Mangle Interpreter
//!
//! A pure Rust interpreter for the Mangle Intermediate Representation (IR).
//!
//! This crate enables the **Edge Mode** of execution, allowing Mangle programs
//! to run on devices or in environments where a WebAssembly runtime is not available
//! or desired.
//!
//! It executes the Physical IR operations (`Op`) directly.
//!
//! ## Usage
//!
//! See `mangle-driver` for the high-level API to compile and execute source code.

use anyhow::{Result, anyhow};
use mangle_ir::physical::{Aggregate, CmpOp, Condition, Constant, DataSource, Expr, Op, Operand};
use mangle_ir::{Ir, NameId};
use std::collections::HashMap;

pub use mangle_common::{CompoundKind, Store, Value};

/// Internal storage cell. Compound values are stored inline as a `CompoundStart`
/// marker (holding the kind and element count) followed by that many cells
/// (which may themselves be nested compounds). Scalar values are stored directly.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Cell {
    Val(Value),
    /// Marks the start of a compound with `n` logical elements following.
    CompoundStart(CompoundKind, usize),
}

/// Flatten a `Value` into a sequence of `Cell`s.
/// Scalar values become a single `Cell::Val`. Compound values become
/// `Cell::CompoundStart(n)` followed by the flattened cells of each element.
fn flatten_value(v: &Value, out: &mut Vec<Cell>) {
    match v {
        Value::Compound(kind, elems) => {
            out.push(Cell::CompoundStart(*kind, elems.len()));
            for elem in elems {
                flatten_value(elem, out);
            }
        }
        _ => out.push(Cell::Val(v.clone())),
    }
}

/// Flatten a tuple of Values into a flat Vec of Cells.
fn flatten_tuple(tuple: &[Value]) -> Vec<Cell> {
    let mut cells = Vec::new();
    for v in tuple {
        flatten_value(v, &mut cells);
    }
    cells
}

/// Read one logical Value from cells starting at `pos`, advancing `pos`.
fn unflatten_one(cells: &[Cell], pos: &mut usize) -> Value {
    match &cells[*pos] {
        Cell::Val(v) => {
            *pos += 1;
            v.clone()
        }
        Cell::CompoundStart(kind, n) => {
            let kind = *kind;
            let n = *n;
            *pos += 1;
            let mut elems = Vec::with_capacity(n);
            for _ in 0..n {
                elems.push(unflatten_one(cells, pos));
            }
            Value::Compound(kind, elems)
        }
    }
}

/// Skip past one logical value in a cell slice, advancing `pos`.
fn skip_one(cells: &[Cell], pos: &mut usize) {
    match &cells[*pos] {
        Cell::Val(_) => *pos += 1,
        Cell::CompoundStart(_, n) => {
            let n = *n;
            *pos += 1;
            for _ in 0..n {
                skip_one(cells, pos);
            }
        }
    }
}

/// Reconstruct a `Vec<Value>` tuple from flat cells, reading `n_cols` logical values.
fn unflatten_tuple(cells: &[Cell], n_cols: usize) -> Vec<Value> {
    let mut pos = 0;
    let mut tuple = Vec::with_capacity(n_cols);
    for _ in 0..n_cols {
        tuple.push(unflatten_one(cells, &mut pos));
    }
    tuple
}

/// A simple in-memory implementation of `Store`.
/// Supports semi-naive evaluation by tracking "stable" and "delta" facts.
///
/// Compound values are stored inline: each `Value::Compound` is flattened into
/// a `CompoundStart(n)` marker followed by its elements. This avoids nested
/// heap allocations in stored tuples and enables future per-field indexing.
#[derive(Default)]
pub struct MemStore {
    // Stable facts from previous iterations
    stable: HashMap<String, Vec<Vec<Cell>>>,
    // New facts from the current iteration
    delta: HashMap<String, Vec<Vec<Cell>>>,
    // Facts being collected for the next iteration
    next_delta: HashMap<String, Vec<Vec<Cell>>>,

    // Number of logical columns per relation (set on first insert).
    arity: HashMap<String, usize>,

    // Secondary indexes: (relation_name, col_idx) -> { Cell -> [row_indices] }
    // Indexes use the first Cell of each logical column (scalars: the value itself,
    // compounds: CompoundStart(n)). This enables index lookups on scalar columns.
    stable_indexes: HashMap<(String, usize), HashMap<Cell, Vec<usize>>>,
    delta_indexes: HashMap<(String, usize), HashMap<Cell, Vec<usize>>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a relation (creating it if absent) to allow scanning it.
    pub fn create_relation(&mut self, relation: &str) {
        self.stable.entry(relation.to_string()).or_default();
    }

    /// Add a fact manually (for testing/setup). Auto-creates relation in stable.
    pub fn add_fact(&mut self, relation: &str, args: Vec<Value>) {
        let n_cols = args.len();
        let cells = flatten_tuple(&args);
        let table = self.stable.entry(relation.to_string()).or_default();
        if !table.contains(&cells) {
            let row_idx = table.len();
            self.arity.entry(relation.to_string()).or_insert(n_cols);
            table.push(cells.clone());
            index_cells(&mut self.stable_indexes, relation, &cells, n_cols, row_idx);
        }
    }

    /// Rebuilds stable and delta indexes for a single relation after mutation.
    fn rebuild_indexes_for(&mut self, relation: &str) {
        self.stable_indexes.retain(|(rel, _), _| rel != relation);
        self.delta_indexes.retain(|(rel, _), _| rel != relation);

        let n_cols = self.arity.get(relation).copied().unwrap_or(0);

        if let Some(table) = self.stable.get(relation) {
            for (row_idx, cells) in table.iter().enumerate() {
                index_cells(&mut self.stable_indexes, relation, cells, n_cols, row_idx);
            }
        }

        if let Some(table) = self.delta.get(relation) {
            for (row_idx, cells) in table.iter().enumerate() {
                index_cells(&mut self.delta_indexes, relation, cells, n_cols, row_idx);
            }
        }
    }

    /// Unflatten stored cells into Values using the relation's arity.
    fn to_values(&self, relation: &str, cells: &[Cell]) -> Vec<Value> {
        let n_cols = self.arity.get(relation).copied().unwrap_or(0);
        if n_cols == 0 {
            // Fallback: treat each cell as a scalar
            cells
                .iter()
                .map(|c| match c {
                    Cell::Val(v) => v.clone(),
                    Cell::CompoundStart(_, _) => Value::Null,
                })
                .collect()
        } else {
            unflatten_tuple(cells, n_cols)
        }
    }

    pub fn get_facts(&self, relation: &str) -> Vec<Vec<Value>> {
        let mut all: Vec<Vec<Value>> = self
            .stable
            .get(relation)
            .into_iter()
            .flatten()
            .map(|cells| self.to_values(relation, cells))
            .collect();
        if let Some(d) = self.delta.get(relation) {
            for cells in d {
                all.push(self.to_values(relation, cells));
            }
        }
        all
    }

    /// Coalesce temporal intervals for a relation.
    /// Groups facts by their non-temporal columns (all except the last 2),
    /// sorts intervals by start time, and merges overlapping/adjacent intervals.
    /// This prevents interval explosion in recursive temporal rules.
    pub fn coalesce_temporal(&mut self, relation: &str) {
        let n_cols = match self.arity.get(relation) {
            Some(&n) if n >= 2 => n,
            _ => return,
        };
        let key_cols = n_cols - 2; // non-temporal columns

        // Collect all facts (stable + delta) as Value tuples
        let mut all_facts: Vec<Vec<Value>> = Vec::new();
        if let Some(table) = self.stable.get(relation) {
            for cells in table {
                all_facts.push(unflatten_tuple(cells, n_cols));
            }
        }
        if let Some(table) = self.delta.get(relation) {
            for cells in table {
                all_facts.push(unflatten_tuple(cells, n_cols));
            }
        }

        if all_facts.is_empty() {
            return;
        }

        // Group by non-temporal key columns
        let mut groups: HashMap<Vec<Value>, Vec<(i64, i64)>> = HashMap::new();
        for fact in &all_facts {
            let key: Vec<Value> = fact[..key_cols].to_vec();
            let start = match &fact[key_cols] {
                Value::Time(t) => *t,
                Value::Number(n) => *n,
                _ => continue,
            };
            let end = match &fact[key_cols + 1] {
                Value::Time(t) => *t,
                Value::Number(n) => *n,
                _ => continue,
            };
            groups.entry(key).or_default().push((start, end));
        }

        // Coalesce intervals within each group
        let mut coalesced_facts: Vec<Vec<Value>> = Vec::new();
        for (key, mut intervals) in groups {
            intervals.sort_by_key(|&(s, _)| s);
            let mut merged: Vec<(i64, i64)> = vec![intervals[0]];
            for &(s, e) in &intervals[1..] {
                let last = merged.last_mut().unwrap();
                // Merge if overlapping or adjacent (within 1 nanosecond)
                if s <= last.1.saturating_add(1) {
                    last.1 = last.1.max(e);
                } else {
                    merged.push((s, e));
                }
            }
            for (start, end) in merged {
                let mut fact = key.clone();
                fact.push(Value::Time(start));
                fact.push(Value::Time(end));
                coalesced_facts.push(fact);
            }
        }

        // Replace stable with coalesced facts, clear delta
        let coalesced_cells: Vec<Vec<Cell>> = coalesced_facts
            .iter()
            .map(|fact| flatten_tuple(fact))
            .collect();
        self.stable.insert(relation.to_string(), coalesced_cells);
        self.delta.remove(relation);
        self.next_delta.remove(relation);
        self.rebuild_indexes_for(relation);
    }
}

/// Index a flattened tuple's logical columns.
fn index_cells(
    indexes: &mut HashMap<(String, usize), HashMap<Cell, Vec<usize>>>,
    relation: &str,
    cells: &[Cell],
    n_cols: usize,
    row_idx: usize,
) {
    let mut pos = 0;
    for col_idx in 0..n_cols {
        let key = cells[pos].clone();
        skip_one(cells, &mut pos);
        indexes
            .entry((relation.to_string(), col_idx))
            .or_default()
            .entry(key)
            .or_default()
            .push(row_idx);
    }
}

/// Convert a Value to its index Cell key (for index lookup matching).
fn value_to_index_key(v: &Value) -> Cell {
    match v {
        Value::Compound(kind, elems) => Cell::CompoundStart(*kind, elems.len()),
        _ => Cell::Val(v.clone()),
    }
}

impl Store for MemStore {
    fn scan(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        let n_cols = self.arity.get(relation).copied().unwrap_or(0);
        let s = self
            .stable
            .get(relation)
            .into_iter()
            .flatten()
            .map(move |cells| unflatten_tuple(cells, n_cols));
        let d = self
            .delta
            .get(relation)
            .into_iter()
            .flatten()
            .map(move |cells| unflatten_tuple(cells, n_cols));
        Ok(Box::new(s.chain(d)))
    }

    fn scan_delta(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        let n_cols = self.arity.get(relation).copied().unwrap_or(0);
        match self.delta.get(relation) {
            Some(tuples) => Ok(Box::new(
                tuples
                    .iter()
                    .map(move |cells| unflatten_tuple(cells, n_cols)),
            )),
            None => Ok(Box::new(std::iter::empty())),
        }
    }

    fn scan_next_delta(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        let n_cols = self.arity.get(relation).copied().unwrap_or(0);
        match self.next_delta.get(relation) {
            Some(tuples) => Ok(Box::new(
                tuples
                    .iter()
                    .map(move |cells| unflatten_tuple(cells, n_cols)),
            )),
            None => Ok(Box::new(std::iter::empty())),
        }
    }

    fn scan_index(
        &self,
        relation: &str,
        col_idx: usize,
        key: &Value,
    ) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        let n_cols = self.arity.get(relation).copied().unwrap_or(0);
        let cell_key = value_to_index_key(key);
        let mut results: Vec<Vec<Value>> = Vec::new();

        if let Some(idx_map) = self.stable_indexes.get(&(relation.to_string(), col_idx))
            && let Some(row_indices) = idx_map.get(&cell_key)
            && let Some(table) = self.stable.get(relation)
        {
            for &i in row_indices {
                results.push(unflatten_tuple(&table[i], n_cols));
            }
        }

        if let Some(idx_map) = self.delta_indexes.get(&(relation.to_string(), col_idx))
            && let Some(row_indices) = idx_map.get(&cell_key)
            && let Some(table) = self.delta.get(relation)
        {
            for &i in row_indices {
                results.push(unflatten_tuple(&table[i], n_cols));
            }
        }

        Ok(Box::new(results.into_iter()))
    }

    fn scan_delta_index(
        &self,
        relation: &str,
        col_idx: usize,
        key: &Value,
    ) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        let n_cols = self.arity.get(relation).copied().unwrap_or(0);
        let cell_key = value_to_index_key(key);
        let mut results: Vec<Vec<Value>> = Vec::new();

        if let Some(idx_map) = self.delta_indexes.get(&(relation.to_string(), col_idx))
            && let Some(row_indices) = idx_map.get(&cell_key)
            && let Some(table) = self.delta.get(relation)
        {
            for &i in row_indices {
                results.push(unflatten_tuple(&table[i], n_cols));
            }
        }

        Ok(Box::new(results.into_iter()))
    }

    fn insert(&mut self, relation: &str, tuple: Vec<Value>) -> Result<bool> {
        let n_cols = tuple.len();
        let cells = flatten_tuple(&tuple);

        // Check if fact is already in stable, delta, or next_delta
        if self
            .stable
            .get(relation)
            .is_some_and(|v| v.contains(&cells))
            || self
                .delta
                .get(relation)
                .is_some_and(|v| v.contains(&cells))
            || self
                .next_delta
                .get(relation)
                .is_some_and(|v| v.contains(&cells))
        {
            return Ok(false);
        }

        self.arity.entry(relation.to_string()).or_insert(n_cols);
        self.next_delta
            .entry(relation.to_string())
            .or_default()
            .push(cells);
        Ok(true)
    }

    fn merge_deltas(&mut self) {
        // 1. Move current delta to stable
        for (rel_name, mut tuples) in self.delta.drain() {
            let n_cols = self.arity.get(&rel_name).copied().unwrap_or(0);
            let table = self.stable.entry(rel_name.clone()).or_default();
            for cells in tuples.drain(..) {
                let row_idx = table.len();
                index_cells(
                    &mut self.stable_indexes,
                    &rel_name,
                    &cells,
                    n_cols,
                    row_idx,
                );
                table.push(cells);
            }
        }
        self.delta_indexes.clear();

        // 2. Move next_delta to delta and build delta index
        self.delta = std::mem::take(&mut self.next_delta);
        for (rel_name, tuples) in &self.delta {
            let n_cols = self.arity.get(rel_name).copied().unwrap_or(0);
            for (row_idx, cells) in tuples.iter().enumerate() {
                index_cells(
                    &mut self.delta_indexes,
                    rel_name,
                    cells,
                    n_cols,
                    row_idx,
                );
            }
        }
    }

    fn create_relation(&mut self, relation: &str) {
        self.stable.entry(relation.to_string()).or_default();
    }

    fn retract(&mut self, relation: &str, tuple: &[Value]) -> Result<bool> {
        let cells = flatten_tuple(tuple);
        let removed = if let Some(table) = self.stable.get_mut(relation) {
            if let Some(pos) = table.iter().position(|t| *t == cells) {
                table.swap_remove(pos);
                true
            } else {
                false
            }
        } else {
            false
        };

        // Also remove from delta and next_delta
        if let Some(table) = self.delta.get_mut(relation) {
            if let Some(pos) = table.iter().position(|t| *t == cells) {
                table.swap_remove(pos);
            }
        }
        if let Some(table) = self.next_delta.get_mut(relation) {
            if let Some(pos) = table.iter().position(|t| *t == cells) {
                table.swap_remove(pos);
            }
        }

        if removed {
            self.rebuild_indexes_for(relation);
        }
        Ok(removed)
    }

    fn clear(&mut self, relation: &str) {
        if let Some(table) = self.stable.get_mut(relation) {
            table.clear();
        }
        if let Some(table) = self.delta.get_mut(relation) {
            table.clear();
        }
        if let Some(table) = self.next_delta.get_mut(relation) {
            table.clear();
        }
        self.stable_indexes.retain(|(rel, _), _| rel != relation);
        self.delta_indexes.retain(|(rel, _), _| rel != relation);
    }

    fn relation_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.stable.keys().cloned().collect();
        for key in self.delta.keys() {
            if !names.contains(key) {
                names.push(key.clone());
            }
        }
        names
    }

    fn coalesce_temporal(&mut self, relation: &str) {
        MemStore::coalesce_temporal(self, relation);
    }
}

/// A record of one derivation: which fact was derived and which premises were used.
#[derive(Debug, Clone)]
pub struct ProvenanceEntry {
    /// The derived fact: (relation_name, tuple).
    pub derived: (String, Vec<Value>),
    /// The premise facts that contributed: (relation_name, tuple) for each.
    pub premises: Vec<(String, Vec<Value>)>,
}

/// Lightweight recorder that captures derivation provenance during execution.
///
/// When enabled on the interpreter, every successful `Op::Insert` records
/// which premise facts (from enclosing `Op::Iterate` scans) contributed to
/// the derivation. When disabled (the default), there is zero overhead.
#[derive(Default)]
pub struct ProvenanceRecorder {
    /// All recorded derivations.
    pub entries: Vec<ProvenanceEntry>,
    /// Stack of currently-active scan sources (pushed on Iterate, popped after).
    active_premises: Vec<(String, Vec<Value>)>,
}

/// A pure Rust interpreter for Mangle IR.
pub struct Interpreter<'a> {
    ir: &'a Ir,
    store: Box<dyn Store + 'a>,
    provenance: Option<ProvenanceRecorder>,
}

struct Env {
    vars: HashMap<NameId, Value>,
}

impl Env {
    fn new() -> Self {
        Self {
            vars: HashMap::new(),
        }
    }
}

impl<'a> Interpreter<'a> {
    pub fn new(ir: &'a Ir, store: Box<dyn Store + 'a>) -> Self {
        Self {
            ir,
            store,
            provenance: None,
        }
    }

    /// Enable provenance recording. When set, every successful insert
    /// records which premise facts were in the current environment.
    pub fn with_provenance(mut self) -> Self {
        self.provenance = Some(ProvenanceRecorder::default());
        self
    }

    /// Helper to get the underlying store (e.g. to inspect results).
    pub fn store(&self) -> &dyn Store {
        &*self.store
    }

    /// Helper to get the underlying store mutably.
    pub fn store_mut(&mut self) -> &mut dyn Store {
        &mut *self.store
    }

    /// Consumes the interpreter and returns the underlying store.
    pub fn into_store(self) -> Box<dyn Store + 'a> {
        self.store
    }

    /// Consume the interpreter, returning the provenance recorder if enabled.
    pub fn into_provenance(self) -> Option<ProvenanceRecorder> {
        self.provenance
    }

    /// Consume the interpreter, returning the store and optional provenance.
    pub fn into_parts(self) -> (Box<dyn Store + 'a>, Option<ProvenanceRecorder>) {
        (self.store, self.provenance)
    }

    /// Executes the operation and returns the number of facts inserted.
    pub fn execute(&mut self, op: &Op) -> Result<usize> {
        let mut env = Env::new();
        self.exec_op(op, &mut env)
    }

    fn exec_op(&mut self, op: &Op, env: &mut Env) -> Result<usize> {
        match op {
            Op::Nop => Ok(0),
            Op::Seq(ops) => {
                let mut count = 0;
                for o in ops {
                    count += self.exec_op(o, env)?;
                }
                Ok(count)
            }
            Op::Iterate { source, body } => {
                let mut count = 0;
                match source {
                    DataSource::Scan { relation, vars } => {
                        let rel_name = self.ir.resolve_name(*relation);
                        let iter = self.store.scan(rel_name)?;
                        let tuples: Vec<_> = iter.collect();

                        for tuple in tuples {
                            if tuple.len() != vars.len() {
                                continue;
                            }
                            for (i, var) in vars.iter().enumerate() {
                                env.vars.insert(*var, tuple[i].clone());
                            }
                            if let Some(ref mut prov) = self.provenance {
                                prov.active_premises
                                    .push((rel_name.to_string(), tuple.clone()));
                            }
                            count += self.exec_op(body, env)?;
                            if self.provenance.is_some() {
                                self.provenance.as_mut().unwrap().active_premises.pop();
                            }
                        }
                    }
                    DataSource::ScanDelta { relation, vars } => {
                        let rel_name = self.ir.resolve_name(*relation);
                        let iter = self.store.scan_delta(rel_name)?;
                        let tuples: Vec<_> = iter.collect();

                        for tuple in tuples {
                            if tuple.len() != vars.len() {
                                continue;
                            }
                            for (i, var) in vars.iter().enumerate() {
                                env.vars.insert(*var, tuple[i].clone());
                            }
                            if let Some(ref mut prov) = self.provenance {
                                prov.active_premises
                                    .push((rel_name.to_string(), tuple.clone()));
                            }
                            count += self.exec_op(body, env)?;
                            if self.provenance.is_some() {
                                self.provenance.as_mut().unwrap().active_premises.pop();
                            }
                        }
                    }
                    DataSource::IndexLookup {
                        relation,
                        col_idx,
                        key,
                        vars,
                    } => {
                        let rel_name = self.ir.resolve_name(*relation);
                        let key_val = self.eval_operand(key, env)?;

                        let iter = self.store.scan_index(rel_name, *col_idx, &key_val)?;
                        let tuples: Vec<_> = iter.collect();

                        for tuple in tuples {
                            if tuple.len() != vars.len() {
                                continue;
                            }
                            for (i, var) in vars.iter().enumerate() {
                                env.vars.insert(*var, tuple[i].clone());
                            }
                            if let Some(ref mut prov) = self.provenance {
                                prov.active_premises
                                    .push((rel_name.to_string(), tuple.clone()));
                            }
                            count += self.exec_op(body, env)?;
                            if self.provenance.is_some() {
                                self.provenance.as_mut().unwrap().active_premises.pop();
                            }
                        }
                    }
                }
                Ok(count)
            }
            Op::Filter { cond, body } => {
                if self.eval_cond(cond, env)? {
                    self.exec_op(body, env)
                } else {
                    Ok(0)
                }
            }
            Op::Insert { relation, args } => {
                let rel_name = self.ir.resolve_name(*relation);
                let mut tuple = Vec::new();
                for arg in args {
                    tuple.push(self.eval_operand(arg, env)?);
                }
                let is_new = self.store.insert(rel_name, tuple.clone())?;
                if is_new {
                    if let Some(ref mut prov) = self.provenance {
                        prov.entries.push(ProvenanceEntry {
                            derived: (rel_name.to_string(), tuple),
                            premises: prov.active_premises.clone(),
                        });
                    }
                    Ok(1)
                } else {
                    Ok(0)
                }
            }
            Op::Let { var, expr, body } => {
                let val = self.eval_expr(expr, env)?;
                env.vars.insert(*var, val);
                self.exec_op(body, env)
            }
            Op::HashJoin {
                build_source,
                probe_source,
                join_keys,
                body,
            } => self.exec_hash_join(build_source, probe_source, join_keys, body, env),
            Op::GroupBy {
                source,
                vars,
                keys,
                aggregates,
                body,
            } => {
                let rel_name = self.ir.resolve_name(*source);

                // For GroupBy, we must scan ALL available facts including ones just produced in this stratum
                // if we want to match Go implementation's behavior for non-recursive strata.
                let iter = self.store.scan(rel_name)?;
                let mut tuples: Vec<_> = iter.collect();

                // Also scan next_delta if it's the same relation
                if let Ok(nd_iter) = self.store.scan_next_delta(rel_name) {
                    tuples.extend(nd_iter);
                }

                let mut groups: HashMap<Vec<Value>, Vec<Vec<Value>>> = HashMap::new();

                for tuple in tuples {
                    if tuple.len() != vars.len() {
                        continue;
                    }
                    // Temporarily bind variables to extract key
                    for (i, var) in vars.iter().enumerate() {
                        env.vars.insert(*var, tuple[i].clone());
                    }

                    let mut key = Vec::new();
                    for k in keys {
                        if let Some(val) = env.vars.get(k) {
                            key.push(val.clone());
                        } else {
                            // Should not happen if well-typed
                            key.push(Value::Null);
                        }
                    }
                    groups.entry(key).or_default().push(tuple);
                }

                let mut count = 0;
                for (key, group_tuples) in groups {
                    // Bind keys
                    for (i, k) in keys.iter().enumerate() {
                        env.vars.insert(*k, key[i].clone());
                    }

                    // Compute aggregates
                    for agg in aggregates {
                        let val = self.eval_aggregate(agg, &group_tuples, vars, env)?;
                        env.vars.insert(agg.var, val);
                    }

                    count += self.exec_op(body, env)?;
                }
                Ok(count)
            }
        }
    }

    /// Iterate a `DataSource` and return all tuples along with the var list
    /// the tuples bind. Used by `HashJoin` to materialize the build side and
    /// to stream the probe side.
    fn collect_data_source(
        &mut self,
        source: &DataSource,
        env: &mut Env,
    ) -> Result<(Vec<Vec<Value>>, Vec<NameId>)> {
        match source {
            DataSource::Scan { relation, vars } => {
                let rel_name = self.ir.resolve_name(*relation);
                let tuples: Vec<_> = self.store.scan(rel_name)?.collect();
                Ok((tuples, vars.clone()))
            }
            DataSource::ScanDelta { relation, vars } => {
                let rel_name = self.ir.resolve_name(*relation);
                let tuples: Vec<_> = self.store.scan_delta(rel_name)?.collect();
                Ok((tuples, vars.clone()))
            }
            DataSource::IndexLookup {
                relation,
                col_idx,
                key,
                vars,
            } => {
                let rel_name = self.ir.resolve_name(*relation);
                let key_val = self.eval_operand(key, env)?;
                let tuples: Vec<_> =
                    self.store.scan_index(rel_name, *col_idx, &key_val)?.collect();
                Ok((tuples, vars.clone()))
            }
        }
    }

    fn exec_hash_join(
        &mut self,
        build_source: &DataSource,
        probe_source: &DataSource,
        join_keys: &[NameId],
        body: &Op,
        env: &mut Env,
    ) -> Result<usize> {
        let (build_tuples, build_vars) = self.collect_data_source(build_source, env)?;

        // Position of each join-key variable inside build_vars. If any
        // join_key is not in build_vars, the plan is malformed.
        let build_key_positions: Vec<usize> = join_keys
            .iter()
            .map(|k| {
                build_vars
                    .iter()
                    .position(|v| v == k)
                    .ok_or_else(|| anyhow!("HashJoin: join key not in build_source vars"))
            })
            .collect::<Result<Vec<_>>>()?;

        let mut table: HashMap<Vec<Value>, Vec<Vec<Value>>> = HashMap::new();
        for tuple in build_tuples {
            if tuple.len() != build_vars.len() {
                continue;
            }
            let key: Vec<Value> = build_key_positions
                .iter()
                .map(|&i| tuple[i].clone())
                .collect();
            table.entry(key).or_default().push(tuple);
        }

        let (probe_tuples, probe_vars) = self.collect_data_source(probe_source, env)?;
        let probe_key_positions: Vec<usize> = join_keys
            .iter()
            .map(|k| {
                probe_vars
                    .iter()
                    .position(|v| v == k)
                    .ok_or_else(|| anyhow!("HashJoin: join key not in probe_source vars"))
            })
            .collect::<Result<Vec<_>>>()?;

        let mut count = 0;
        for probe_tuple in probe_tuples {
            if probe_tuple.len() != probe_vars.len() {
                continue;
            }
            let key: Vec<Value> = probe_key_positions
                .iter()
                .map(|&i| probe_tuple[i].clone())
                .collect();
            let Some(matches) = table.get(&key) else {
                continue;
            };
            // Bind probe-side vars once; build-side bindings cycle per match.
            for (i, var) in probe_vars.iter().enumerate() {
                env.vars.insert(*var, probe_tuple[i].clone());
            }
            for build_tuple in matches {
                for (i, var) in build_vars.iter().enumerate() {
                    env.vars.insert(*var, build_tuple[i].clone());
                }
                count += self.exec_op(body, env)?;
            }
        }
        Ok(count)
    }

    fn eval_aggregate(
        &self,
        agg: &Aggregate,
        group: &[Vec<Value>],
        vars: &[NameId],
        env: &mut Env,
    ) -> Result<Value> {
        let fn_name = self.ir.resolve_name(agg.func);
        match fn_name {
            "fn:count" => Ok(Value::Number(group.len() as i64)),
            "fn:sum" => {
                let arg = agg
                    .args
                    .first()
                    .ok_or_else(|| anyhow!("fn:sum requires 1 argument"))?;

                let mut int_sum: i64 = 0;
                for tuple in group {
                    for (i, var) in vars.iter().enumerate() {
                        env.vars.insert(*var, tuple[i].clone());
                    }
                    let val = self.eval_operand(arg, env)?;
                    match val {
                        Value::Number(n) => int_sum += n,
                        _ => return Err(anyhow!("fn:sum: expected integer, got {val}")),
                    }
                }
                Ok(Value::Number(int_sum))
            }
            "fn:float:sum" => {
                let arg = agg
                    .args
                    .first()
                    .ok_or_else(|| anyhow!("fn:float:sum requires 1 argument"))?;

                let mut float_sum: f64 = 0.0;
                for tuple in group {
                    for (i, var) in vars.iter().enumerate() {
                        env.vars.insert(*var, tuple[i].clone());
                    }
                    let val = self.eval_operand(arg, env)?;
                    float_sum += value_as_float(&val)?;
                }
                Ok(Value::Float(float_sum))
            }
            "fn:max" => {
                let mut max_val = None;
                let arg = agg
                    .args
                    .first()
                    .ok_or_else(|| anyhow!("fn:max requires 1 argument"))?;

                for tuple in group {
                    for (i, var) in vars.iter().enumerate() {
                        env.vars.insert(*var, tuple[i].clone());
                    }
                    let val = self.eval_operand(arg, env)?;
                    match max_val {
                        None => max_val = Some(val),
                        Some(ref m) => {
                            if val > *m {
                                max_val = Some(val);
                            }
                        }
                    }
                }
                max_val.ok_or_else(|| anyhow!("fn:max on empty group"))
            }
            "fn:min" => {
                let mut min_val = None;
                let arg = agg
                    .args
                    .first()
                    .ok_or_else(|| anyhow!("fn:min requires 1 argument"))?;

                for tuple in group {
                    for (i, var) in vars.iter().enumerate() {
                        env.vars.insert(*var, tuple[i].clone());
                    }
                    let val = self.eval_operand(arg, env)?;
                    match min_val {
                        None => min_val = Some(val),
                        Some(ref m) => {
                            if val < *m {
                                min_val = Some(val);
                            }
                        }
                    }
                }
                min_val.ok_or_else(|| anyhow!("fn:min on empty group"))
            }
            "fn:float:max" => {
                let mut max_val: Option<f64> = None;
                let arg = agg
                    .args
                    .first()
                    .ok_or_else(|| anyhow!("fn:float:max requires 1 argument"))?;

                for tuple in group {
                    for (i, var) in vars.iter().enumerate() {
                        env.vars.insert(*var, tuple[i].clone());
                    }
                    let val = self.eval_operand(arg, env)?;
                    let f = value_as_float(&val)?;
                    max_val = Some(match max_val {
                        None => f,
                        Some(m) => f.max(m),
                    });
                }
                max_val
                    .map(Value::Float)
                    .ok_or_else(|| anyhow!("fn:float:max on empty group"))
            }
            "fn:float:min" => {
                let mut min_val: Option<f64> = None;
                let arg = agg
                    .args
                    .first()
                    .ok_or_else(|| anyhow!("fn:float:min requires 1 argument"))?;

                for tuple in group {
                    for (i, var) in vars.iter().enumerate() {
                        env.vars.insert(*var, tuple[i].clone());
                    }
                    let val = self.eval_operand(arg, env)?;
                    let f = value_as_float(&val)?;
                    min_val = Some(match min_val {
                        None => f,
                        Some(m) => f.min(m),
                    });
                }
                min_val
                    .map(Value::Float)
                    .ok_or_else(|| anyhow!("fn:float:min on empty group"))
            }
            _ => Err(anyhow!("Unknown aggregation function: {fn_name}")),
        }
    }

    fn eval_cond(&self, cond: &Condition, env: &Env) -> Result<bool> {
        match cond {
            Condition::Cmp { op, left, right } => {
                let l = self.eval_operand(left, env)?;
                let r = self.eval_operand(right, env)?;
                match op {
                    CmpOp::Eq => Ok(l == r),
                    CmpOp::Neq => Ok(l != r),
                    CmpOp::Lt => Ok(l < r),
                    CmpOp::Le => Ok(l <= r),
                    CmpOp::Gt => Ok(l > r),
                    CmpOp::Ge => Ok(l >= r),
                }
            }
            Condition::Negation { relation, args } => {
                let rel_name = self.ir.resolve_name(*relation);
                let iter = self.store.scan(rel_name)?;
                for tuple in iter {
                    let mut mat = true;
                    if tuple.len() != args.len() {
                        continue;
                    }
                    for (i, arg) in args.iter().enumerate() {
                        let val = self.eval_operand(arg, env)?;
                        if tuple[i] != val {
                            mat = false;
                            break;
                        }
                    }
                    if mat {
                        return Ok(false); // Found match, negation fails
                    }
                }
                Ok(true) // No match found
            }
            Condition::Call { function, args } => {
                let fn_name = self.ir.resolve_name(*function);
                let mut vals = Vec::new();
                for arg in args {
                    vals.push(self.eval_operand(arg, env)?);
                }
                self.eval_builtin_predicate(fn_name, &vals)
            }
        }
    }

    fn eval_builtin_predicate(&self, name: &str, vals: &[Value]) -> Result<bool> {
        match name {
            ":string:starts_with" => match (&vals[0], &vals[1]) {
                (Value::String(s), Value::String(p)) => Ok(s.starts_with(p.as_str())),
                _ => Err(anyhow!(":string:starts_with: expected string arguments")),
            },
            ":string:ends_with" => match (&vals[0], &vals[1]) {
                (Value::String(s), Value::String(p)) => Ok(s.ends_with(p.as_str())),
                _ => Err(anyhow!(":string:ends_with: expected string arguments")),
            },
            ":string:contains" => match (&vals[0], &vals[1]) {
                (Value::String(s), Value::String(p)) => Ok(s.contains(p.as_str())),
                _ => Err(anyhow!(":string:contains: expected string arguments")),
            },
            ":match_prefix" => match (&vals[0], &vals[1]) {
                (Value::Name(name), Value::Name(prefix)) => {
                    Ok(name.starts_with(prefix.as_str()) && name.len() > prefix.len())
                }
                _ => Err(anyhow!(":match_prefix: expected name arguments")),
            },
            _ => Err(anyhow!("Unknown built-in predicate: {name}")),
        }
    }

    fn eval_expr(&self, expr: &Expr, env: &Env) -> Result<Value> {
        match expr {
            Expr::Value(op) => self.eval_operand(op, env),
            Expr::Call { function, args } => {
                let fn_name = self.ir.resolve_name(*function);
                let mut vals = Vec::new();
                for arg in args {
                    vals.push(self.eval_operand(arg, env)?);
                }
                eval_function(fn_name, &vals)
            }
        }
    }

    fn eval_operand(&self, op: &Operand, env: &Env) -> Result<Value> {
        match op {
            Operand::Var(v) => env
                .vars
                .get(v)
                .cloned()
                .ok_or_else(|| anyhow!("Variable not found")),
            Operand::Const(c) => match c {
                Constant::Number(n) => Ok(Value::Number(*n)),
                Constant::Float(f) => Ok(Value::Float(*f)),
                Constant::String(sid) => {
                    Ok(Value::String(self.ir.resolve_string(*sid).to_string()))
                }
                Constant::Name(nid) => Ok(Value::Name(self.ir.resolve_name(*nid).to_string())),
                Constant::Time(t) => Ok(Value::Time(*t)),
                Constant::Duration(d) => Ok(Value::Duration(*d)),
            },
        }
    }
}

// --- Helper functions ---

fn value_as_float(v: &Value) -> Result<f64> {
    match v {
        Value::Float(f) => Ok(*f),
        Value::Number(n) => Ok(*n as f64),
        _ => Err(anyhow!("expected numeric value, got {v}")),
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::Number(n) => n.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::String(s) => s.clone(),
        Value::Name(s) => s.clone(),
        Value::Time(t) => format!("{}", Value::Time(*t)),
        Value::Duration(d) => format!("{}", Value::Duration(*d)),
        Value::Compound(kind, elems) => format!("{}", Value::Compound(*kind, elems.clone())),
        Value::Null => "null".to_string(),
    }
}

/// Evaluate a built-in function call.
pub fn eval_function(fn_name: &str, vals: &[Value]) -> Result<Value> {
    match fn_name {
        // --- Integer arithmetic (variadic) ---
        "fn:plus" => {
            let mut sum: i64 = 0;
            for v in vals {
                match v {
                    Value::Number(n) => sum += n,
                    _ => return Err(anyhow!("fn:plus: expected integer, got {v}")),
                }
            }
            Ok(Value::Number(sum))
        }
        "fn:minus" => {
            if vals.is_empty() {
                return Err(anyhow!("fn:minus: requires at least 1 argument"));
            }
            let first = match &vals[0] {
                Value::Number(n) => *n,
                v => return Err(anyhow!("fn:minus: expected integer, got {v}")),
            };
            if vals.len() == 1 {
                return Ok(Value::Number(-first));
            }
            let mut result = first;
            for v in &vals[1..] {
                match v {
                    Value::Number(n) => result -= n,
                    _ => return Err(anyhow!("fn:minus: expected integer, got {v}")),
                }
            }
            Ok(Value::Number(result))
        }
        "fn:mult" => {
            let mut product: i64 = 1;
            for v in vals {
                match v {
                    Value::Number(n) => product *= n,
                    _ => return Err(anyhow!("fn:mult: expected integer, got {v}")),
                }
            }
            Ok(Value::Number(product))
        }
        "fn:div" => {
            if vals.is_empty() {
                return Err(anyhow!("fn:div: requires at least 1 argument"));
            }
            let first = match &vals[0] {
                Value::Number(n) => *n,
                v => return Err(anyhow!("fn:div: expected integer, got {v}")),
            };
            if vals.len() == 1 {
                if first == 0 {
                    return Err(anyhow!("Division by zero in fn:div"));
                }
                return Ok(Value::Number(1 / first));
            }
            let mut result = first;
            for v in &vals[1..] {
                match v {
                    Value::Number(0) => return Err(anyhow!("Division by zero in fn:div")),
                    Value::Number(n) => {
                        result /= n;
                        if result == 0 {
                            return Ok(Value::Number(0));
                        }
                    }
                    _ => return Err(anyhow!("fn:div: expected integer, got {v}")),
                }
            }
            Ok(Value::Number(result))
        }

        // --- Float arithmetic (variadic, accepts Number via promotion) ---
        "fn:float:plus" => {
            let mut sum: f64 = 0.0;
            for v in vals {
                sum += value_as_float(v)?;
            }
            Ok(Value::Float(sum))
        }
        "fn:float:minus" => {
            if vals.is_empty() {
                return Err(anyhow!("fn:float:minus: requires at least 1 argument"));
            }
            let first = value_as_float(&vals[0])?;
            if vals.len() == 1 {
                return Ok(Value::Float(-first));
            }
            let mut result = first;
            for v in &vals[1..] {
                result -= value_as_float(v)?;
            }
            Ok(Value::Float(result))
        }
        "fn:float:mult" => {
            let mut product: f64 = 1.0;
            for v in vals {
                product *= value_as_float(v)?;
            }
            Ok(Value::Float(product))
        }
        "fn:float:div" => {
            if vals.is_empty() {
                return Err(anyhow!("fn:float:div: requires at least 1 argument"));
            }
            let first = value_as_float(&vals[0])?;
            if vals.len() == 1 {
                if first == 0.0 {
                    return Err(anyhow!("Division by zero in fn:float:div"));
                }
                return Ok(Value::Float(1.0 / first));
            }
            let mut result = first;
            for v in &vals[1..] {
                let d = value_as_float(v)?;
                if d == 0.0 {
                    return Err(anyhow!("Division by zero in fn:float:div"));
                }
                result /= d;
            }
            Ok(Value::Float(result))
        }
        "fn:sqrt" => {
            if vals.len() != 1 {
                return Err(anyhow!("fn:sqrt: requires exactly 1 argument"));
            }
            let f = value_as_float(&vals[0])?;
            Ok(Value::Float(f.sqrt()))
        }

        // --- String functions ---
        "fn:string:concat" => {
            let mut result = String::new();
            for v in vals {
                result.push_str(&value_to_string(v));
            }
            Ok(Value::String(result))
        }
        "fn:string:replace" => {
            if vals.len() != 4 {
                return Err(anyhow!("fn:string:replace: requires 4 arguments (string, old, new, count)"));
            }
            let s = match &vals[0] {
                Value::String(s) => s,
                v => return Err(anyhow!("fn:string:replace: first arg must be string, got {v}")),
            };
            let old = match &vals[1] {
                Value::String(s) => s,
                v => return Err(anyhow!("fn:string:replace: second arg must be string, got {v}")),
            };
            let new_s = match &vals[2] {
                Value::String(s) => s,
                v => return Err(anyhow!("fn:string:replace: third arg must be string, got {v}")),
            };
            let count = match &vals[3] {
                Value::Number(n) => *n,
                v => return Err(anyhow!("fn:string:replace: fourth arg must be number, got {v}")),
            };
            let result = if count < 0 {
                s.replace(old.as_str(), new_s.as_str())
            } else {
                s.replacen(old.as_str(), new_s.as_str(), count as usize)
            };
            Ok(Value::String(result))
        }

        // --- Type conversion functions ---
        "fn:number:to_string" => {
            if vals.len() != 1 {
                return Err(anyhow!("fn:number:to_string: requires 1 argument"));
            }
            match &vals[0] {
                Value::Number(n) => Ok(Value::String(n.to_string())),
                v => Err(anyhow!("fn:number:to_string: expected number, got {v}")),
            }
        }
        "fn:float64:to_string" => {
            if vals.len() != 1 {
                return Err(anyhow!("fn:float64:to_string: requires 1 argument"));
            }
            match &vals[0] {
                Value::Float(f) => Ok(Value::String(format!("{f}"))),
                v => Err(anyhow!("fn:float64:to_string: expected float, got {v}")),
            }
        }
        "fn:name:to_string" => {
            if vals.len() != 1 {
                return Err(anyhow!("fn:name:to_string: requires 1 argument"));
            }
            match &vals[0] {
                Value::Name(s) => Ok(Value::String(s.clone())),
                v => Err(anyhow!("fn:name:to_string: expected name, got {v}")),
            }
        }

        // --- Time functions ---
        "fn:time:now" => {
            if !vals.is_empty() {
                return Err(anyhow!("fn:time:now: takes no arguments"));
            }
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| anyhow!("fn:time:now: {e}"))?
                .as_nanos() as i64;
            Ok(Value::Time(nanos))
        }
        "fn:time:add" => {
            if vals.len() != 2 {
                return Err(anyhow!("fn:time:add: requires 2 arguments (time, duration)"));
            }
            match (&vals[0], &vals[1]) {
                (Value::Time(t), Value::Duration(d)) => Ok(Value::Time(t + d)),
                _ => Err(anyhow!("fn:time:add: expected (time, duration), got ({}, {})", vals[0], vals[1])),
            }
        }
        "fn:time:sub" => {
            if vals.len() != 2 {
                return Err(anyhow!("fn:time:sub: requires 2 arguments"));
            }
            match (&vals[0], &vals[1]) {
                (Value::Time(t1), Value::Time(t2)) => Ok(Value::Duration(t1 - t2)),
                (Value::Time(t), Value::Duration(d)) => Ok(Value::Time(t - d)),
                _ => Err(anyhow!("fn:time:sub: expected (time, time) or (time, duration)")),
            }
        }
        "fn:time:year" => time_component(vals, |secs, _| {
            let (y, _, _) = civil_from_epoch_secs(secs);
            y as i64
        }),
        "fn:time:month" => time_component(vals, |secs, _| {
            let (_, m, _) = civil_from_epoch_secs(secs);
            m as i64
        }),
        "fn:time:day" => time_component(vals, |secs, _| {
            let (_, _, d) = civil_from_epoch_secs(secs);
            d as i64
        }),
        "fn:time:hour" => time_component(vals, |secs, _| {
            secs.rem_euclid(86400) / 3600
        }),
        "fn:time:minute" => time_component(vals, |secs, _| {
            (secs.rem_euclid(86400) % 3600) / 60
        }),
        "fn:time:second" => time_component(vals, |secs, _| {
            secs.rem_euclid(86400) % 60
        }),
        "fn:time:from_unix_nanos" => {
            if vals.len() != 1 {
                return Err(anyhow!("fn:time:from_unix_nanos: requires 1 argument"));
            }
            match &vals[0] {
                Value::Number(n) => Ok(Value::Time(*n)),
                v => Err(anyhow!("fn:time:from_unix_nanos: expected number, got {v}")),
            }
        }
        "fn:time:to_unix_nanos" => {
            if vals.len() != 1 {
                return Err(anyhow!("fn:time:to_unix_nanos: requires 1 argument"));
            }
            match &vals[0] {
                Value::Time(t) => Ok(Value::Number(*t)),
                v => Err(anyhow!("fn:time:to_unix_nanos: expected time, got {v}")),
            }
        }
        "fn:time:trunc" => {
            if vals.len() != 2 {
                return Err(anyhow!("fn:time:trunc: requires 2 arguments (time, unit_name)"));
            }
            let t = match &vals[0] {
                Value::Time(t) => *t,
                v => return Err(anyhow!("fn:time:trunc: first arg must be time, got {v}")),
            };
            let unit_name = match &vals[1] {
                Value::Name(s) => s.as_str(),
                v => return Err(anyhow!("fn:time:trunc: second arg must be name, got {v}")),
            };
            let d: i64 = match unit_name {
                "/nanosecond" => 1,
                "/microsecond" => 1_000,
                "/millisecond" => 1_000_000,
                "/second" => 1_000_000_000,
                "/minute" => 60 * 1_000_000_000,
                "/hour" => 3600 * 1_000_000_000,
                "/day" => 24 * 3600 * 1_000_000_000,
                _ => return Err(anyhow!("fn:time:trunc: unknown unit {unit_name:?}")),
            };
            Ok(Value::Time(t - t.rem_euclid(d)))
        }
        "fn:time:format" => {
            if vals.len() != 2 {
                return Err(anyhow!("fn:time:format: requires 2 arguments (time, precision)"));
            }
            let t = match &vals[0] {
                Value::Time(t) => *t,
                v => return Err(anyhow!("fn:time:format: first arg must be time, got {v}")),
            };
            let precision = match &vals[1] {
                Value::String(s) => s.as_str(),
                v => return Err(anyhow!("fn:time:format: second arg must be name, got {v}")),
            };
            Ok(Value::String(format_time_with_precision(t, precision)?))
        }
        "fn:time:format_civil" => {
            if vals.len() != 3 {
                return Err(anyhow!("fn:time:format_civil: requires 3 arguments (time, timezone, precision)"));
            }
            let t = match &vals[0] {
                Value::Time(t) => *t,
                v => return Err(anyhow!("fn:time:format_civil: first arg must be time, got {v}")),
            };
            let tz = match &vals[1] {
                Value::String(s) => s.as_str(),
                v => return Err(anyhow!("fn:time:format_civil: second arg must be string, got {v}")),
            };
            let precision = match &vals[2] {
                Value::String(s) => s.as_str(),
                v => return Err(anyhow!("fn:time:format_civil: third arg must be name, got {v}")),
            };
            let offset = parse_timezone_offset(tz)?;
            let adjusted = t + offset * 1_000_000_000;
            let formatted = format_time_with_precision(adjusted, precision)?;
            Ok(Value::String(formatted))
        }
        "fn:time:parse_rfc3339" => {
            if vals.len() != 1 {
                return Err(anyhow!("fn:time:parse_rfc3339: requires 1 argument"));
            }
            match &vals[0] {
                Value::String(s) => {
                    let nanos = parse_rfc3339_to_nanos(s)?;
                    Ok(Value::Time(nanos))
                }
                v => Err(anyhow!("fn:time:parse_rfc3339: expected string, got {v}")),
            }
        }
        "fn:time:parse_civil" => {
            if vals.len() != 2 {
                return Err(anyhow!("fn:time:parse_civil: requires 2 arguments (string, timezone)"));
            }
            let s = match &vals[0] {
                Value::String(s) => s.as_str(),
                v => return Err(anyhow!("fn:time:parse_civil: first arg must be string, got {v}")),
            };
            let tz = match &vals[1] {
                Value::String(s) => s.as_str(),
                v => return Err(anyhow!("fn:time:parse_civil: second arg must be string, got {v}")),
            };
            let offset = parse_timezone_offset(tz)?;
            let nanos = parse_civil_datetime_to_nanos(s)?;
            // Subtract offset to convert local time to UTC
            Ok(Value::Time(nanos - offset * 1_000_000_000))
        }

        // --- Duration functions ---
        "fn:duration:add" => {
            if vals.len() != 2 {
                return Err(anyhow!("fn:duration:add: requires 2 arguments"));
            }
            match (&vals[0], &vals[1]) {
                (Value::Duration(a), Value::Duration(b)) => Ok(Value::Duration(a + b)),
                _ => Err(anyhow!("fn:duration:add: expected (duration, duration)")),
            }
        }
        "fn:duration:mult" => {
            if vals.len() != 2 {
                return Err(anyhow!("fn:duration:mult: requires 2 arguments"));
            }
            match (&vals[0], &vals[1]) {
                (Value::Duration(d), Value::Number(n)) => Ok(Value::Duration(d * n)),
                (Value::Number(n), Value::Duration(d)) => Ok(Value::Duration(n * d)),
                _ => Err(anyhow!("fn:duration:mult: expected (duration, number) or (number, duration)")),
            }
        }
        "fn:duration:hours" => duration_component_float(vals, |nanos| nanos as f64 / (60.0 * 60.0 * 1_000_000_000.0)),
        "fn:duration:minutes" => duration_component_float(vals, |nanos| nanos as f64 / (60.0 * 1_000_000_000.0)),
        "fn:duration:seconds" => duration_component_float(vals, |nanos| nanos as f64 / 1_000_000_000.0),
        "fn:duration:nanos" => duration_component_int(vals, |nanos| nanos),
        "fn:duration:from_nanos" => {
            if vals.len() != 1 {
                return Err(anyhow!("fn:duration:from_nanos: requires 1 argument"));
            }
            match &vals[0] {
                Value::Number(n) => Ok(Value::Duration(*n)),
                v => Err(anyhow!("fn:duration:from_nanos: expected number, got {v}")),
            }
        }
        "fn:duration:from_hours" => duration_from(vals, "hours", 60 * 60 * 1_000_000_000),
        "fn:duration:from_minutes" => duration_from(vals, "minutes", 60 * 1_000_000_000),
        "fn:duration:from_seconds" => duration_from(vals, "seconds", 1_000_000_000),
        "fn:duration:parse" => {
            if vals.len() != 1 {
                return Err(anyhow!("fn:duration:parse: requires 1 argument"));
            }
            match &vals[0] {
                Value::String(s) => {
                    let nanos = parse_duration_string(s)?;
                    Ok(Value::Duration(nanos))
                }
                v => Err(anyhow!("fn:duration:parse: expected string, got {v}")),
            }
        }

        // --- Compound type constructors ---
        "fn:list" => Ok(Value::Compound(CompoundKind::List, vals.to_vec())),
        "fn:pair" => {
            if vals.len() != 2 {
                return Err(anyhow!("fn:pair: requires exactly 2 arguments"));
            }
            Ok(Value::Compound(CompoundKind::Pair, vals.to_vec()))
        }
        "fn:struct" => {
            // Args are interleaved: field_name, value, field_name, value, ...
            if vals.len() % 2 != 0 {
                return Err(anyhow!(
                    "fn:struct: requires even number of arguments (field, value pairs)"
                ));
            }
            Ok(Value::Compound(CompoundKind::Struct, vals.to_vec()))
        }
        "fn:map" => {
            // Args are interleaved: key, value, key, value, ...
            if vals.len() % 2 != 0 {
                return Err(anyhow!(
                    "fn:map: requires even number of arguments (key, value pairs)"
                ));
            }
            Ok(Value::Compound(CompoundKind::Map, vals.to_vec()))
        }

        // --- Compound type accessors ---
        "fn:list:get" => {
            if vals.len() != 2 {
                return Err(anyhow!("fn:list:get: requires 2 arguments (list, index)"));
            }
            match (&vals[0], &vals[1]) {
                (Value::Compound(_, elems), Value::Number(idx)) => {
                    let i = *idx as usize;
                    elems
                        .get(i)
                        .cloned()
                        .ok_or_else(|| anyhow!("fn:list:get: index {i} out of bounds (len {})", elems.len()))
                }
                _ => Err(anyhow!("fn:list:get: expected (compound, number)")),
            }
        }
        "fn:list:append" => {
            if vals.len() != 2 {
                return Err(anyhow!("fn:list:append: requires 2 arguments (list, elem)"));
            }
            match &vals[0] {
                Value::Compound(CompoundKind::List, elems) => {
                    let mut out = elems.clone();
                    out.push(vals[1].clone());
                    Ok(Value::Compound(CompoundKind::List, out))
                }
                _ => Err(anyhow!("fn:list:append: expected list as first argument")),
            }
        }
        "fn:list:len" | "fn:len" => {
            if vals.len() != 1 {
                return Err(anyhow!("fn:len: requires 1 argument"));
            }
            match &vals[0] {
                Value::Compound(_, elems) => Ok(Value::Number(elems.len() as i64)),
                _ => Err(anyhow!("fn:len: expected compound value")),
            }
        }
        "fn:pair:first" => {
            if vals.len() != 1 {
                return Err(anyhow!("fn:pair:first: requires 1 argument"));
            }
            match &vals[0] {
                Value::Compound(_, elems) if elems.len() >= 1 => Ok(elems[0].clone()),
                _ => Err(anyhow!("fn:pair:first: expected compound with at least 1 element")),
            }
        }
        "fn:pair:second" => {
            if vals.len() != 1 {
                return Err(anyhow!("fn:pair:second: requires 1 argument"));
            }
            match &vals[0] {
                Value::Compound(_, elems) if elems.len() >= 2 => Ok(elems[1].clone()),
                _ => Err(anyhow!("fn:pair:second: expected compound with at least 2 elements")),
            }
        }
        "fn:struct:get" | "fn:map:get" => {
            if vals.len() != 2 {
                return Err(anyhow!("{fn_name}: requires 2 arguments (compound, key)"));
            }
            match &vals[0] {
                Value::Compound(_, elems) => {
                    // Interleaved key-value pairs: [k1, v1, k2, v2, ...]
                    for pair in elems.chunks_exact(2) {
                        if pair[0] == vals[1] {
                            return Ok(pair[1].clone());
                        }
                    }
                    Err(anyhow!("{fn_name}: key not found"))
                }
                _ => Err(anyhow!("{fn_name}: expected compound value")),
            }
        }
        "fn:map:len" | "fn:struct:len" => {
            if vals.len() != 1 {
                return Err(anyhow!("{fn_name}: requires 1 argument"));
            }
            match &vals[0] {
                Value::Compound(_, elems) => Ok(Value::Number((elems.len() / 2) as i64)),
                _ => Err(anyhow!("{fn_name}: expected compound value")),
            }
        }
        "fn:map:keys" => {
            if vals.len() != 1 {
                return Err(anyhow!("fn:map:keys: requires 1 argument"));
            }
            match &vals[0] {
                Value::Compound(_, elems) => {
                    let keys: Vec<Value> = elems.chunks_exact(2).map(|p| p[0].clone()).collect();
                    Ok(Value::Compound(CompoundKind::List, keys))
                }
                _ => Err(anyhow!("fn:map:keys: expected compound value")),
            }
        }
        "fn:map:values" | "fn:struct:values" => {
            if vals.len() != 1 {
                return Err(anyhow!("{fn_name}: requires 1 argument"));
            }
            match &vals[0] {
                Value::Compound(_, elems) => {
                    let values: Vec<Value> = elems.chunks_exact(2).map(|p| p[1].clone()).collect();
                    Ok(Value::Compound(CompoundKind::List, values))
                }
                _ => Err(anyhow!("{fn_name}: expected compound value")),
            }
        }

        _ => Err(anyhow!("Unknown function: {fn_name}")),
    }
}

fn time_component(vals: &[Value], extract: impl Fn(i64, i64) -> i64) -> Result<Value> {
    if vals.len() != 1 {
        return Err(anyhow!("time component function: requires 1 argument"));
    }
    match &vals[0] {
        Value::Time(nanos) => {
            let secs = nanos.div_euclid(1_000_000_000);
            let sub_nanos = nanos.rem_euclid(1_000_000_000);
            Ok(Value::Number(extract(secs, sub_nanos)))
        }
        v => Err(anyhow!("time component function: expected time, got {v}")),
    }
}

fn civil_from_epoch_secs(secs: i64) -> (i32, u32, u32) {
    let days = secs.div_euclid(86400);
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
    (y as i32, m, d)
}

fn duration_component_float(vals: &[Value], extract: impl Fn(i64) -> f64) -> Result<Value> {
    if vals.len() != 1 {
        return Err(anyhow!("duration component function: requires 1 argument"));
    }
    match &vals[0] {
        Value::Duration(nanos) => Ok(Value::Float(extract(*nanos))),
        v => Err(anyhow!("duration component function: expected duration, got {v}")),
    }
}

fn duration_component_int(vals: &[Value], extract: impl Fn(i64) -> i64) -> Result<Value> {
    if vals.len() != 1 {
        return Err(anyhow!("duration component function: requires 1 argument"));
    }
    match &vals[0] {
        Value::Duration(nanos) => Ok(Value::Number(extract(*nanos))),
        v => Err(anyhow!("duration component function: expected duration, got {v}")),
    }
}

fn duration_from(vals: &[Value], name: &str, multiplier: i64) -> Result<Value> {
    if vals.len() != 1 {
        return Err(anyhow!("fn:duration:from_{name}: requires 1 argument"));
    }
    match &vals[0] {
        Value::Number(n) => Ok(Value::Duration(n * multiplier)),
        v => Err(anyhow!("fn:duration:from_{name}: expected number, got {v}")),
    }
}

/// Format a time (in UTC nanoseconds) with a given precision specifier.
fn format_time_with_precision(nanos: i64, precision: &str) -> Result<String> {
    let secs = nanos.div_euclid(1_000_000_000);
    let sub_nanos = nanos.rem_euclid(1_000_000_000);
    let (y, m, d) = civil_from_epoch_secs(secs);
    let time_of_day = secs.rem_euclid(86400);
    let hour = time_of_day / 3600;
    let minute = (time_of_day % 3600) / 60;
    let second = time_of_day % 60;

    match precision {
        "/year" => Ok(format!("{y:04}")),
        "/month" => Ok(format!("{y:04}-{m:02}")),
        "/day" => Ok(format!("{y:04}-{m:02}-{d:02}")),
        "/hour" => Ok(format!("{y:04}-{m:02}-{d:02}T{hour:02}Z")),
        "/minute" => Ok(format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}Z")),
        "/second" => Ok(format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")),
        "/millisecond" => {
            let ms = sub_nanos / 1_000_000;
            Ok(format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}.{ms:03}Z"))
        }
        "/microsecond" => {
            let us = sub_nanos / 1_000;
            Ok(format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}.{us:06}Z"))
        }
        "/nanosecond" => {
            if sub_nanos == 0 {
                Ok(format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z"))
            } else {
                let ns_str = format!("{sub_nanos:09}");
                let ns_trimmed = ns_str.trim_end_matches('0');
                Ok(format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}.{ns_trimmed}Z"))
            }
        }
        _ => Err(anyhow!("unknown time precision: {precision:?}")),
    }
}

/// Parse a timezone string. Supports "UTC" and fixed offsets like "+05:30", "-08:00".
/// Returns offset in seconds from UTC.
fn parse_timezone_offset(tz: &str) -> Result<i64> {
    match tz {
        "UTC" => Ok(0),
        s if s.starts_with('+') || s.starts_with('-') => {
            let sign: i64 = if s.starts_with('-') { -1 } else { 1 };
            let rest = &s[1..];
            let parts: Vec<&str> = rest.split(':').collect();
            if parts.len() != 2 {
                return Err(anyhow!("invalid timezone offset: {tz:?}"));
            }
            let hours: i64 = parts[0].parse().map_err(|_| anyhow!("invalid timezone: {tz:?}"))?;
            let minutes: i64 = parts[1].parse().map_err(|_| anyhow!("invalid timezone: {tz:?}"))?;
            Ok(sign * (hours * 3600 + minutes * 60))
        }
        _ => Err(anyhow!("unsupported timezone: {tz:?} (use \"UTC\" or offset like \"+05:30\")")),
    }
}

/// Parse an RFC3339-like timestamp string to nanoseconds since epoch.
fn parse_rfc3339_to_nanos(s: &str) -> Result<i64> {
    // Minimal RFC3339: "2006-01-02T15:04:05Z" or with fractional seconds
    if s.len() < 10 {
        return Err(anyhow!("fn:time:parse_rfc3339: string too short: {s:?}"));
    }
    let year: i64 = s[0..4].parse().map_err(|_| anyhow!("invalid year in {s:?}"))?;
    let month: u32 = s[5..7].parse().map_err(|_| anyhow!("invalid month in {s:?}"))?;
    let day: u32 = s[8..10].parse().map_err(|_| anyhow!("invalid day in {s:?}"))?;

    let (hour, minute, second, frac_nanos) = if s.len() > 10 && s.as_bytes()[10] == b'T' {
        if s.len() < 19 {
            return Err(anyhow!("fn:time:parse_rfc3339: incomplete time in {s:?}"));
        }
        let h: u32 = s[11..13].parse().map_err(|_| anyhow!("invalid hour"))?;
        let min: u32 = s[14..16].parse().map_err(|_| anyhow!("invalid minute"))?;
        let sec: u32 = s[17..19].parse().map_err(|_| anyhow!("invalid second"))?;

        let frac = if s.len() > 19 && s.as_bytes()[19] == b'.' {
            let end = s.len() - if s.ends_with('Z') { 1 } else { 0 };
            let frac_str = &s[20..end];
            let padded = format!("{frac_str:0<9}");
            padded[..9].parse::<i64>().unwrap_or(0)
        } else {
            0
        };
        (h, min, sec, frac)
    } else {
        (0, 0, 0, 0)
    };

    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u32;
    let m_adj = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * m_adj + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe as i64 - 719468;

    let total_seconds = days * 86400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64;
    Ok(total_seconds * 1_000_000_000 + frac_nanos)
}

/// Parse a civil datetime string like "2024-01-15T10:30:00" or "2024-01-15T10:30:00.000000000".
fn parse_civil_datetime_to_nanos(s: &str) -> Result<i64> {
    // Reuse RFC3339 parser — civil format is the same without 'Z' suffix
    parse_rfc3339_to_nanos(s)
}

/// Parse a Go-style duration string like "1h30m", "500ms", "-2h45m30s", "1.5h".
fn parse_duration_string(s: &str) -> Result<i64> {
    if s.is_empty() {
        return Err(anyhow!("fn:duration:parse: empty string"));
    }
    if s == "0" || s == "0s" {
        return Ok(0);
    }

    let (sign, mut rest) = if s.starts_with('-') {
        (-1i64, &s[1..])
    } else if s.starts_with('+') {
        (1i64, &s[1..])
    } else {
        (1i64, s)
    };

    let mut total_nanos: i64 = 0;

    while !rest.is_empty() {
        // Parse numeric part (integer or float)
        let num_end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        if num_end == 0 {
            return Err(anyhow!("fn:duration:parse: expected number in {s:?}"));
        }
        let num_str = &rest[..num_end];
        rest = &rest[num_end..];

        // Parse unit suffix
        let (unit_nanos, unit_len) = if rest.starts_with("ns") {
            (1i64, 2)
        } else if rest.starts_with("us") || rest.starts_with("µs") {
            (1_000i64, if rest.starts_with("µ") { 3 } else { 2 })
        } else if rest.starts_with("ms") {
            (1_000_000i64, 2)
        } else if rest.starts_with('s') {
            (1_000_000_000i64, 1)
        } else if rest.starts_with('m') {
            (60 * 1_000_000_000i64, 1)
        } else if rest.starts_with('h') {
            (3600 * 1_000_000_000i64, 1)
        } else {
            return Err(anyhow!("fn:duration:parse: unknown unit in {s:?}"));
        };
        rest = &rest[unit_len..];

        if num_str.contains('.') {
            let val: f64 = num_str.parse().map_err(|_| anyhow!("fn:duration:parse: invalid number {num_str:?}"))?;
            total_nanos += (val * unit_nanos as f64) as i64;
        } else {
            let val: i64 = num_str.parse().map_err(|_| anyhow!("fn:duration:parse: invalid number {num_str:?}"))?;
            total_nanos += val * unit_nanos;
        }
    }

    Ok(sign * total_nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retract_existing() {
        let mut store = MemStore::new();
        store.add_fact("r", vec![Value::Number(1), Value::Number(2)]);
        store.add_fact("r", vec![Value::Number(3), Value::Number(4)]);

        let removed = store
            .retract("r", &[Value::Number(1), Value::Number(2)])
            .unwrap();
        assert!(removed);

        let facts = store.get_facts("r");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0], vec![Value::Number(3), Value::Number(4)]);
    }

    #[test]
    fn test_retract_nonexistent() {
        let mut store = MemStore::new();
        store.add_fact("r", vec![Value::Number(1)]);

        let removed = store.retract("r", &[Value::Number(99)]).unwrap();
        assert!(!removed);

        let facts = store.get_facts("r");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0], vec![Value::Number(1)]);
    }

    #[test]
    fn test_clear() {
        let mut store = MemStore::new();
        store.add_fact("r", vec![Value::Number(1)]);
        store.add_fact("r", vec![Value::Number(2)]);
        store.add_fact("s", vec![Value::Number(10)]);

        store.clear("r");

        let r_facts = store.get_facts("r");
        assert!(r_facts.is_empty());

        // "s" should be untouched
        let s_facts = store.get_facts("s");
        assert_eq!(s_facts.len(), 1);
    }

    #[test]
    fn test_relation_names() {
        let mut store = MemStore::new();
        store.create_relation("alpha");
        store.create_relation("beta");
        store.add_fact("gamma", vec![Value::Number(1)]);

        let mut names = store.relation_names();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn test_provenance_recording() {
        use mangle_ir::physical::{DataSource, Operand};

        // Build a minimal IR manually to test provenance
        let mut ir = mangle_ir::Ir::new();
        let base_name = ir.intern_name("base");
        let derived_name = ir.intern_name("derived");
        let var_x = ir.intern_name("X");

        // Create an Op: Iterate(Scan("base", [X]), Insert("derived", [X]))
        let op = Op::Iterate {
            source: DataSource::Scan {
                relation: base_name,
                vars: vec![var_x],
            },
            body: Box::new(Op::Insert {
                relation: derived_name,
                args: vec![Operand::Var(var_x)],
            }),
        };

        let mut store = Box::new(MemStore::new());
        store.add_fact("base", vec![Value::Number(10)]);
        store.add_fact("base", vec![Value::Number(20)]);
        store.create_relation("derived");

        let mut interpreter = Interpreter::new(&ir, store as Box<dyn Store>).with_provenance();

        let count = interpreter.execute(&op).unwrap();
        assert_eq!(count, 2);

        // Check provenance was recorded
        let prov = interpreter.provenance.as_ref().unwrap();
        assert_eq!(prov.entries.len(), 2);

        // Each derived fact should have one premise (from "base")
        for entry in &prov.entries {
            assert_eq!(entry.derived.0, "derived");
            assert_eq!(entry.premises.len(), 1);
            assert_eq!(entry.premises[0].0, "base");
        }

        // Check the actual derived facts
        let mut derived_vals: Vec<i64> = prov
            .entries
            .iter()
            .map(|e| match &e.derived.1[0] {
                Value::Number(n) => *n,
                _ => panic!("expected number"),
            })
            .collect();
        derived_vals.sort();
        assert_eq!(derived_vals, vec![10, 20]);
    }

    #[test]
    fn test_float_values() {
        use mangle_ir::physical::{self, DataSource, Expr, Operand};

        let mut ir = mangle_ir::Ir::new();
        let temps = ir.intern_name("temps");
        let result = ir.intern_name("result");
        let var_x = ir.intern_name("X");
        // First, verify basic scan+insert of floats works
        let simple_op = Op::Iterate {
            source: DataSource::Scan {
                relation: temps,
                vars: vec![var_x],
            },
            body: Box::new(Op::Insert {
                relation: result,
                args: vec![Operand::Var(var_x)],
            }),
        };

        let mut store = Box::new(MemStore::new());
        store.add_fact("temps", vec![Value::Float(36.5)]);
        store.add_fact("temps", vec![Value::Float(35.9)]);
        store.add_fact("temps", vec![Value::Float(37.2)]);
        store.create_relation("result");

        let mut interpreter = Interpreter::new(&ir, store as Box<dyn Store>);
        let count = interpreter.execute(&simple_op).unwrap();
        assert_eq!(count, 3, "basic float scan+insert should produce 3 facts");

        // Now test with filter and arithmetic
        let mut ir2 = mangle_ir::Ir::new();
        let temps2 = ir2.intern_name("temps");
        let result2 = ir2.intern_name("result2");
        let var_x2 = ir2.intern_name("X");
        let var_y2 = ir2.intern_name("Y");
        let fn_plus2 = ir2.intern_name("fn:float:plus");

        let op = Op::Iterate {
            source: DataSource::Scan {
                relation: temps2,
                vars: vec![var_x2],
            },
            body: Box::new(Op::Filter {
                cond: Condition::Cmp {
                    op: physical::CmpOp::Gt,
                    left: Operand::Var(var_x2),
                    right: Operand::Const(physical::Constant::Float(36.0)),
                },
                body: Box::new(Op::Let {
                    var: var_y2,
                    expr: Expr::Call {
                        function: fn_plus2,
                        args: vec![
                            Operand::Var(var_x2),
                            Operand::Const(physical::Constant::Float(0.5)),
                        ],
                    },
                    body: Box::new(Op::Insert {
                        relation: result2,
                        args: vec![Operand::Var(var_x2), Operand::Var(var_y2)],
                    }),
                }),
            }),
        };

        let mut store2 = Box::new(MemStore::new());
        store2.add_fact("temps", vec![Value::Float(36.5)]);
        store2.add_fact("temps", vec![Value::Float(35.9)]);
        store2.add_fact("temps", vec![Value::Float(37.2)]);
        store2.create_relation("result2");

        let mut interpreter2 = Interpreter::new(&ir2, store2 as Box<dyn Store>);
        let count2 = interpreter2.execute(&op).unwrap();

        // Only 36.5 and 37.2 are > 36.0
        assert_eq!(count2, 2);

        // Results are in next_delta, check via scan_next_delta
        let results: Vec<_> = interpreter2
            .store()
            .scan_next_delta("result2")
            .unwrap()
            .collect();
        assert_eq!(results.len(), 2);

        let mut output: Vec<(f64, f64)> = results
            .iter()
            .map(|t| match (&t[0], &t[1]) {
                (Value::Float(a), Value::Float(b)) => (*a, *b),
                _ => panic!("expected floats"),
            })
            .collect();
        output.sort_by(|a, b| a.0.total_cmp(&b.0));

        assert_eq!(output[0], (36.5, 37.0));
        assert_eq!(output[1], (37.2, 37.7));
    }

    #[test]
    fn test_float_in_memstore() {
        // Test that Float values work correctly as HashMap keys (equality, hashing)
        let mut store = MemStore::new();
        store.add_fact("data", vec![Value::Float(1.5), Value::Number(10)]);
        store.add_fact("data", vec![Value::Float(2.5), Value::Number(20)]);
        // Duplicate should not be added
        store.add_fact("data", vec![Value::Float(1.5), Value::Number(10)]);

        let facts = store.get_facts("data");
        assert_eq!(facts.len(), 2);

        // Test index lookup
        let results: Vec<_> = store
            .scan_index("data", 0, &Value::Float(1.5))
            .unwrap()
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0][1], Value::Number(10));
    }

    // --- eval_function unit tests ---

    #[test]
    fn test_fn_plus_variadic() {
        assert_eq!(
            eval_function("fn:plus", &[Value::Number(1), Value::Number(2), Value::Number(3)]).unwrap(),
            Value::Number(6)
        );
        // Zero args returns 0 (identity)
        assert_eq!(eval_function("fn:plus", &[]).unwrap(), Value::Number(0));
    }

    #[test]
    fn test_fn_minus_variadic() {
        // Unary
        assert_eq!(
            eval_function("fn:minus", &[Value::Number(5)]).unwrap(),
            Value::Number(-5)
        );
        // Binary
        assert_eq!(
            eval_function("fn:minus", &[Value::Number(10), Value::Number(3)]).unwrap(),
            Value::Number(7)
        );
        // Variadic: 100 - 10 - 20 = 70
        assert_eq!(
            eval_function("fn:minus", &[Value::Number(100), Value::Number(10), Value::Number(20)]).unwrap(),
            Value::Number(70)
        );
        // Zero args is an error
        assert!(eval_function("fn:minus", &[]).is_err());
    }

    #[test]
    fn test_fn_mult_variadic() {
        assert_eq!(
            eval_function("fn:mult", &[Value::Number(2), Value::Number(3), Value::Number(4)]).unwrap(),
            Value::Number(24)
        );
        // Zero args returns 1 (identity)
        assert_eq!(eval_function("fn:mult", &[]).unwrap(), Value::Number(1));
    }

    #[test]
    fn test_fn_div() {
        // Binary
        assert_eq!(
            eval_function("fn:div", &[Value::Number(10), Value::Number(3)]).unwrap(),
            Value::Number(3)
        );
        // Unary: 1/n
        assert_eq!(
            eval_function("fn:div", &[Value::Number(5)]).unwrap(),
            Value::Number(0)
        );
        assert_eq!(
            eval_function("fn:div", &[Value::Number(1)]).unwrap(),
            Value::Number(1)
        );
        // Division by zero
        assert!(eval_function("fn:div", &[Value::Number(1), Value::Number(0)]).is_err());
        assert!(eval_function("fn:div", &[Value::Number(0)]).is_err());
    }

    #[test]
    fn test_fn_float_promotion() {
        // fn:float:plus accepts both Float and Number
        assert_eq!(
            eval_function("fn:float:plus", &[Value::Float(1.5), Value::Number(2)]).unwrap(),
            Value::Float(3.5)
        );
        // fn:sqrt accepts Number
        assert_eq!(
            eval_function("fn:sqrt", &[Value::Number(9)]).unwrap(),
            Value::Float(3.0)
        );
    }

    #[test]
    fn test_fn_string_concat() {
        assert_eq!(
            eval_function(
                "fn:string:concat",
                &[Value::String("a".into()), Value::String("b".into()), Value::String("c".into())]
            ).unwrap(),
            Value::String("abc".to_string())
        );
        // Mixed types
        assert_eq!(
            eval_function(
                "fn:string:concat",
                &[Value::String("n=".into()), Value::Number(42)]
            ).unwrap(),
            Value::String("n=42".to_string())
        );
    }

    #[test]
    fn test_fn_string_replace() {
        // Replace all (-1)
        assert_eq!(
            eval_function(
                "fn:string:replace",
                &[Value::String("a-b-c".into()), Value::String("-".into()), Value::String("_".into()), Value::Number(-1)]
            ).unwrap(),
            Value::String("a_b_c".to_string())
        );
        // Replace first only (1)
        assert_eq!(
            eval_function(
                "fn:string:replace",
                &[Value::String("a-b-c".into()), Value::String("-".into()), Value::String("_".into()), Value::Number(1)]
            ).unwrap(),
            Value::String("a_b-c".to_string())
        );
    }

    #[test]
    fn test_fn_to_string() {
        assert_eq!(
            eval_function("fn:number:to_string", &[Value::Number(42)]).unwrap(),
            Value::String("42".to_string())
        );
        assert_eq!(
            eval_function("fn:float64:to_string", &[Value::Float(3.14)]).unwrap(),
            Value::String("3.14".to_string())
        );
        assert_eq!(
            eval_function("fn:name:to_string", &[Value::Name("/role/admin".into())]).unwrap(),
            Value::String("/role/admin".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // Temporal coalescing tests (ported from Go factstore/temporal_test.go)
    // -----------------------------------------------------------------------

    // Helper: create a time value in nanoseconds for a date
    fn date_nanos(year: i64, month: u32, day: u32) -> i64 {
        // Howard Hinnant's algorithm (matching the parser)
        let m = month;
        let y = if m <= 2 { year - 1 } else { year };
        let era = (if y >= 0 { y } else { y - 399 }) / 400;
        let yoe = (y - era * 400) as u32;
        let m_adj = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * m_adj + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146097 + doe as i64 - 719468;
        days * 86400 * 1_000_000_000
    }

    fn datetime_nanos(year: i64, month: u32, day: u32, h: u32, m: u32, s: u32, ns: i64) -> i64 {
        date_nanos(year, month, day) + (h as i64) * 3_600_000_000_000
            + (m as i64) * 60_000_000_000 + (s as i64) * 1_000_000_000 + ns
    }

    /// Go: TestTemporalStore_Coalesce - overlapping intervals merge into one
    #[test]
    fn test_coalesce_overlapping() {
        let mut store = MemStore::new();
        let jan1 = date_nanos(2024, 1, 1);
        let jan15 = date_nanos(2024, 1, 15);
        let jan10 = date_nanos(2024, 1, 10);
        let jan25 = date_nanos(2024, 1, 25);
        let jan20 = date_nanos(2024, 1, 20);
        let jan31 = date_nanos(2024, 1, 31);

        store.add_fact("active", vec![Value::String("/service".into()), Value::Time(jan1), Value::Time(jan15)]);
        store.add_fact("active", vec![Value::String("/service".into()), Value::Time(jan10), Value::Time(jan25)]);
        store.add_fact("active", vec![Value::String("/service".into()), Value::Time(jan20), Value::Time(jan31)]);

        assert_eq!(store.get_facts("active").len(), 3);

        store.coalesce_temporal("active");

        let facts = store.get_facts("active");
        assert_eq!(facts.len(), 1, "after coalesce: expected 1, got {:?}", facts);
        assert_eq!(facts[0][1], Value::Time(jan1), "start should be Jan 1");
        assert_eq!(facts[0][2], Value::Time(jan31), "end should be Jan 31");
    }

    /// Go: TestTemporalStore_CoalesceAdjacent - adjacent intervals (1ns gap) coalesce
    #[test]
    fn test_coalesce_adjacent() {
        let mut store = MemStore::new();
        let shift1_start = datetime_nanos(2024, 1, 1, 8, 0, 0, 0);
        let shift1_end = datetime_nanos(2024, 1, 1, 16, 0, 0, 0);
        let shift2_start = datetime_nanos(2024, 1, 1, 16, 0, 0, 1); // 1ns after
        let shift2_end = date_nanos(2024, 1, 2);

        store.add_fact("shift", vec![Value::String("/worker".into()), Value::Time(shift1_start), Value::Time(shift1_end)]);
        store.add_fact("shift", vec![Value::String("/worker".into()), Value::Time(shift2_start), Value::Time(shift2_end)]);

        store.coalesce_temporal("shift");

        let facts = store.get_facts("shift");
        assert_eq!(facts.len(), 1, "adjacent intervals should coalesce");
        assert_eq!(facts[0][1], Value::Time(shift1_start));
        assert_eq!(facts[0][2], Value::Time(shift2_end));
    }

    /// Go: TestTemporalStore_CoalesceNonOverlapping - non-overlapping intervals stay separate
    #[test]
    fn test_coalesce_non_overlapping() {
        let mut store = MemStore::new();
        let jan1 = date_nanos(2024, 1, 1);
        let jan7 = date_nanos(2024, 1, 7);
        let jun1 = date_nanos(2024, 6, 1);
        let jun14 = date_nanos(2024, 6, 14);

        store.add_fact("vacation", vec![Value::String("/alice".into()), Value::Time(jan1), Value::Time(jan7)]);
        store.add_fact("vacation", vec![Value::String("/alice".into()), Value::Time(jun1), Value::Time(jun14)]);

        store.coalesce_temporal("vacation");

        let facts = store.get_facts("vacation");
        assert_eq!(facts.len(), 2, "non-overlapping intervals should stay separate");
    }

    /// Go: TestTemporalStore_CoalesceMixedGranularity - sub-second precision
    #[test]
    fn test_coalesce_mixed_granularity() {
        let mut store = MemStore::new();
        // Second-level: 10:00:00 to 10:00:05
        let t1_start = datetime_nanos(2024, 1, 1, 10, 0, 0, 0);
        let t1_end = datetime_nanos(2024, 1, 1, 10, 0, 5, 0);
        // Overlapping millisecond-level: 10:00:04.5 to 10:00:06
        let t2_start = datetime_nanos(2024, 1, 1, 10, 0, 4, 500_000_000);
        let t2_end = datetime_nanos(2024, 1, 1, 10, 0, 6, 0);
        // Adjacent nanosecond-level: 10:00:06.000000001 to 10:00:07
        let t3_start = datetime_nanos(2024, 1, 1, 10, 0, 6, 1);
        let t3_end = datetime_nanos(2024, 1, 1, 10, 0, 7, 0);

        store.add_fact("event", vec![Value::String("/sensor".into()), Value::Time(t1_start), Value::Time(t1_end)]);
        store.add_fact("event", vec![Value::String("/sensor".into()), Value::Time(t2_start), Value::Time(t2_end)]);
        store.add_fact("event", vec![Value::String("/sensor".into()), Value::Time(t3_start), Value::Time(t3_end)]);

        store.coalesce_temporal("event");

        let facts = store.get_facts("event");
        assert_eq!(facts.len(), 1, "mixed granularity should coalesce to 1");
        assert_eq!(facts[0][1], Value::Time(t1_start), "start: 10:00:00");
        assert_eq!(facts[0][2], Value::Time(t3_end), "end: 10:00:07");
    }

    /// Test coalescing with multiple keys: same relation, different key columns
    #[test]
    fn test_coalesce_multiple_keys() {
        let mut store = MemStore::new();
        let jan1 = date_nanos(2024, 1, 1);
        let jan10 = date_nanos(2024, 1, 10);
        let jan5 = date_nanos(2024, 1, 5);
        let jan15 = date_nanos(2024, 1, 15);

        // Alice: two overlapping intervals
        store.add_fact("employed", vec![Value::String("/alice".into()), Value::Time(jan1), Value::Time(jan10)]);
        store.add_fact("employed", vec![Value::String("/alice".into()), Value::Time(jan5), Value::Time(jan15)]);
        // Bob: one interval
        store.add_fact("employed", vec![Value::String("/bob".into()), Value::Time(jan1), Value::Time(jan15)]);

        store.coalesce_temporal("employed");

        let facts = store.get_facts("employed");
        // Alice's 2 intervals merge to 1; Bob stays as 1
        assert_eq!(facts.len(), 2, "expected 2 facts after coalesce, got {:?}", facts);
    }

    // --- HashJoin tests ---------------------------------------------------
    //
    // Each test hand-constructs an `Op::HashJoin` and compares its output to
    // the equivalent nested-loop plan (`Op::Iterate { body: Op::Iterate {..}
    // }` with an equality check). This isolates HashJoin correctness from
    // planner emission, which lives in a separate gated path.

    /// Build `result(X, Y) :- a(X, Z), b(Z, Y).` as nested-loop (join-key
    /// check via Filter on Z) — the correctness baseline for HashJoin.
    fn nested_loop_two_way(
        ir: &mangle_ir::Ir,
        a: NameId,
        b: NameId,
        result: NameId,
        x: NameId,
        z: NameId,
        y: NameId,
        z_right: NameId,
    ) -> Op {
        use mangle_ir::physical::{CmpOp, Condition, DataSource, Operand};
        let _ = ir;
        Op::Iterate {
            source: DataSource::Scan {
                relation: a,
                vars: vec![x, z],
            },
            body: Box::new(Op::Iterate {
                source: DataSource::Scan {
                    relation: b,
                    vars: vec![z_right, y],
                },
                body: Box::new(Op::Filter {
                    cond: Condition::Cmp {
                        op: CmpOp::Eq,
                        left: Operand::Var(z),
                        right: Operand::Var(z_right),
                    },
                    body: Box::new(Op::Insert {
                        relation: result,
                        args: vec![Operand::Var(x), Operand::Var(y)],
                    }),
                }),
            }),
        }
    }

    /// Same rule as `nested_loop_two_way` but with HashJoin.
    fn hash_join_two_way(
        a: NameId,
        b: NameId,
        result: NameId,
        x: NameId,
        z: NameId,
        y: NameId,
    ) -> Op {
        use mangle_ir::physical::{DataSource, Operand};
        Op::HashJoin {
            build_source: DataSource::Scan {
                relation: a,
                vars: vec![x, z],
            },
            probe_source: DataSource::Scan {
                relation: b,
                vars: vec![z, y],
            },
            join_keys: vec![z],
            body: Box::new(Op::Insert {
                relation: result,
                args: vec![Operand::Var(x), Operand::Var(y)],
            }),
        }
    }

    fn run_plan(ir: &mangle_ir::Ir, facts: &[(&str, Vec<Value>)], op: &Op) -> Vec<Vec<Value>> {
        let mut store = Box::new(MemStore::new());
        for (rel, t) in facts {
            store.add_fact(rel, t.clone());
        }
        store.create_relation("result");
        let mut interp = Interpreter::new(ir, store as Box<dyn Store>);
        interp.execute(op).unwrap();
        // Inserts land in next_delta; promote twice so `scan()` surfaces them.
        let mut store = interp.into_store();
        store.merge_deltas();
        store.merge_deltas();
        store.scan("result").unwrap().collect()
    }

    fn sorted(mut v: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
        v.sort();
        v
    }

    /// Build an IR with the names needed for the two-way-join tests.
    fn setup_two_way_ir()
    -> (mangle_ir::Ir, NameId, NameId, NameId, NameId, NameId, NameId, NameId) {
        let mut ir = mangle_ir::Ir::new();
        let a = ir.intern_name("a");
        let b = ir.intern_name("b");
        let result = ir.intern_name("result");
        let x = ir.intern_name("X");
        let z = ir.intern_name("Z");
        let y = ir.intern_name("Y");
        // A separate NameId for the nested-loop baseline's Z on the right side
        // — nested-loop needs a distinct var so the Filter can check equality.
        let z_right = ir.intern_name("Z_right");
        (ir, a, b, result, x, z, y, z_right)
    }

    #[test]
    fn test_hashjoin_matches_nested_loop_basic() {
        let (ir, a, b, result, x, z, y, z_right) = setup_two_way_ir();

        let facts: Vec<(&str, Vec<Value>)> = vec![
            ("a", vec![Value::Number(1), Value::Number(10)]),
            ("a", vec![Value::Number(2), Value::Number(20)]),
            ("a", vec![Value::Number(3), Value::Number(10)]),
            ("b", vec![Value::Number(10), Value::Number(100)]),
            ("b", vec![Value::Number(10), Value::Number(101)]),
            ("b", vec![Value::Number(20), Value::Number(200)]),
            ("b", vec![Value::Number(30), Value::Number(300)]),
        ];

        let baseline = run_plan(
            &ir,
            &facts,
            &nested_loop_two_way(&ir, a, b, result, x, z, y, z_right),
        );
        let via_hash = run_plan(&ir, &facts, &hash_join_two_way(a, b, result, x, z, y));

        assert_eq!(
            sorted(baseline.clone()),
            sorted(via_hash.clone()),
            "HashJoin output must match nested-loop baseline"
        );
        // Sanity: we expect (1,100), (1,101), (2,200), (3,100), (3,101).
        assert_eq!(sorted(via_hash).len(), 5);
    }

    #[test]
    fn test_hashjoin_empty_build() {
        let (ir, a, b, result, x, z, y, _z_right) = setup_two_way_ir();
        let facts: Vec<(&str, Vec<Value>)> = vec![
            ("b", vec![Value::Number(10), Value::Number(100)]),
            ("b", vec![Value::Number(20), Value::Number(200)]),
        ];
        let out = run_plan(&ir, &facts, &hash_join_two_way(a, b, result, x, z, y));
        assert!(out.is_empty());
    }

    #[test]
    fn test_hashjoin_empty_probe() {
        let (ir, a, b, result, x, z, y, _z_right) = setup_two_way_ir();
        let facts: Vec<(&str, Vec<Value>)> = vec![
            ("a", vec![Value::Number(1), Value::Number(10)]),
            ("a", vec![Value::Number(2), Value::Number(20)]),
        ];
        let out = run_plan(&ir, &facts, &hash_join_two_way(a, b, result, x, z, y));
        assert!(out.is_empty());
    }

    #[test]
    fn test_hashjoin_no_matches() {
        let (ir, a, b, result, x, z, y, _z_right) = setup_two_way_ir();
        let facts: Vec<(&str, Vec<Value>)> = vec![
            ("a", vec![Value::Number(1), Value::Number(10)]),
            ("b", vec![Value::Number(99), Value::Number(200)]),
        ];
        let out = run_plan(&ir, &facts, &hash_join_two_way(a, b, result, x, z, y));
        assert!(out.is_empty());
    }

    #[test]
    fn test_hashjoin_value_variants_as_key() {
        // Use strings and names as join keys to exercise value hashing on
        // non-integer variants. `r(X, Y) :- a(X, K), b(K, Y).`
        let mut ir = mangle_ir::Ir::new();
        let a = ir.intern_name("a");
        let b = ir.intern_name("b");
        let result = ir.intern_name("result");
        let x = ir.intern_name("X");
        let k = ir.intern_name("K");
        let y = ir.intern_name("Y");

        let facts: Vec<(&str, Vec<Value>)> = vec![
            ("a", vec![Value::Number(1), Value::String("hello".into())]),
            ("a", vec![Value::Number(2), Value::Name("/foo".into())]),
            ("b", vec![Value::String("hello".into()), Value::Number(100)]),
            ("b", vec![Value::Name("/foo".into()), Value::Number(200)]),
            // An entry that must NOT match — Name vs String are distinct.
            (
                "b",
                vec![Value::String("/foo".into()), Value::Number(999)],
            ),
        ];
        let op = hash_join_two_way(a, b, result, x, k, y);
        let out = sorted(run_plan(&ir, &facts, &op));
        assert_eq!(
            out,
            sorted(vec![
                vec![Value::Number(1), Value::Number(100)],
                vec![Value::Number(2), Value::Number(200)],
            ])
        );
    }

    #[test]
    fn test_hashjoin_multi_key() {
        // `r(X, W) :- a(X, K1, K2), b(K1, K2, W).` — compound (two-variable)
        // join key.
        let mut ir = mangle_ir::Ir::new();
        let a = ir.intern_name("a");
        let b = ir.intern_name("b");
        let result = ir.intern_name("result");
        let x = ir.intern_name("X");
        let k1 = ir.intern_name("K1");
        let k2 = ir.intern_name("K2");
        let w = ir.intern_name("W");

        let op = Op::HashJoin {
            build_source: DataSource::Scan {
                relation: a,
                vars: vec![x, k1, k2],
            },
            probe_source: DataSource::Scan {
                relation: b,
                vars: vec![k1, k2, w],
            },
            join_keys: vec![k1, k2],
            body: Box::new(Op::Insert {
                relation: result,
                args: vec![Operand::Var(x), Operand::Var(w)],
            }),
        };

        let facts: Vec<(&str, Vec<Value>)> = vec![
            (
                "a",
                vec![Value::Number(1), Value::Number(10), Value::Number(100)],
            ),
            (
                "a",
                vec![Value::Number(2), Value::Number(10), Value::Number(200)],
            ),
            // Matching K1 but different K2 — must NOT join with (10, 100, ...).
            (
                "a",
                vec![Value::Number(3), Value::Number(10), Value::Number(999)],
            ),
            (
                "b",
                vec![Value::Number(10), Value::Number(100), Value::Number(1000)],
            ),
            (
                "b",
                vec![Value::Number(10), Value::Number(200), Value::Number(2000)],
            ),
        ];

        let out = sorted(run_plan(&ir, &facts, &op));
        assert_eq!(
            out,
            sorted(vec![
                vec![Value::Number(1), Value::Number(1000)],
                vec![Value::Number(2), Value::Number(2000)],
            ])
        );
    }

    #[test]
    fn test_hashjoin_duplicate_build_keys() {
        // Multiple build rows with the same key must all be emitted (one
        // probe tuple × N build matches = N results).
        let (ir, a, b, result, x, z, y, _z_right) = setup_two_way_ir();
        let facts: Vec<(&str, Vec<Value>)> = vec![
            ("a", vec![Value::Number(1), Value::Number(10)]),
            ("a", vec![Value::Number(2), Value::Number(10)]),
            ("a", vec![Value::Number(3), Value::Number(10)]),
            ("b", vec![Value::Number(10), Value::Number(99)]),
        ];
        let op = hash_join_two_way(a, b, result, x, z, y);
        let out = sorted(run_plan(&ir, &facts, &op));
        assert_eq!(
            out,
            sorted(vec![
                vec![Value::Number(1), Value::Number(99)],
                vec![Value::Number(2), Value::Number(99)],
                vec![Value::Number(3), Value::Number(99)],
            ])
        );
    }
}
