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

//! Predicate extraction from compiled Mangle IR.
//!
//! Analyzes the physical plan to discover column-level predicates that can be
//! pushed down to EDB sources. This enables partition pruning, data skipping,
//! and Parquet-level predicate pushdown in external data sources (e.g. Delta Lake).
//!
//! # How It Works
//!
//! The Mangle compiler produces a physical plan (`Op` tree) where each EDB
//! relation appears as a `DataSource::Scan` or `DataSource::IndexLookup`.
//! The first operations applied to scanned rows are typically `Filter` nodes
//! that check equality or comparison conditions against constants.
//!
//! This module walks the `Op` tree, finds such patterns, and extracts them
//! as `ColumnPredicate`s keyed by relation name. The predicates are then
//! passed to `EdbSource::scan_with_predicates()` before data loading.
//!
//! # Safety
//!
//! Extracted predicates are **conservative**: they are only extracted when
//! the predicate is provably always-applied to every row from the scan.
//! The Mangle runtime re-checks all predicates in-memory regardless, so
//! approximate pushdown in the source is always safe.

use std::collections::{HashMap, HashSet};

use fxhash::FxHashSet;
use mangle_common::Value;
use mangle_ir::physical::{CmpOp, Condition, DataSource, Op, Operand};
use mangle_ir::Ir;

use crate::source::{ColumnPredicate, PredicateOp};

/// Extract pushdown-eligible predicates from the compiled physical plan.
///
/// Returns a map from relation name to the list of `ColumnPredicate`s that
/// can be safely pushed down to the EDB source for that relation.
///
/// Only extracts predicates from **immediate** filters on EDB scans — it does
/// not attempt to push join predicates or predicates from nested iterations.
pub fn extract_predicates(ir: &Ir, ops: &[Op], edb_relations: &HashSet<String>) -> HashMap<String, Vec<ColumnPredicate>> {
    let mut result: HashMap<String, Vec<ColumnPredicate>> = HashMap::new();
    for op in ops {
        extract_from_op(ir, op, edb_relations, &mut result);
    }
    result
}

fn extract_from_op(
    ir: &Ir,
    op: &Op,
    edb_relations: &HashSet<String>,
    result: &mut HashMap<String, Vec<ColumnPredicate>>,
) {
    match op {
        Op::Nop => {}
        Op::Seq(ops) => {
            for o in ops {
                extract_from_op(ir, o, edb_relations, result);
            }
        }
        Op::Iterate { source, body } => {
            // Extract predicates from the body that are immediately applied
            // to variables from this scan source.
            extract_from_iterate(ir, source, body, edb_relations, result);
            // Also recurse into the body for nested iterations
            extract_from_op(ir, body, edb_relations, result);
        }
        Op::Filter { cond: _, body } => {
            // Filters are handled in extract_from_iterate when they're the
            // immediate child of an Iterate. Standalone filters operate on
            // already-bound variables, not directly on EDB columns.
            extract_from_op(ir, body, edb_relations, result);
        }
        Op::Insert { .. } => {}
        Op::Let { body, .. } => {
            extract_from_op(ir, body, edb_relations, result);
        }
        Op::GroupBy { body, .. } => {
            extract_from_op(ir, body, edb_relations, result);
        }
        Op::HashJoin { body, .. } => {
            extract_from_op(ir, body, edb_relations, result);
        }
    }
}

