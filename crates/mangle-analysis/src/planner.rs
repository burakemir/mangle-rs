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

use anyhow::{Result, anyhow};
use fxhash::FxHashSet;
use mangle_ir::physical::{self, Aggregate, CmpOp, Condition, DataSource, Expr, Op, Operand};
use mangle_ir::{Inst, InstId, Ir, NameId};

pub struct Planner<'a> {
    ir: &'a mut Ir,
    delta_pred: Option<NameId>,
    fresh_counter: usize,
    /// When true, emit `Op::HashJoin` for eligible two-premise joins. Off by
    /// default. Defaults to the value of `MANGLE_HASHJOIN=1` at construction
    /// time; tests and callers can override with `.with_hash_join(...)`.
    hash_join: bool,
}

/// Read the `MANGLE_HASHJOIN` env var once. Used to seed `Planner::hash_join`
/// so the flag can be set from the shell without code changes. Overridable
/// per-planner via `with_hash_join`.
fn hash_join_env_default() -> bool {
    std::env::var("MANGLE_HASHJOIN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Replace the placeholder `Op::Nop` body inside a freshly-built `Op::HashJoin`
/// with the real body planned after the join.
fn splice_hash_join_body(op: Op, body: Op) -> Op {
    match op {
        Op::HashJoin {
            build_source,
            probe_source,
            join_keys,
            ..
        } => Op::HashJoin {
            build_source,
            probe_source,
            join_keys,
            body: Box::new(body),
        },
        other => other,
    }
}

impl<'a> Planner<'a> {
    pub fn new(ir: &'a mut Ir) -> Self {
        Self {
            ir,
            delta_pred: None,
            fresh_counter: 0,
            hash_join: hash_join_env_default(),
        }
    }

    pub fn with_delta(mut self, delta_pred: NameId) -> Self {
        self.delta_pred = Some(delta_pred);
        self
    }

    /// Override the HashJoin emission flag (seeded from `MANGLE_HASHJOIN` env
    /// var by default). Primarily for testing and for callers that want to
    /// opt in or out explicitly.
    pub fn with_hash_join(mut self, enabled: bool) -> Self {
        self.hash_join = enabled;
        self
    }

    pub fn plan_rule(mut self, rule_id: InstId) -> Result<Op> {
        let (head, premises, transform) = match self.ir.get(rule_id) {
            Inst::Rule {
                head,
                premises,
                transform,
            } => (*head, premises.clone(), transform.clone()),
            _ => return Err(anyhow!("Not a rule")),
        };

        // Split transforms into blocks by 'do' statements
        let blocks = self.split_transforms(transform);
        let num_blocks = blocks.len();

        let mut ops = Vec::new();
        let mut current_source: Option<(NameId, Vec<NameId>)> = None;
        let mut bound_vars = FxHashSet::default();

        for (i, block) in blocks.into_iter().enumerate() {
            let is_last = i == num_blocks - 1;

            if i == 0 {
                // Block 0: Premises + Lets
                if is_last {
                    // Only one block, no aggregations
                    let op = self.plan_join_sequence(
                        premises.clone(),
                        &mut bound_vars,
                        |planner, vars| {
                            planner.plan_transforms_sequence(&block, vars, |p, v| {
                                p.plan_head_insert(head, v)
                            })
                        },
                    )?;
                    ops.push(op);
                } else {
                    // Materialize to temp
                    let temp_rel = self.fresh_var("temp_grp");
                    let mut capture_vars: Vec<NameId> = Vec::new(); // Will be populated by continuation

                    let op = self.plan_join_sequence(
                        premises.clone(),
                        &mut bound_vars,
                        |planner, vars| {
                            planner.plan_transforms_sequence(&block, vars, |_, v| {
                                let mut sorted_vars: Vec<NameId> = v.iter().cloned().collect();
                                sorted_vars.sort();
                                capture_vars = sorted_vars.clone();
                                let args =
                                    sorted_vars.iter().map(|&var| Operand::Var(var)).collect();
                                Ok(Op::Insert {
                                    relation: temp_rel,
                                    args,
                                })
                            })
                        },
                    )?;
                    ops.push(op);
                    current_source = Some((temp_rel, capture_vars));
                }
            } else {
                // Block i > 0: Starts with 'do'
                let (src_rel, src_vars) = current_source.take().expect("No source for aggregation");

                if is_last {
                    let op = self.plan_block_k(src_rel, src_vars, &block, |p, v| {
                        p.plan_head_insert(head, v)
                    })?;
                    ops.push(op);
                } else {
                    let next_temp = self.fresh_var("temp_grp");
                    let mut next_vars: Vec<NameId> = Vec::new();

                    let op = self.plan_block_k(src_rel, src_vars, &block, |_, v| {
                        let mut sorted_vars: Vec<NameId> = v.iter().cloned().collect();
                        sorted_vars.sort();
                        next_vars = sorted_vars.clone();
                        let args = sorted_vars.iter().map(|&var| Operand::Var(var)).collect();
                        Ok(Op::Insert {
                            relation: next_temp,
                            args,
                        })
                    })?;
                    ops.push(op);
                    current_source = Some((next_temp, next_vars));
                }
            }
        }

        if ops.len() == 1 {
            Ok(ops.remove(0))
        } else {
            Ok(Op::Seq(ops))
        }
    }

    fn split_transforms(&self, transforms: Vec<InstId>) -> Vec<Vec<InstId>> {
        let mut blocks = Vec::new();
        let mut current = Vec::new();
        for t in transforms {
            let inst = self.ir.get(t);
            if let Inst::Transform { var: None, .. } = inst {
                blocks.push(current);
                current = Vec::new();
            }
            current.push(t);
        }
        blocks.push(current);
        blocks
    }

    fn plan_block_k<F>(
        &mut self,
        source_rel: NameId,
        source_vars: Vec<NameId>,
        block: &[InstId],
        continuation: F,
    ) -> Result<Op>
    where
        F: FnOnce(&mut Self, &mut FxHashSet<NameId>) -> Result<Op>,
    {
        let do_stmt = block[0];
        let rest = &block[1..];

        let keys_insts = self.get_transform_app_args(do_stmt)?;
        let mut keys = Vec::new();
        for k in keys_insts {
            if let Inst::Var(v) = self.ir.get(k) {
                keys.push(*v);
            } else {
                return Err(anyhow!("GroupBy keys must be variables"));
            }
        }

        let mut aggregates = Vec::new();
        let mut lets = Vec::new();
        for &t in rest {
            if let Some(agg) = self.try_parse_aggregate(t)? {
                aggregates.push(agg);
            } else {
                lets.push(t);
            }
        }

        let mut inner_vars = FxHashSet::default();
        for &k in &keys {
            inner_vars.insert(k);
        }
        for agg in &aggregates {
            inner_vars.insert(agg.var);
        }

        let body = self.plan_transforms_sequence(&lets, &mut inner_vars, continuation)?;

        Ok(Op::GroupBy {
            source: source_rel,
            vars: source_vars,
            keys,
            aggregates,
            body: Box::new(body),
        })
    }

    fn plan_transforms_sequence<F>(
        &mut self,
        transforms: &[InstId],
        bound_vars: &mut FxHashSet<NameId>,
        continuation: F,
    ) -> Result<Op>
    where
        F: FnOnce(&mut Self, &mut FxHashSet<NameId>) -> Result<Op>,
    {
        if transforms.is_empty() {
            return continuation(self, bound_vars);
        }

        let t_id = transforms[0];
        let rest = &transforms[1..];

        let inst = self.ir.get(t_id).clone();
        if let Inst::Transform {
            var: Some(var),
            app,
        } = inst
        {
            self.inst_to_expr(app, |planner, expr| {
                bound_vars.insert(var);
                let body = planner.plan_transforms_sequence(rest, bound_vars, continuation)?;
                Ok(Op::Let {
                    var,
                    expr,
                    body: Box::new(body),
                })
            })
        } else {
            // Should not happen if split_transforms is correct
            Err(anyhow!("Unexpected transform in sequence"))
        }
    }

    fn fresh_var(&mut self, prefix: &str) -> NameId {
        let id = self.fresh_counter;
        self.fresh_counter += 1;
        let name = format!("${}_{}", prefix, id);
        self.ir.intern_name(name)
    }

    /// If this premise and the next one are both Atoms of the shape
    /// `p(V1, V2, ...)` — every arg a fresh, unbound, non-repeating variable
    /// — and they share at least one variable, consume the next premise and
    /// return a `HashJoin` op whose `body` is `Op::Nop` (spliced in by the
    /// caller). Returns `None` if the pattern does not match.
    fn try_plan_hash_join(
        &self,
        predicate: NameId,
        args: &[InstId],
        premises: &mut Vec<InstId>,
        bound_vars: &mut FxHashSet<NameId>,
    ) -> Result<Option<Op>> {
        let Some(build_vars) = self.all_fresh_distinct_vars(args, bound_vars) else {
            return Ok(None);
        };
        let next_id = *premises.first().unwrap();
        let next_inst = self.ir.get(next_id).clone();
        let Inst::Atom {
            predicate: next_pred,
            args: next_args,
        } = next_inst
        else {
            return Ok(None);
        };
        if Some(next_pred) == self.delta_pred {
            return Ok(None);
        }
        let Some(probe_vars) = self.all_fresh_distinct_vars(&next_args, bound_vars) else {
            return Ok(None);
        };
        let join_keys: Vec<NameId> = build_vars
            .iter()
            .filter(|v| probe_vars.contains(v))
            .copied()
            .collect();
        if join_keys.is_empty() {
            return Ok(None);
        }

        // Commit: consume the next premise and mark both sides' vars as bound.
        premises.remove(0);
        for v in build_vars.iter().chain(probe_vars.iter()) {
            bound_vars.insert(*v);
        }

        Ok(Some(Op::HashJoin {
            build_source: DataSource::Scan {
                relation: predicate,
                vars: build_vars,
            },
            probe_source: DataSource::Scan {
                relation: next_pred,
                vars: probe_vars,
            },
            join_keys,
            body: Box::new(Op::Nop),
        }))
    }

    /// Return the list of variables an atom's args bind, or `None` if any
    /// arg is already bound, not a `Var`, or repeats. Used to gate the
    /// HashJoin fast path — the existing nested-Iterate path handles the
    /// general case.
    fn all_fresh_distinct_vars(
        &self,
        args: &[InstId],
        bound_vars: &FxHashSet<NameId>,
    ) -> Option<Vec<NameId>> {
        let mut out: Vec<NameId> = Vec::with_capacity(args.len());
        for arg in args {
            let Inst::Var(v) = self.ir.get(*arg) else {
                return None;
            };
            if bound_vars.contains(v) || out.contains(v) {
                return None;
            }
            out.push(*v);
        }
        Some(out)
    }

    fn plan_join_sequence<F>(
        &mut self,
        mut premises: Vec<InstId>,
        bound_vars: &mut FxHashSet<NameId>,
        continuation: F,
    ) -> Result<Op>
    where
        F: FnOnce(&mut Self, &mut FxHashSet<NameId>) -> Result<Op>,
    {
        if premises.is_empty() {
            return continuation(self, bound_vars);
        }

        let current_premise = premises.remove(0);
        let inst = self.ir.get(current_premise).clone();

        match inst {
            Inst::Atom { predicate, args }
                if matches!(
                    self.ir.resolve_name(predicate),
                    ":lt" | ":le" | ":gt" | ":ge"
                        | ":time:lt" | ":time:le" | ":time:gt" | ":time:ge"
                        | ":duration:lt" | ":duration:le" | ":duration:gt" | ":duration:ge"
                ) =>
            {
                let cmp_op = match self.ir.resolve_name(predicate) {
                    ":lt" | ":time:lt" | ":duration:lt" => CmpOp::Lt,
                    ":le" | ":time:le" | ":duration:le" => CmpOp::Le,
                    ":gt" | ":time:gt" | ":duration:gt" => CmpOp::Gt,
                    ":ge" | ":time:ge" | ":duration:ge" => CmpOp::Ge,
                    _ => unreachable!(),
                };
                if args.len() != 2 {
                    return Err(anyhow!("Comparison predicate requires exactly 2 arguments"));
                }
                let body = self.plan_join_sequence(premises, bound_vars, continuation)?;
                self.with_eval(args[0], |this, left_op| {
                    this.with_eval(args[1], |_this, right_op| {
                        Ok(Op::Filter {
                            cond: Condition::Cmp {
                                op: cmp_op,
                                left: left_op.clone(),
                                right: right_op,
                            },
                            body: Box::new(body),
                        })
                    })
                })
            }
            Inst::Atom { predicate, args }
                if matches!(
                    self.ir.resolve_name(predicate),
                    ":string:starts_with"
                        | ":string:ends_with"
                        | ":string:contains"
                        | ":match_prefix"
                ) =>
            {
                if args.len() != 2 {
                    return Err(anyhow!(
                        "Built-in predicate requires exactly 2 arguments"
                    ));
                }
                let body = self.plan_join_sequence(premises, bound_vars, continuation)?;
                self.with_eval(args[0], |this, left_op| {
                    this.with_eval(args[1], |_this, right_op| {
                        Ok(Op::Filter {
                            cond: Condition::Call {
                                function: predicate,
                                args: vec![left_op.clone(), right_op],
                            },
                            body: Box::new(body),
                        })
                    })
                })
            }
            Inst::Atom { predicate, args } => {
                // Fast path: emit HashJoin when the env var
                // `MANGLE_HASHJOIN=1` is set, neither side is the delta
                // predicate, and both this premise and the next one are
                // simple Atoms whose args are all fresh (unbound, unique,
                // non-constant) variables sharing at least one join key.
                //
                // The planner is otherwise conservative: falling through to
                // the existing nested-Iterate + IndexLookup path is always
                // safe, so this is purely an opt-in performance path for
                // the use cases where hash join beats index-nested-loop.
                if self.hash_join
                    && self.delta_pred.is_none()
                    && !premises.is_empty()
                {
                    if let Some(op) = self.try_plan_hash_join(
                        predicate,
                        &args,
                        &mut premises,
                        bound_vars,
                    )? {
                        let body_op = self.plan_join_sequence(premises, bound_vars, continuation)?;
                        return Ok(splice_hash_join_body(op, body_op));
                    }
                }

                let mut scan_vars = Vec::new();
                let mut new_bindings = Vec::new();

                // Look for a potential index lookup
                let mut index_lookup: Option<(usize, Operand)> = None;

                for (i, arg) in args.iter().enumerate() {
                    let arg_inst = self.ir.get(*arg).clone();
                    match arg_inst {
                        Inst::Var(v) if bound_vars.contains(&v) => {
                            if index_lookup.is_none() {
                                index_lookup = Some((i, Operand::Var(v)));
                            }
                        }
                        Inst::Number(n) => {
                            if index_lookup.is_none() {
                                index_lookup =
                                    Some((i, Operand::Const(physical::Constant::Number(n))));
                            }
                        }
                        Inst::String(s) => {
                            if index_lookup.is_none() {
                                index_lookup =
                                    Some((i, Operand::Const(physical::Constant::String(s))));
                            }
                        }
                        Inst::Name(n) => {
                            if index_lookup.is_none() {
                                index_lookup =
                                    Some((i, Operand::Const(physical::Constant::Name(n))));
                            }
                        }
                        Inst::Float(fl) => {
                            if index_lookup.is_none() {
                                index_lookup =
                                    Some((i, Operand::Const(physical::Constant::Float(fl))));
                            }
                        }
                        Inst::Time(t) => {
                            if index_lookup.is_none() {
                                index_lookup =
                                    Some((i, Operand::Const(physical::Constant::Time(t))));
                            }
                        }
                        Inst::Duration(d) => {
                            if index_lookup.is_none() {
                                index_lookup =
                                    Some((i, Operand::Const(physical::Constant::Duration(d))));
                            }
                        }
                        _ => {}
                    }
                }

                for arg in &args {
                    if let Inst::Var(v) = self.ir.get(*arg)
                        && !bound_vars.contains(v)
                    {
                        scan_vars.push(*v);
                        new_bindings.push(*v);
                        continue;
                    }
                    let tmp = self.fresh_var("scan");
                    scan_vars.push(tmp);
                    new_bindings.push(tmp);
                }

                for v in &new_bindings {
                    bound_vars.insert(*v);
                }

                let body = self.plan_join_sequence(premises, bound_vars, continuation)?;
                let wrapped_body = self.apply_constraints(&args, &scan_vars, body)?;

                let source = if let Some((col_idx, key)) = index_lookup {
                    DataSource::IndexLookup {
                        relation: predicate,
                        col_idx,
                        key,
                        vars: scan_vars,
                    }
                } else if Some(predicate) == self.delta_pred {
                    DataSource::ScanDelta {
                        relation: predicate,
                        vars: scan_vars,
                    }
                } else {
                    DataSource::Scan {
                        relation: predicate,
                        vars: scan_vars,
                    }
                };
                Ok(Op::Iterate {
                    source,
                    body: Box::new(wrapped_body),
                })
            }
            Inst::Eq(l, r) => {
                let body = self.plan_join_sequence(premises, bound_vars, continuation)?;
                self.wrap_eq_check(l, r, body)
            }
            Inst::Ineq(l, r) => {
                let body = self.plan_join_sequence(premises, bound_vars, continuation)?;
                self.with_eval(l, |this, left_op| {
                    this.with_eval(r, |_this, right_op| {
                        Ok(Op::Filter {
                            cond: Condition::Cmp {
                                op: CmpOp::Neq,
                                left: left_op.clone(),
                                right: right_op,
                            },
                            body: Box::new(body),
                        })
                    })
                })
            }
            Inst::NegAtom(inner) => {
                let inner_inst = self.ir.get(inner).clone();
                if let Inst::Atom { predicate, args } = inner_inst {
                    let body = self.plan_join_sequence(premises, bound_vars, continuation)?;
                    let mut neg_args = Vec::new();
                    for arg in &args {
                        let arg_inst = self.ir.get(*arg).clone();
                        match arg_inst {
                            Inst::Var(v) => neg_args.push(Operand::Var(v)),
                            Inst::Number(n) => {
                                neg_args.push(Operand::Const(physical::Constant::Number(n)))
                            }
                            Inst::String(s) => {
                                neg_args.push(Operand::Const(physical::Constant::String(s)))
                            }
                            Inst::Name(n) => {
                                neg_args.push(Operand::Const(physical::Constant::Name(n)))
                            }
                            Inst::Float(fl) => {
                                neg_args.push(Operand::Const(physical::Constant::Float(fl)))
                            }
                            Inst::Time(t) => {
                                neg_args.push(Operand::Const(physical::Constant::Time(t)))
                            }
                            Inst::Duration(d) => {
                                neg_args.push(Operand::Const(physical::Constant::Duration(d)))
                            }
                            _ => return Err(anyhow!("Complex expression in negated atom")),
                        }
                    }
                    Ok(Op::Filter {
                        cond: Condition::Negation {
                            relation: predicate,
                            args: neg_args,
                        },
                        body: Box::new(body),
                    })
                } else {
                    Err(anyhow!("NegAtom wraps non-atom"))
                }
            }
            _ => Err(anyhow!("Unsupported premise type: {:?}", inst)),
        }
    }

    fn apply_constraints(
        &mut self,
        args: &[InstId],
        scan_vars: &[NameId],
        mut body: Op,
    ) -> Result<Op> {
        for (i, arg) in args.iter().enumerate().rev() {
            let scan_var = scan_vars[i];
            let arg_inst = self.ir.get(*arg).clone();
            match arg_inst {
                Inst::Var(v) => {
                    if v == scan_var {
                        continue;
                    }
                    body = Op::Filter {
                        cond: Condition::Cmp {
                            op: CmpOp::Eq,
                            left: Operand::Var(scan_var),
                            right: Operand::Var(v),
                        },
                        body: Box::new(body),
                    };
                }
                _ => {
                    body = self.wrap_eval_check(*arg, Operand::Var(scan_var), body)?;
                }
            }
        }
        Ok(body)
    }

    fn wrap_eq_check(&mut self, l: InstId, r: InstId, body: Op) -> Result<Op> {
        self.with_eval(l, |this, op_l| {
            this.with_eval(r, |_this, op_r| {
                Ok(Op::Filter {
                    cond: Condition::Cmp {
                        op: CmpOp::Eq,
                        left: op_l,
                        right: op_r,
                    },
                    body: Box::new(body),
                })
            })
        })
    }

    fn wrap_eval_check(&mut self, inst: InstId, target: Operand, body: Op) -> Result<Op> {
        self.with_eval(inst, |_this, op| {
            Ok(Op::Filter {
                cond: Condition::Cmp {
                    op: CmpOp::Eq,
                    left: target,
                    right: op,
                },
                body: Box::new(body),
            })
        })
    }

    fn with_eval<F>(&mut self, inst: InstId, f: F) -> Result<Op>
    where
        F: FnOnce(&mut Self, Operand) -> Result<Op>,
    {
        let i = self.ir.get(inst).clone();
        match i {
            Inst::Var(v) => f(self, Operand::Var(v)),
            Inst::String(s) => f(self, Operand::Const(physical::Constant::String(s))),
            Inst::Number(n) => f(self, Operand::Const(physical::Constant::Number(n))),
            Inst::Name(n) => f(self, Operand::Const(physical::Constant::Name(n))),
            Inst::Float(fl) => f(self, Operand::Const(physical::Constant::Float(fl))),
            Inst::Time(t) => f(self, Operand::Const(physical::Constant::Time(t))),
            Inst::Duration(d) => f(self, Operand::Const(physical::Constant::Duration(d))),
            Inst::ApplyFn { function, args } => self.with_eval_args(
                &args,
                0,
                Vec::new(),
                Box::new(|this, ops| {
                    let tmp = this.fresh_var("call");
                    let inner = f(this, Operand::Var(tmp))?;
                    Ok(Op::Let {
                        var: tmp,
                        expr: Expr::Call {
                            function,
                            args: ops,
                        },
                        body: Box::new(inner),
                    })
                }),
            ),
            // Compound types: route through function calls
            Inst::List(args) => {
                let fn_name = self.ir.intern_name("fn:list".to_string());
                self.with_eval_args(
                    &args,
                    0,
                    Vec::new(),
                    Box::new(|this, ops| {
                        let tmp = this.fresh_var("list");
                        let inner = f(this, Operand::Var(tmp))?;
                        Ok(Op::Let {
                            var: tmp,
                            expr: Expr::Call {
                                function: fn_name,
                                args: ops,
                            },
                            body: Box::new(inner),
                        })
                    }),
                )
            }
            Inst::Map { keys, values } => {
                // Interleave keys and values: [k1, v1, k2, v2, ...]
                let mut interleaved = Vec::with_capacity(keys.len() + values.len());
                for (k, v) in keys.iter().zip(values.iter()) {
                    interleaved.push(*k);
                    interleaved.push(*v);
                }
                let fn_name = self.ir.intern_name("fn:map".to_string());
                self.with_eval_args(
                    &interleaved,
                    0,
                    Vec::new(),
                    Box::new(|this, ops| {
                        let tmp = this.fresh_var("map");
                        let inner = f(this, Operand::Var(tmp))?;
                        Ok(Op::Let {
                            var: tmp,
                            expr: Expr::Call {
                                function: fn_name,
                                args: ops,
                            },
                            body: Box::new(inner),
                        })
                    }),
                )
            }
            Inst::Struct { fields, values } => {
                // Interleave field names and values: [name1, val1, name2, val2, ...]
                let mut interleaved = Vec::with_capacity(fields.len() + values.len());
                for (field, val) in fields.iter().zip(values.iter()) {
                    // Field names are NameIds, emit as Name constants
                    let name_inst = self.ir.add_inst(Inst::Name(*field));
                    interleaved.push(name_inst);
                    interleaved.push(*val);
                }
                let fn_name = self.ir.intern_name("fn:struct".to_string());
                self.with_eval_args(
                    &interleaved,
                    0,
                    Vec::new(),
                    Box::new(|this, ops| {
                        let tmp = this.fresh_var("struct");
                        let inner = f(this, Operand::Var(tmp))?;
                        Ok(Op::Let {
                            var: tmp,
                            expr: Expr::Call {
                                function: fn_name,
                                args: ops,
                            },
                            body: Box::new(inner),
                        })
                    }),
                )
            }
            _ => Err(anyhow!("Unsupported expression in evaluation")),
        }
    }

    fn inst_to_expr<F>(&mut self, inst: InstId, f: F) -> Result<Op>
    where
        F: FnOnce(&mut Self, Expr) -> Result<Op>,
    {
        let i = self.ir.get(inst).clone();
        match i {
            Inst::ApplyFn { function, args } => self.with_eval_args(
                &args,
                0,
                Vec::new(),
                Box::new(|this, ops| {
                    f(
                        this,
                        Expr::Call {
                            function,
                            args: ops,
                        },
                    )
                }),
            ),
            _ => self.with_eval(inst, |this, op| f(this, Expr::Value(op))),
        }
    }

    fn with_eval_args(
        &mut self,
        args: &[InstId],
        index: usize,
        mut acc: Vec<Operand>,
        f: Box<dyn FnOnce(&mut Self, Vec<Operand>) -> Result<Op> + '_>,
    ) -> Result<Op> {
        if index >= args.len() {
            return f(self, acc);
        }
        self.with_eval(args[index], |this, op| {
            acc.push(op);
            this.with_eval_args(args, index + 1, acc, f)
        })
    }

    fn plan_head_insert(
        &mut self,
        head: InstId,
        _bound_vars: &mut FxHashSet<NameId>,
    ) -> Result<Op> {
        let inst = self.ir.get(head).clone();
        if let Inst::Atom { predicate, args } = inst {
            self.with_eval_args(
                &args,
                0,
                Vec::new(),
                Box::new(|_this, ops| {
                    Ok(Op::Insert {
                        relation: predicate,
                        args: ops,
                    })
                }),
            )
        } else {
            Err(anyhow!("Head must be an atom"))
        }
    }

    fn get_transform_app_args(&self, t_id: InstId) -> Result<Vec<InstId>> {
        if let Inst::Transform { app, .. } = self.ir.get(t_id)
            && let Inst::ApplyFn { args, .. } = self.ir.get(*app)
        {
            return Ok(args.clone());
        }
        Err(anyhow!("Invalid transform structure"))
    }

    fn try_parse_aggregate(&mut self, t_id: InstId) -> Result<Option<Aggregate>> {
        let inst = self.ir.get(t_id).clone();
        if let Inst::Transform {
            var: Some(var),
            app,
        } = inst
            && let Inst::ApplyFn { function, args } = self.ir.get(app).clone()
        {
            let func_name = self.ir.resolve_name(function);
            if matches!(
                func_name,
                "fn:sum"
                    | "fn:count"
                    | "fn:max"
                    | "fn:min"
                    | "fn:collect"
                    | "fn:float:sum"
                    | "fn:float:max"
                    | "fn:float:min"
            ) {
                let mut op_args = Vec::new();
                for arg in args {
                    let arg_inst = self.ir.get(arg).clone();
                    match arg_inst {
                        Inst::Var(v) => op_args.push(Operand::Var(v)),
                        Inst::Number(n) => {
                            op_args.push(Operand::Const(physical::Constant::Number(n)))
                        }
                        Inst::Float(fl) => {
                            op_args.push(Operand::Const(physical::Constant::Float(fl)))
                        }
                        _ => {
                            return Err(anyhow!(
                                "Complex expressions in aggregates not supported yet"
                            ));
                        }
                    }
                }
                return Ok(Some(Aggregate {
                    var,
                    func: function,
                    args: op_args,
                }));
            }
        }
        Ok(None)
    }
}
