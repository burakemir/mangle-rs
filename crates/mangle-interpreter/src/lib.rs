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

pub use mangle_common::{Store, Value};

/// A simple in-memory implementation of `Store`.
/// Supports semi-naive evaluation by tracking "stable" and "delta" facts.
#[derive(Default)]
pub struct MemStore {
    // Stable facts from previous iterations
    stable: HashMap<String, Vec<Vec<Value>>>,
    // New facts from the current iteration
    delta: HashMap<String, Vec<Vec<Value>>>,
    // Facts being collected for the next iteration
    next_delta: HashMap<String, Vec<Vec<Value>>>,

    // Secondary indexes: (relation_name, col_idx) -> { Value -> [row_indices] }
    // These only index stable facts for simplicity, or we re-build them.
    // Actually, let's index ALL facts (stable + delta).
    stable_indexes: HashMap<(String, usize), HashMap<Value, Vec<usize>>>,
    delta_indexes: HashMap<(String, usize), HashMap<Value, Vec<usize>>>,
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
        let table = self.stable.entry(relation.to_string()).or_default();
        if !table.contains(&args) {
            let row_idx = table.len();
            table.push(args.clone());
            // Update stable index
            for (col_idx, val) in args.into_iter().enumerate() {
                self.stable_indexes
                    .entry((relation.to_string(), col_idx))
                    .or_default()
                    .entry(val)
                    .or_default()
                    .push(row_idx);
            }
        }
    }

    /// Rebuilds stable and delta indexes for a single relation after mutation.
    fn rebuild_indexes_for(&mut self, relation: &str) {
        // Clear existing indexes for this relation
        self.stable_indexes.retain(|(rel, _), _| rel != relation);
        self.delta_indexes.retain(|(rel, _), _| rel != relation);

        // Rebuild stable indexes
        if let Some(table) = self.stable.get(relation) {
            for (row_idx, tuple) in table.iter().enumerate() {
                for (col_idx, val) in tuple.iter().enumerate() {
                    self.stable_indexes
                        .entry((relation.to_string(), col_idx))
                        .or_default()
                        .entry(val.clone())
                        .or_default()
                        .push(row_idx);
                }
            }
        }

        // Rebuild delta indexes
        if let Some(table) = self.delta.get(relation) {
            for (row_idx, tuple) in table.iter().enumerate() {
                for (col_idx, val) in tuple.iter().enumerate() {
                    self.delta_indexes
                        .entry((relation.to_string(), col_idx))
                        .or_default()
                        .entry(val.clone())
                        .or_default()
                        .push(row_idx);
                }
            }
        }
    }

    pub fn get_facts(&self, relation: &str) -> Vec<Vec<Value>> {
        let mut all = self.stable.get(relation).cloned().unwrap_or_default();
        if let Some(d) = self.delta.get(relation) {
            all.extend(d.iter().cloned());
        }
        all
    }
}

impl Store for MemStore {
    fn scan(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        let s = self.stable.get(relation).into_iter().flatten().cloned();
        let d = self.delta.get(relation).into_iter().flatten().cloned();
        Ok(Box::new(s.chain(d)))
    }

    fn scan_delta(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        match self.delta.get(relation) {
            Some(tuples) => Ok(Box::new(tuples.iter().cloned())),
            None => Ok(Box::new(std::iter::empty())),
        }
    }

    fn scan_next_delta(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        match self.next_delta.get(relation) {
            Some(tuples) => Ok(Box::new(tuples.iter().cloned())),
            None => Ok(Box::new(std::iter::empty())),
        }
    }

    fn scan_index(
        &self,
        relation: &str,
        col_idx: usize,
        key: &Value,
    ) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        let mut results: Vec<Vec<Value>> = Vec::new();

        if let Some(idx_map) = self.stable_indexes.get(&(relation.to_string(), col_idx))
            && let Some(row_indices) = idx_map.get(key)
            && let Some(table) = self.stable.get(relation)
        {
            for &i in row_indices {
                results.push(table[i].clone());
            }
        }