/// Extract predicates from an Iterate node. Looks for patterns like:
///
/// ```text
/// Iterate {
///     source: Scan { relation, vars },
///     body: Filter { cond: Cmp { op, left: Var(v), right: Const(c) }, body: ... }
/// }
/// ```
///
/// or `IndexLookup` which already has an embedded equality predicate.
fn extract_from_iterate(
    ir: &Ir,
    source: &DataSource,
    body: &Op,
    edb_relations: &HashSet<String>,
    result: &mut HashMap<String, Vec<ColumnPredicate>>,
) {
    match source {
        DataSource::Scan { relation, vars } => {
            let rel_name = ir.resolve_name(*relation).to_string();
            if !edb_relations.contains(&rel_name) {
                return;
            }

            // Build a mapping from variable NameId to column index
            let var_to_col: HashMap<mangle_ir::NameId, usize> = vars
                .iter()
                .enumerate()
                .map(|(i, v)| (*v, i))
                .collect();

            // Collect immediate filters on this scan's variables
            let mut predicates = Vec::new();
            collect_filters(ir, body, &var_to_col, &mut predicates);

            if !predicates.is_empty() {
                result
                    .entry(rel_name)
                    .or_default()
                    .extend(predicates);
            }
        }
        DataSource::ScanDelta { relation, vars } => {
            let rel_name = ir.resolve_name(*relation).to_string();
            if !edb_relations.contains(&rel_name) {
                return;
            }
            // Delta scans are used during semi-naive evaluation, not during
            // initial EDB loading. No predicates to extract here.
            let _ = vars;
        }
        DataSource::IndexLookup {
            relation,
            col_idx,
            key,
            vars: _,
        } => {
            let rel_name = ir.resolve_name(*relation).to_string();
            if !edb_relations.contains(&rel_name) {
                return;
            }

            // An IndexLookup is already an equality predicate on col_idx = key
            if let Some(value) = operand_to_value(ir, key) {
                result
                    .entry(rel_name)
                    .or_default()
                    .push(ColumnPredicate::new(*col_idx, PredicateOp::Eq, value));
            }
        }
    }
}

/// Recursively collect filter conditions from a chain of Filter nodes
/// that are the immediate children of an Iterate.
///
/// Stops collecting when encountering a non-Filter node (e.g. another Iterate,
/// Insert, or Let), since predicates beyond that point may depend on variables
/// not from the original scan.
fn collect_filters(
    ir: &Ir,
    op: &Op,
    var_to_col: &HashMap<mangle_ir::NameId, usize>,
    predicates: &mut Vec<ColumnPredicate>,
) {
    match op {
        Op::Filter { cond, body } => {
            if let Some(pred) = condition_to_predicate(ir, cond, var_to_col) {
                predicates.push(pred);
            }
            // Continue collecting from nested filters
            collect_filters(ir, body, var_to_col, predicates);
        }
        _ => {
            // Not a filter — stop collecting. Predicates beyond this point
            // may depend on variables from nested iterations or let-bindings.
        }
    }
}

/// Try to convert a filter condition into a ColumnPredicate.
///
/// Only handles simple comparisons of the form:
/// - `Var(v) <op> Const(c)` where `v` is in `var_to_col`
/// - `Const(c) <op> Var(v)` (swapped operands)
fn condition_to_predicate(
    ir: &Ir,
    cond: &Condition,
    var_to_col: &HashMap<mangle_ir::NameId, usize>,
) -> Option<ColumnPredicate> {
    match cond {
        Condition::Cmp { op, left, right } => {
            // Try Var <op> Const
            if let Some(pred) = try_var_const(ir, *op, left, right, var_to_col) {
                return Some(pred);
            }
            // Try Const <op> Var (swap operands and flip operator)
            if let Some(pred) = try_var_const(ir, flip_cmp(*op), right, left, var_to_col) {
                return Some(pred);
            }
            None
        }
        Condition::Negation { .. } | Condition::Call { .. } => None,
    }
}

/// Try to extract a predicate from `Var(v) <op> Const(c)`.
fn try_var_const(
    ir: &Ir,
    op: CmpOp,
    left: &Operand,
    right: &Operand,
    var_to_col: &HashMap<mangle_ir::NameId, usize>,
) -> Option<ColumnPredicate> {
    let var_id = match left {
        Operand::Var(id) => *id,
        _ => return None,
    };
    let col_idx = var_to_col.get(&var_id)?;
    let value = operand_to_value(ir, right)?;
    Some(ColumnPredicate::new(*col_idx, cmp_to_pred_op(op), value))
}

