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

//! EDB source trait and supporting types.

use anyhow::Result;
use mangle_common::Value;

/// Metadata about a relation provided by an EDB source.
#[derive(Debug, Clone)]
pub struct RelationInfo {
    pub name: String,
    pub estimated_rows: usize,
}

/// A fingerprint for staleness detection.
/// Typically a SHA-256 hash of source metadata (file mtimes, sizes, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint(pub Vec<u8>);

/// Comparison operator for a column-level predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredicateOp {
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A column-level predicate that can be pushed down to an EDB source.
///
/// Predicates are **best-effort**: an EDB source may return more rows than
/// strictly match (false positives are allowed), but should try to filter
/// early using whatever native pushdown it supports (partition pruning,
/// data skipping, Parquet row-group pruning, etc.).
///
/// The Mangle runtime will always re-check predicates in-memory, so
/// correctness is preserved even if a source's pushdown is approximate.
#[derive(Debug, Clone)]
pub struct ColumnPredicate {
    /// Zero-based column index within the relation.
    pub col_idx: usize,
    /// The comparison operator.
    pub op: PredicateOp,
    /// The constant value to compare against.
    pub value: Value,
}

impl ColumnPredicate {
    /// Create a new column predicate.
    pub fn new(col_idx: usize, op: PredicateOp, value: Value) -> Self {
        Self { col_idx, op, value }
    }

    /// Evaluate this predicate against a row.
    pub fn eval(&self, row: &[Value]) -> bool {
        let col_val = match row.get(self.col_idx) {
            Some(v) => v,
            None => return false,
        };
        match self.op {
            PredicateOp::Eq => col_val == &self.value,
            PredicateOp::Neq => col_val != &self.value,
            PredicateOp::Lt => col_val < &self.value,
            PredicateOp::Le => col_val <= &self.value,
            PredicateOp::Gt => col_val > &self.value,
            PredicateOp::Ge => col_val >= &self.value,
        }
    }
}

/// Readonly provider of extensional (base) facts.
///
/// Implementations load facts from external sources (files, databases, etc.)
/// into the working store during `Database::open()`.
pub trait EdbSource: Send + Sync {
    /// A human-readable name for this source.
    fn name(&self) -> &str;

    /// Returns metadata about the relations this source provides.
    fn relations(&self) -> Result<Vec<RelationInfo>>;

    /// Returns all tuples for the given relation.
    fn scan(&self, relation: &str) -> Result<Vec<Vec<Value>>>;

    /// Returns tuples for the given relation, with best-effort predicate pushdown.
    ///
    /// The `predicates` are extracted from the compiled Mangle program by analyzing
    /// the physical plan. They represent constraints that are **always** applied to
    /// every row scanned from this relation, so filtering at the source is safe.
    ///
    /// Implementations should exploit whatever native pushdown their backend
    /// supports (partition pruning, data skipping, Parquet predicate pushdown, etc.)
    /// but are not required to guarantee exact filtering — the Mangle runtime
    /// will re-check predicates in-memory. False positives are acceptable;
    /// false negatives (dropping rows that should match) are not.
    ///
    /// The default implementation falls back to a full scan followed by
    /// in-memory filtering, preserving correctness for all sources.
    fn scan_with_predicates(
        &self,
        relation: &str,
        predicates: &[ColumnPredicate],
    ) -> Result<Vec<Vec<Value>>> {
        let rows = self.scan(relation)?;
        if predicates.is_empty() {
            return Ok(rows);
        }
        Ok(rows
            .into_iter()
            .filter(|row| predicates.iter().all(|p| p.eval(row)))
            .collect())
    }

    /// Returns a fingerprint for staleness detection.
    /// `None` means "always recompute" (no caching possible).
    fn fingerprint(&self) -> Result<Option<Fingerprint>>;
}