        if let Some(idx_map) = self.delta_indexes.get(&(relation.to_string(), col_idx))
            && let Some(row_indices) = idx_map.get(key)
            && let Some(table) = self.delta.get(relation)
        {
            for &i in row_indices {
                results.push(table[i].clone());
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
        let mut results: Vec<Vec<Value>> = Vec::new();

        if let Some(idx_map) = self.delta_indexes.get(&(relation.to_string(), col_idx))
            && let Some(row_indices) = idx_map.get(key)
            && let Some(table) = self.delta.get(relation)
        {
            for &i in row_indices {
                results.push(table[i].clone());
            }
        }

        Ok(Box::new(results.into_iter()))
    }

    fn insert(&mut self, relation: &str, tuple: Vec<Value>) -> Result<bool> {
        // Check if fact is already in stable, delta, or next_delta
        if self
            .stable
            .get(relation)
            .is_some_and(|v| v.contains(&tuple))
            || self.delta.get(relation).is_some_and(|v| v.contains(&tuple))
            || self
                .next_delta
                .get(relation)
                .is_some_and(|v| v.contains(&tuple))
        {
            return Ok(false);
        }

        self.next_delta
            .entry(relation.to_string())
            .or_default()
            .push(tuple);
        Ok(true)
    }

    fn merge_deltas(&mut self) {
        // 1. Move current delta to stable
        for (rel_name, mut tuples) in self.delta.drain() {
            let table = self.stable.entry(rel_name.clone()).or_default();
            for tuple in tuples.drain(..) {
                let row_idx = table.len();
                // Update stable index
                for (col_idx, val) in tuple.iter().enumerate() {
                    self.stable_indexes
                        .entry((rel_name.clone(), col_idx))
                        .or_default()
                        .entry(val.clone())
                        .or_default()
                        .push(row_idx);
                }
                table.push(tuple);
            }
        }
        self.delta_indexes.clear();

        // 2. Move next_delta to delta and build delta index
        self.delta = std::mem::take(&mut self.next_delta);
        for (rel_name, tuples) in &self.delta {
            for (row_idx, tuple) in tuples.iter().enumerate() {
                for (col_idx, val) in tuple.iter().enumerate() {
                    self.delta_indexes
                        .entry((rel_name.clone(), col_idx))
                        .or_default()
                        .entry(val.clone())
                        .or_default()
                        .push(row_idx);
                }
            }
        }
    }

    fn create_relation(&mut self, relation: &str) {
        self.stable.entry(relation.to_string()).or_default();
    }

    fn retract(&mut self, relation: &str, tuple: &[Value]) -> Result<bool> {
        let removed = if let Some(table) = self.stable.get_mut(relation) {
            if let Some(pos) = table.iter().position(|t| t.as_slice() == tuple) {
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
            if let Some(pos) = table.iter().position(|t| t.as_slice() == tuple) {
                table.swap_remove(pos);
            }
        }
        if let Some(table) = self.next_delta.get_mut(relation) {
            if let Some(pos) = table.iter().position(|t| t.as_slice() == tuple) {
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
        // Remove index entries for this relation
        self.stable_indexes.retain(|(rel, _), _| rel != relation);
        self.delta_indexes.retain(|(rel, _), _| rel != relation);
    }

    fn relation_names(&self) -> Vec<String> {
        self.stable.keys().cloned().collect()
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
                        _ => {}
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
                    match val {
                        Value::Float(f) => float_sum += f,
                        _ => {}
                    }
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
                    if let Value::Float(f) = val {
                        max_val = Some(match max_val {
                            None => f,
                            Some(m) => f.max(m),
                        });
                    }
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
                    if let Value::Float(f) = val {
                        min_val = Some(match min_val {
                            None => f,
                            Some(m) => f.min(m),
                        });
                    }
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
            Condition::Call { .. } => {
                // TODO: Implement boolean calls
                Ok(true)
            }
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
                match fn_name {
                    "fn:plus" => match (&vals[0], &vals[1]) {
                        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a + b)),
                        _ => Err(anyhow!("Type mismatch for fn:plus: expected integers")),
                    },
                    "fn:minus" => match (&vals[0], &vals[1]) {
                        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a - b)),
                        _ => Err(anyhow!("Type mismatch for fn:minus: expected integers")),
                    },
                    "fn:mult" => match (&vals[0], &vals[1]) {
                        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a * b)),
                        _ => Err(anyhow!("Type mismatch for fn:mult: expected integers")),
                    },
                    "fn:div" => match (&vals[0], &vals[1]) {
                        (Value::Number(_), Value::Number(0)) => {
                            Err(anyhow!("Division by zero in fn:div"))
                        }
                        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a / b)),
                        _ => Err(anyhow!("Type mismatch for fn:div: expected integers")),
                    },
                    "fn:float:plus" => match (&vals[0], &vals[1]) {
                        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
                        _ => Err(anyhow!("Type mismatch for fn:float:plus: expected floats")),
                    },
                    "fn:float:minus" => match (&vals[0], &vals[1]) {
                        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a - b)),
                        _ => Err(anyhow!("Type mismatch for fn:float:minus: expected floats")),
                    },
                    "fn:float:mult" => match (&vals[0], &vals[1]) {
                        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a * b)),
                        _ => Err(anyhow!("Type mismatch for fn:float:mult: expected floats")),
                    },
                    "fn:float:div" => match (&vals[0], &vals[1]) {
                        (Value::Float(_), Value::Float(b)) if *b == 0.0 => {
                            Err(anyhow!("Division by zero in fn:float:div"))
                        }
                        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a / b)),
                        _ => Err(anyhow!("Type mismatch for fn:float:div: expected floats")),
                    },
                    "fn:sqrt" => match &vals[0] {
                        Value::Float(a) => Ok(Value::Float(a.sqrt())),
                        _ => Err(anyhow!("Type mismatch for fn:sqrt: expected float")),
                    },
                    _ => Err(anyhow!("Unknown function: {fn_name}")),
                }
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
                Constant::Name(nid) => Ok(Value::String(self.ir.resolve_name(*nid).to_string())),
            },
        }
    }
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
}