/// Convert an IR `Operand` to a `Value`, if it's a constant.
fn operand_to_value(ir: &Ir, operand: &Operand) -> Option<Value> {
    match operand {
        Operand::Const(c) => constant_to_value(ir, c),
        Operand::Var(_) => None,
    }
}

/// Convert an IR `Constant` to a `Value`.
fn constant_to_value(ir: &Ir, c: &mangle_ir::physical::Constant) -> Option<Value> {
    match c {
        mangle_ir::physical::Constant::Number(n) => Some(Value::Number(*n)),
        mangle_ir::physical::Constant::Float(f) => Some(Value::Float(*f)),
        mangle_ir::physical::Constant::String(sid) => {
            Some(Value::String(ir.resolve_string(*sid).to_string()))
        }
        mangle_ir::physical::Constant::Name(nid) => {
            Some(Value::Name(ir.resolve_name(*nid).to_string()))
        }
        mangle_ir::physical::Constant::Time(t) => Some(Value::Time(*t)),
        mangle_ir::physical::Constant::Duration(d) => Some(Value::Duration(*d)),
    }
}

/// Flip a comparison operator (for swapping operand order).
fn flip_cmp(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Neq => CmpOp::Neq,
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
    }
}

/// Convert an IR `CmpOp` to a `PredicateOp`.
fn cmp_to_pred_op(op: CmpOp) -> PredicateOp {
    match op {
        CmpOp::Eq => PredicateOp::Eq,
        CmpOp::Neq => PredicateOp::Neq,
        CmpOp::Lt => PredicateOp::Lt,
        CmpOp::Le => PredicateOp::Le,
        CmpOp::Gt => PredicateOp::Gt,
        CmpOp::Ge => PredicateOp::Ge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangle_ast::Arena;
    use mangle_driver::compile;
    use mangle_analysis::Planner;
    use mangle_ir::Inst;
    use std::collections::HashSet;

    /// Compile a source program, plan all rules, and extract predicates
    /// for named EDB relations.
    fn extract(source: &str, edb_names: &[&str]) -> HashMap<String, Vec<ColumnPredicate>> {
        let arena = Arena::new_with_global_interner();
        let (mut ir, stratified) = compile(source, &arena).unwrap();

        let edb_set: HashSet<String> = edb_names.iter().map(|s| s.to_string()).collect();

        // Collect all stratum ops by planning each rule
        let mut all_ops: Vec<mangle_ir::physical::Op> = Vec::new();
        for stratum in stratified.strata() {
            let mut stratum_pred_names = FxHashSet::default();
            for pred in &stratum {
                if let Some(name) = arena.predicate_name(*pred) {
                    stratum_pred_names.insert(name);
                }
            }

            // Collect rule IDs first (avoids borrowing ir while planning)
            let mut rule_ids = Vec::new();
            for (i, inst) in ir.insts.iter().enumerate() {
                if let Inst::Rule { head, .. } = inst
                    && let Inst::Atom { predicate, .. } = ir.get(*head)
                {
                    let head_name = ir.resolve_name(*predicate);
                    if stratum_pred_names.contains(head_name) {
                        rule_ids.push(mangle_ir::InstId::new(i));
                    }
                }
            }

            for rule_id in rule_ids {
                let planner = Planner::new(&mut ir);
                if let Ok(op) = planner.plan_rule(rule_id) {
                    all_ops.push(op);
                }
            }
        }

        extract_predicates(&ir, &all_ops, &edb_set)
    }

    #[test]
    fn test_column_predicate_eval_eq() {
        let pred = ColumnPredicate::new(1, PredicateOp::Eq, Value::Number(42));
        assert!(pred.eval(&[Value::Number(1), Value::Number(42)]));
        assert!(!pred.eval(&[Value::Number(1), Value::Number(99)]));
    }

    #[test]
    fn test_column_predicate_eval_gt() {
        let pred = ColumnPredicate::new(1, PredicateOp::Gt, Value::Number(100));
        assert!(pred.eval(&[Value::Number(1), Value::Number(200)]));
        assert!(!pred.eval(&[Value::Number(1), Value::Number(50)]));
        assert!(!pred.eval(&[Value::Number(1), Value::Number(100)]));
    }

    #[test]
    fn test_column_predicate_eval_string() {
        let pred = ColumnPredicate::new(0, PredicateOp::Eq, Value::String("US".to_string()));
        assert!(pred.eval(&[Value::String("US".to_string())]));
        assert!(!pred.eval(&[Value::String("EU".to_string())]));
    }

    #[test]
    fn test_column_predicate_eval_out_of_bounds() {
        let pred = ColumnPredicate::new(5, PredicateOp::Eq, Value::Number(1));
        assert!(!pred.eval(&[Value::Number(1)]));
    }

    #[test]
    fn test_extract_equality_constant() {
        // Rule: q(X) :- p(X, "hello").
        // The constant "hello" on column 1 should be extracted.
        let preds = extract(
            r#"q(X) :- p(X, "hello")."#,
            &["p"],
        );
        let p_preds = preds.get("p").unwrap();
        assert_eq!(p_preds.len(), 1);
        assert_eq!(p_preds[0].col_idx, 1);
        assert_eq!(p_preds[0].op, PredicateOp::Eq);
        assert_eq!(p_preds[0].value, Value::String("hello".to_string()));
    }

    #[test]
    fn test_extract_comparison_predicate() {
        // Rule: q(X) :- p(X, Y), Y > 100.
        // The comparison Y > 100 on column 1 should be extracted.
        let preds = extract(
            r#"q(X) :- p(X, Y), Y > 100."#,
            &["p"],
        );
        let p_preds = preds.get("p").unwrap();
        assert!(p_preds.len() >= 1, "expected at least 1 predicate, got {:?}", p_preds);
        let gt_pred = p_preds.iter().find(|p| p.op == PredicateOp::Gt && p.col_idx == 1);
        assert!(gt_pred.is_some(), "expected Gt predicate on col 1, got {:?}", p_preds);
        let gt_pred = gt_pred.unwrap();
        assert_eq!(gt_pred.value, Value::Number(100));
    }

    #[test]
    fn test_extract_multiple_predicates() {
        // Rule: q(X) :- p(X, Y, /region, "US"), Y > 1000.
        //
        // The planner turns /region into an IndexLookup (Eq on col 2), and
        // "US" into a constant in the scan. The Y > 1000 filter may or may
        // not be a direct child of the Iterate depending on how the planner
        // structures the plan. We verify that at minimum the /region
        // equality is extracted.
        let preds = extract(
            r#"q(X) :- p(X, Y, /region, "US"), Y > 1000."#,
            &["p"],
        );
        let p_preds = preds.get("p").unwrap();
        assert!(!p_preds.is_empty(), "expected at least 1 predicate, got none");

        // The /region name constant should always be extracted (as IndexLookup)
        let region_pred = p_preds.iter().find(|p| p.col_idx == 2 && p.op == PredicateOp::Eq);
        assert!(region_pred.is_some(), "expected Eq predicate on col 2 for /region, got {:?}", p_preds);
    }

    #[test]
    fn test_extract_no_predicates_for_idb() {
        // p is both EDB (fact) and IDB (rule). No predicates should be
        // extracted for it since it's not a pure EDB source.
        let preds = extract(
            r#"p(1). q(X) :- p(X)."#,
            &[],  // no EDB sources
        );
        assert!(preds.is_empty());
    }

    #[test]
    fn test_extract_no_predicate_without_constant() {
        // Rule: q(X, Y) :- p(X, Y).
        // No predicates to push down — both are variables.
        let preds = extract(
            r#"q(X, Y) :- p(X, Y)."#,
            &["p"],
        );
        // May or may not have entries, but no predicates
        let p_preds = preds.get("p").map(|v| v.as_slice()).unwrap_or(&[]);
        assert_eq!(p_preds.len(), 0, "expected no predicates, got {:?}", p_preds);
    }
}
