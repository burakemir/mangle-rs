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

//! Bounds checker for Mangle.
//!
//! Validates that facts and rule derivations conform to declared type bounds.
//! Implements the Go-equivalent bounds analysis with:
//!
//! - Inference state tracking with per-variable type accumulation
//! - Feasible alternatives analysis with special cases for built-in predicates
//! - Skolemization of polymorphic type variables
//! - Cross-predicate type inference
//! - UpperBound/LowerBound for type intersection/union

use anyhow::{Result, anyhow};
use mangle_ir::{Inst, InstId, Ir, NameId};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::name_trie::NameTrie;
use crate::type_expr::{self, TypeContext};

/// Built-in predicates all of whose arguments are inputs (evaluated as
/// filters): every variable must be bound by an earlier premise.
const FILTER_PREDS: &[&str] = &[
    ":lt",
    ":le",
    ":gt",
    ":ge",
    ":time:lt",
    ":time:le",
    ":time:gt",
    ":time:ge",
    ":duration:lt",
    ":duration:le",
    ":duration:gt",
    ":duration:ge",
    ":string:starts_with",
    ":string:ends_with",
    ":string:contains",
    ":match_prefix",
];

/// Reducer (aggregation) functions, matching the planner's
/// `try_parse_aggregate` whitelist.
const REDUCER_FNS: &[&str] = &[
    "fn:sum",
    "fn:count",
    "fn:max",
    "fn:min",
    "fn:collect",
    "fn:collect_distinct",
    "fn:float:sum",
    "fn:float:max",
    "fn:float:min",
];

/// Arity requirement of a built-in function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FnArity {
    /// Exactly N arguments.
    Fixed(usize),
    /// Any number of arguments (folds, constructors).
    VarArgs,
    /// At least one argument (aggregations).
    AtLeastOne,
    /// An even number of arguments (key/value constructors).
    Even,
}

/// Arity table for every function the runtime (interpreter / WASM codegen)
/// implements, mirroring mangle-go's `builtin.Functions`. `None` means the
/// function does not exist at runtime.
fn builtin_arity(name: &str) -> Option<FnArity> {
    use FnArity::*;
    Some(match name {
        // Var-arity folds.
        "fn:plus" | "fn:minus" | "fn:mult" | "fn:div" | "fn:float:plus" | "fn:float:minus"
        | "fn:float:mult" | "fn:float:div" | "fn:string:concat" | "fn:list" | "fn:group_by" => {
            VarArgs
        }
        // Key/value constructors need an even number of arguments.
        "fn:map" | "fn:struct" => Even,
        // Aggregations need at least one argument.
        "fn:count"
        | "fn:sum"
        | "fn:max"
        | "fn:min"
        | "fn:collect"
        | "fn:collect_distinct"
        | "fn:float:sum"
        | "fn:float:max"
        | "fn:float:min" => AtLeastOne,
        // Fixed arity.
        "fn:sqrt" => Fixed(1),
        "fn:string:replace" => Fixed(4),
        "fn:number:to_string" | "fn:float64:to_string" | "fn:name:to_string" => Fixed(1),
        "fn:pair" => Fixed(2),
        "fn:list:get" => Fixed(2),
        "fn:list:append" => Fixed(2),
        "fn:len" | "fn:list:len" | "fn:map:len" | "fn:struct:len" => Fixed(1),
        "fn:pair:first" | "fn:pair:second" => Fixed(1),
        "fn:map:get" | "fn:struct:get" => Fixed(2),
        "fn:map:keys" | "fn:map:values" | "fn:struct:values" => Fixed(1),
        "fn:time:now" => Fixed(0),
        "fn:time:add"
        | "fn:time:sub"
        | "fn:time:trunc"
        | "fn:time:format"
        | "fn:time:parse_civil" => Fixed(2),
        "fn:time:format_civil" => Fixed(3),
        "fn:time:year"
        | "fn:time:month"
        | "fn:time:day"
        | "fn:time:hour"
        | "fn:time:minute"
        | "fn:time:second"
        | "fn:time:from_unix_nanos"
        | "fn:time:to_unix_nanos"
        | "fn:time:parse_rfc3339" => Fixed(1),
        "fn:duration:add" | "fn:duration:mult" => Fixed(2),
        "fn:duration:hours"
        | "fn:duration:minutes"
        | "fn:duration:seconds"
        | "fn:duration:nanos"
        | "fn:duration:from_nanos"
        | "fn:duration:from_hours"
        | "fn:duration:from_minutes"
        | "fn:duration:from_seconds"
        | "fn:duration:parse" => Fixed(1),
        _ => return None,
    })
}

/// Bounds checker state.
pub struct BoundsChecker<'a> {
    ir: &'a mut Ir,
    name_trie: NameTrie,
    /// Predicate NameId -> declared type alternatives.
    /// Each alternative is a Vec<InstId> of argument types.
    rel_type_map: FxHashMap<NameId, Vec<Vec<InstId>>>,
    /// Predicate NameId -> rules defining it: (head, premises, transforms).
    rules_map: FxHashMap<NameId, Vec<(InstId, Vec<InstId>, Vec<InstId>)>>,
    /// Cross-predicate inference: inferred types for predicates without declarations.
    inferred: FxHashMap<NameId, Vec<Vec<InstId>>>,
    /// Cycle detection for cross-predicate inference.
    visiting: FxHashSet<NameId>,
    /// Counter for generating fresh type variable names.
    fresh_var_counter: usize,
    /// Function argument-type errors (e.g. `fn:plus` applied to /string),
    /// collected during inference and reported after all clauses are checked.
    fn_arg_errors: Vec<String>,
}

impl<'a> BoundsChecker<'a> {
    pub fn new(ir: &'a mut Ir) -> Self {
        Self {
            ir,
            name_trie: NameTrie::new(),
            rel_type_map: FxHashMap::default(),
            rules_map: FxHashMap::default(),
            inferred: FxHashMap::default(),
            visiting: FxHashSet::default(),
            fresh_var_counter: 0,
            fn_arg_errors: Vec::new(),
        }
    }

    /// Main entry point: collect declarations, build rules map, check all clauses.
    pub fn check(&mut self) -> Result<()> {
        self.collect_declarations()?;
        self.build_rules_map();
        self.check_arity_consistency()?;
        self.check_bindings()?;
        self.check_function_arities()?;
        self.check_all_clauses()?;
        if let Some(e) = self.fn_arg_errors.first() {
            return Err(anyhow!("type error: {e}"));
        }
        Ok(())
    }

    /// Generates a fresh type variable NameId (e.g., `?X0`, `?X1`, ...).
    fn fresh_var(&mut self) -> NameId {
        let name = format!("?X{}", self.fresh_var_counter);
        self.fresh_var_counter += 1;
        self.ir.intern_name(&name)
    }

    /// Pass 1: Collect declared types from Decl instructions and build name trie.
    fn collect_declarations(&mut self) -> Result<()> {
        let insts: Vec<Inst> = self.ir.insts.clone();
        for inst in &insts {
            if let Inst::Decl { atom, bounds, .. } = inst {
                let pred_name = self.atom_predicate(*atom);
                if let Some(pred) = pred_name {
                    let mut alternatives = Vec::new();
                    for bound_id in bounds {
                        if let Inst::BoundDecl { base_terms } = self.ir.get(*bound_id) {
                            let base_terms = base_terms.clone();
                            // Collect name constants into trie.
                            for term in &base_terms {
                                self.name_trie.collect(self.ir, *term);
                            }
                            // Build type context with any type variables in this bound.
                            let any = type_expr::find_or_create_name(self.ir, "/any");
                            let mut ctx = TypeContext::default();
                            for term in &base_terms {
                                let mut vars = FxHashSet::default();
                                type_expr::collect_vars(self.ir, *term, &mut vars);
                                for v in vars {
                                    ctx.entry(v).or_insert(any);
                                }
                            }
                            // Validate wellformedness of each type expression.
                            for term in &base_terms {
                                type_expr::wellformed_type(self.ir, &ctx, *term)?;
                            }
                            alternatives.push(base_terms);
                        }
                    }
                    if !alternatives.is_empty() {
                        self.rel_type_map.insert(pred, alternatives);
                    }
                }
            }
        }
        Ok(())
    }

    /// Build a map from predicate NameId to rules (head, premises, transforms).
    fn build_rules_map(&mut self) {
        let insts: Vec<Inst> = self.ir.insts.clone();
        for inst in &insts {
            if let Inst::Rule {
                head,
                premises,
                transform,
            } = inst
            {
                // Only non-unit clauses (actual rules with premises or transforms).
                if (!premises.is_empty() || !transform.is_empty())
                    && let Some(pred) = self.atom_predicate(*head)
                {
                    self.rules_map.entry(pred).or_default().push((
                        *head,
                        premises.clone(),
                        transform.clone(),
                    ));
                }
            }
        }
    }

    /// Pass 1.5: Check that every predicate is used with a consistent arity.
    ///
    /// Scans all facts and rule heads to detect arity mismatches, e.g.:
    /// `p(1). p(2, 3).` — predicate `p` used with arity 1 and 2.
    fn check_arity_consistency(&self) -> Result<()> {
        // Map predicate NameId -> (first seen arity, first seen location)
        let mut arity_map: FxHashMap<NameId, (usize, String)> = FxHashMap::default();
        let mut errors: Vec<String> = Vec::new();

        let insts: Vec<Inst> = self.ir.insts.clone();
        for inst in &insts {
            if let Inst::Rule {
                head,
                premises,
                transform,
            } = inst
            {
                let is_fact = premises.is_empty() && transform.is_empty();
                if let Some(pred) = self.atom_predicate(*head) {
                    let expected_args = if self.ir.temporal_predicates.contains(&pred) {
                        let args = self.atom_args(*head);
                        if args.len() >= 2 {
                            args.len() - 2
                        } else {
                            args.len()
                        }
                    } else {
                        self.atom_args(*head).len()
                    };

                    let kind = if is_fact { "fact" } else { "rule" };
                    let pred_name = self.ir.resolve_name(pred).to_string();
                    let location = format!("{} {}({} arg(s))", kind, pred_name, expected_args);

                    if let Some(&(first_arity, ref first_location)) = arity_map.get(&pred) {
                        if first_arity != expected_args {
                            errors.push(format!(
                                "predicate '{}' used with inconsistent arity: {} vs {}",
                                pred_name, first_location, location
                            ));
                        }
                    } else {
                        arity_map.insert(pred, (expected_args, location));
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!("arity error: {}", errors.join("; ")))
        }
    }

    /// Pass 2: Check all unit clauses and rules against declared bounds.
    fn check_all_clauses(&mut self) -> Result<()> {
        let insts: Vec<Inst> = self.ir.insts.clone();
        for inst in &insts {
            if let Inst::Rule {
                head,
                premises,
                transform,
            } = inst
            {
                let head = *head;
                let premises = premises.clone();
                let transform = transform.clone();
                let is_fact = premises.is_empty() && transform.is_empty();
                let alternatives = self
                    .atom_predicate(head)
                    .and_then(|pred| self.rel_type_map.get(&pred).cloned());
                if is_fact {
                    if let Some(alternatives) = alternatives {
                        self.check_fact(head, &alternatives)?;
                    }
                } else if let Some(alternatives) = alternatives {
                    self.check_rule(head, &premises, &transform, &alternatives)?;
                } else {
                    // Undeclared head predicate: no declared bounds to check
                    // against, but still run the inference pipeline so that
                    // function argument-type errors (e.g. fn:plus applied to
                    // a /string) surface for every rule, not just declared
                    // ones.
                    let _ = self.infer_rule_types(head, &premises, &transform)?;
                }
            }
        }
        Ok(())
    }

    /// Pass 3.5: Check that every function application in a rule's premises
    /// and transforms has a valid arity and refers to a function the runtime
    /// actually implements (port of mangle-go's `checkFunctions`/
    /// `checkExprArity`). Facts have no premises or transforms and are
    /// skipped, matching mangle-go.
    fn check_function_arities(&self) -> Result<()> {
        for inst in &self.ir.insts {
            if let Inst::Rule {
                premises,
                transform,
                ..
            } = inst
            {
                for p in premises {
                    match self.ir.get(*p) {
                        Inst::Atom { args, .. } => {
                            for a in args {
                                self.check_term_fn_arities(*a)?;
                            }
                        }
                        Inst::NegAtom(inner) => {
                            if let Inst::Atom { args, .. } = self.ir.get(*inner) {
                                for a in args {
                                    self.check_term_fn_arities(*a)?;
                                }
                            }
                        }
                        Inst::Eq(l, r) | Inst::Ineq(l, r) => {
                            self.check_term_fn_arities(*l)?;
                            self.check_term_fn_arities(*r)?;
                        }
                        _ => {}
                    }
                }
                for t in transform {
                    if let Inst::Transform { app, .. } = self.ir.get(*t) {
                        self.check_term_fn_arities(*app)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Recursively checks function applications inside a term tree.
    fn check_term_fn_arities(&self, id: InstId) -> Result<()> {
        match self.ir.get(id) {
            Inst::ApplyFn { function, args } => {
                let fname = self.ir.resolve_name(*function);
                match builtin_arity(fname) {
                    Some(FnArity::Fixed(n)) if args.len() != n => {
                        return Err(anyhow!(
                            "function {} expects {} argument{}, provided {}",
                            fname,
                            n,
                            if n == 1 { "" } else { "s" },
                            args.len()
                        ));
                    }
                    Some(FnArity::AtLeastOne) if args.is_empty() => {
                        return Err(anyhow!(
                            "reducer function {} expects at least one argument",
                            fname
                        ));
                    }
                    Some(FnArity::Even) if args.len() % 2 != 0 => {
                        return Err(anyhow!(
                            "expect even number of arguments for {} - use the literal syntax for {}s",
                            fname,
                            if fname == "fn:struct" {
                                "struct"
                            } else {
                                "map"
                            }
                        ));
                    }
                    Some(_) => {}
                    None => {
                        return Err(anyhow!("unknown function {}", fname));
                    }
                }
                for a in args {
                    self.check_term_fn_arities(*a)?;
                }
                Ok(())
            }
            Inst::List(elems) => {
                for e in elems {
                    self.check_term_fn_arities(*e)?;
                }
                Ok(())
            }
            Inst::Map { keys, values } => {
                for k in keys {
                    self.check_term_fn_arities(*k)?;
                }
                for v in values {
                    self.check_term_fn_arities(*v)?;
                }
                Ok(())
            }
            Inst::Struct { values, .. } => {
                for v in values {
                    self.check_term_fn_arities(*v)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Pass 3: Binding (safety) analysis for every rule, mirroring mangle-go's
    /// `Analyzer.CheckRule`. A variable is bound when:
    /// - it appears in a positive (non-builtin) atom, or
    /// - it is unified (via an equality) with a constant or bound variable, or
    /// - it is an output of a built-in predicate (`:match_field`,
    ///   `:match_entry`, `:list:member`), or
    /// - it is defined by a `let` in the rule's transform.
    ///
    /// Additionally checks that transforms do not redefine body variables,
    /// that group_by keys are bound variables, and that head variables are
    /// either group_by keys or transform definitions.
    fn check_bindings(&mut self) -> Result<()> {
        // Collect rules first: the checks below only read the IR, but keeping
        // them in a separate pass avoids borrow conflicts on self.ir.
        let mut rules: Vec<(InstId, Vec<InstId>, Vec<InstId>)> = Vec::new();
        let mut facts: Vec<InstId> = Vec::new();
        for inst in self.ir.insts.iter() {
            if let Inst::Rule {
                head,
                premises,
                transform,
            } = inst
            {
                if !premises.is_empty() || !transform.is_empty() {
                    rules.push((*head, premises.clone(), transform.clone()));
                } else {
                    facts.push(*head);
                }
            }
        }

        // Facts (unit clauses) cannot bind variables: every variable in a
        // fact head is an error (matching mangle-go's CheckRule, which
        // requires head variables to be bound by the — empty — body).
        for head in facts {
            let mut vars = FxHashSet::default();
            for arg in self.atom_args(head) {
                self.term_vars(arg, &mut vars);
            }
            if let Some(v) = vars.iter().next() {
                let pred_name = self
                    .atom_predicate(head)
                    .map(|p| self.ir.resolve_name(p).to_string())
                    .unwrap_or_else(|| "?".to_string());
                return Err(anyhow!(
                    "variable {} in fact {}(...) is not bound: facts must be ground",
                    self.var_name(*v),
                    pred_name
                ));
            }
        }

        for (head, premises, transform) in rules {
            self.check_rule_bindings(head, &premises, &transform)?;
        }
        Ok(())
    }

    /// Binding analysis for a single rule.
    fn check_rule_bindings(
        &self,
        head: InstId,
        premises: &[InstId],
        transform: &[InstId],
    ) -> Result<()> {
        let head_args = self.atom_args(head);
        let pred_name = self
            .atom_predicate(head)
            .map(|p| self.ir.resolve_name(p).to_string())
            .unwrap_or_else(|| "?".to_string());

        let mut head_vars: FxHashSet<NameId> = FxHashSet::default();
        for arg in head_args {
            self.term_vars(arg, &mut head_vars);
        }

        let mut bound: FxHashSet<NameId> = FxHashSet::default();
        let mut seen: FxHashSet<NameId> = head_vars.clone();
        // Variable-variable equalities whose binding is still pending.
        let mut pending_eqs: Vec<(NameId, NameId)> = Vec::new();
        // Variables used inside apply-expressions of equalities; checked at
        // the end so forward references via later atoms are still errors.
        let mut eq_expr_vars: Vec<NameId> = Vec::new();

        // --- Premises: evaluate left-to-right, binding as we go. ---
        for &p in premises {
            match self.ir.get(p) {
                Inst::Atom { predicate, args } => {
                    let pred = self.ir.resolve_name(*predicate).to_string();
                    let mut vars = FxHashSet::default();
                    for a in args {
                        self.term_vars(*a, &mut vars);
                    }
                    seen.extend(vars.iter().copied());

                    match pred.as_str() {
                        // Filter predicates: every variable must already be
                        // bound (evaluation is left-to-right).
                        p if FILTER_PREDS.contains(&p) => {
                            for v in &vars {
                                if !bound.contains(v) {
                                    return Err(anyhow!(
                                        "variable {} in {}(...) of rule for {} will not have a value yet; move the subgoal to the right",
                                        self.var_name(*v),
                                        pred,
                                        pred_name
                                    ));
                                }
                            }
                        }
                        // :match_field(Struct, Field, Value): inputs bound,
                        // Value is an output.
                        ":match_field" | ":match_entry" => {
                            if args.len() == 3 {
                                let mut inputs = FxHashSet::default();
                                self.term_vars(args[0], &mut inputs);
                                self.term_vars(args[1], &mut inputs);
                                for v in &inputs {
                                    if !bound.contains(v) {
                                        return Err(anyhow!(
                                            "variable {} in {}(...) of rule for {} will not have a value yet; move the subgoal to the right",
                                            self.var_name(*v),
                                            pred,
                                            pred_name
                                        ));
                                    }
                                }
                                bound.extend(inputs);
                                if let Inst::Var(v) = self.ir.get(args[2]) {
                                    bound.insert(*v);
                                }
                            }
                        }
                        // :list:member(Elem, List): List is an input, Elem an output.
                        ":list:member" => {
                            if args.len() == 2 {
                                let mut inputs = FxHashSet::default();
                                self.term_vars(args[1], &mut inputs);
                                for v in &inputs {
                                    if !bound.contains(v) {
                                        return Err(anyhow!(
                                            "variable {} in :list:member(...) of rule for {} will not have a value yet; move the subgoal to the right",
                                            self.var_name(*v),
                                            pred_name
                                        ));
                                    }
                                }
                                bound.extend(inputs);
                                if let Inst::Var(v) = self.ir.get(args[0]) {
                                    bound.insert(*v);
                                }
                            }
                        }
                        // Regular atoms bind all their variables.
                        _ => {
                            bound.extend(vars.iter().copied());
                        }
                    }
                }
                Inst::NegAtom(inner) => {
                    // Negated atoms consume bindings; they never produce any.
                    if let Inst::Atom { args, .. } = self.ir.get(*inner) {
                        for a in args {
                            self.term_vars(*a, &mut seen);
                        }
                    }
                }
                Inst::Eq(l, r) => {
                    let mut lvars = FxHashSet::default();
                    let mut rvars = FxHashSet::default();
                    self.term_vars(*l, &mut lvars);
                    self.term_vars(*r, &mut rvars);
                    seen.extend(lvars.iter().copied());
                    seen.extend(rvars.iter().copied());

                    match (self.ir.get(*l), self.ir.get(*r)) {
                        (Inst::Var(lv), Inst::Var(rv)) => {
                            if bound.contains(lv) {
                                bound.insert(*rv);
                            } else if bound.contains(rv) {
                                bound.insert(*lv);
                            } else {
                                // Neither side bound yet: may be resolved by a
                                // later premise; re-check after the scan.
                                pending_eqs.push((*lv, *rv));
                            }
                        }
                        (Inst::Var(lv), _) => {
                            if rvars.iter().all(|v| bound.contains(v)) {
                                bound.insert(*lv);
                            } else {
                                eq_expr_vars.extend(rvars);
                            }
                        }
                        (_, Inst::Var(rv)) => {
                            if lvars.iter().all(|v| bound.contains(v)) {
                                bound.insert(*rv);
                            } else {
                                eq_expr_vars.extend(lvars);
                            }
                        }
                        _ => {}
                    }
                }
                Inst::Ineq(l, r) => {
                    // Inequality is a filter: both sides must be bound here.
                    let mut vars = FxHashSet::default();
                    self.term_vars(*l, &mut vars);
                    self.term_vars(*r, &mut vars);
                    seen.extend(vars.iter().copied());
                    for v in &vars {
                        if !bound.contains(v) {
                            return Err(anyhow!(
                                "variable {} in inequality of rule for {} will not have a value yet; move the subgoal to the right",
                                self.var_name(*v),
                                pred_name
                            ));
                        }
                    }
                }
                _ => {}
            }
        }

        // Resolve pending variable-variable equalities to a fixpoint
        // (e.g. `X = Y, Y = Z, p(Z)` binds all three).
        let mut progressed = true;
        while progressed {
            progressed = false;
            for &(l, r) in &pending_eqs {
                if bound.contains(&l) && !bound.contains(&r) {
                    bound.insert(r);
                    progressed = true;
                } else if bound.contains(&r) && !bound.contains(&l) {
                    bound.insert(l);
                    progressed = true;
                }
            }
        }

        // Variables used in apply-expressions of equalities must be bound.
        for v in &eq_expr_vars {
            if !bound.contains(v) {
                return Err(anyhow!(
                    "variable {} in equality expression of rule for {} is not bound",
                    self.var_name(*v),
                    pred_name
                ));
            }
        }

        // --- Transforms. ---
        // `defs` accumulates all transform definitions (for the final
        // checks); `available` tracks which variables are in scope for each
        // block (mirroring the planner's temp-relation capture).
        let mut defs: FxHashSet<NameId> = FxHashSet::default();
        let mut uses: FxHashSet<NameId> = FxHashSet::default();
        let mut available: FxHashSet<NameId> = bound.clone();

        // Split into blocks by 'do' statements (same as the planner).
        let mut blocks: Vec<Vec<InstId>> = Vec::new();
        let mut current: Vec<InstId> = Vec::new();
        for &t in transform {
            if let Inst::Transform { var: None, .. } = self.ir.get(t) {
                blocks.push(std::mem::take(&mut current));
            }
            current.push(t);
        }
        blocks.push(current);

        // Block 0: leading let-statements evaluated per joined row.
        for &t in &blocks[0] {
            if let Inst::Transform { var: Some(v), app } = self.ir.get(t) {
                let (v, app) = (*v, *app);
                self.term_vars(app, &mut uses);
                if self.is_wildcard(v) {
                    continue; // `let _ = ...` is allowed.
                }
                if bound.contains(&v) {
                    return Err(anyhow!(
                        "the transform of rule for {} redefines variable {} from rule body",
                        pred_name,
                        self.var_name(v)
                    ));
                }
                defs.insert(v);
                available.insert(v);
            }
        }

        // Later blocks: each starts with `do fn:group_by(...)`.
        let mut last_group_keys: Option<FxHashSet<NameId>> = None;
        let mut last_block_defs: FxHashSet<NameId> = FxHashSet::default();
        for block in blocks.iter().skip(1) {
            let Some(&do_stmt) = block.first() else {
                continue;
            };
            let app = match self.ir.get(do_stmt) {
                Inst::Transform { app, .. } => *app,
                _ => continue,
            };
            let (function, key_args) = match self.ir.get(app) {
                Inst::ApplyFn { function, args } => (*function, args.clone()),
                _ => {
                    return Err(anyhow!(
                        "do-transform of rule for {} must apply a function",
                        pred_name
                    ));
                }
            };
            let fn_name = self.ir.resolve_name(function);
            if fn_name != "fn:group_by" {
                return Err(anyhow!(
                    "unsupported do-transform fn {} in rule for {} (only fn:group_by is supported)",
                    fn_name,
                    pred_name
                ));
            }

            // Group keys: distinct variables in scope.
            let mut group_keys: FxHashSet<NameId> = FxHashSet::default();
            for &k in &key_args {
                let Inst::Var(v) = self.ir.get(k) else {
                    return Err(anyhow!(
                        "each argument of group_by must be a variable in rule for {}",
                        pred_name
                    ));
                };
                if !group_keys.insert(*v) {
                    return Err(anyhow!(
                        "each argument of group_by must be a distinct variable in rule for {}",
                        pred_name
                    ));
                }
                if !available.contains(v) {
                    return Err(anyhow!(
                        "group_by key {} is not bound in rule for {}",
                        self.var_name(*v),
                        pred_name
                    ));
                }
            }

            // Statements after group_by must be let-statements.
            let mut block_defs: FxHashSet<NameId> = FxHashSet::default();
            for &t in block.iter().skip(1) {
                let Inst::Transform { var: Some(v), app } = self.ir.get(t) else {
                    return Err(anyhow!(
                        "all statements following group_by have to be let-statements in rule for {}",
                        pred_name
                    ));
                };
                let (v, app) = (*v, *app);
                if self.is_wildcard(v) {
                    continue;
                }
                if bound.contains(&v) {
                    return Err(anyhow!(
                        "the transform of rule for {} redefines variable {} from rule body",
                        pred_name,
                        self.var_name(v)
                    ));
                }

                let mut fn_vars = FxHashSet::default();
                self.term_vars(app, &mut fn_vars);
                let is_reducer = match self.ir.get(app) {
                    Inst::ApplyFn { function, .. } => {
                        REDUCER_FNS.contains(&self.ir.resolve_name(*function))
                    }
                    _ => false,
                };
                if is_reducer {
                    // Reducers aggregate over the group's rows: their
                    // variables must be columns of the source relation.
                    for fv in &fn_vars {
                        if !available.contains(fv) {
                            return Err(anyhow!(
                                "variable {} used in transform of rule for {} is not in scope",
                                self.var_name(*fv),
                                pred_name
                            ));
                        }
                    }
                } else {
                    // Regular let-expressions see only this block's group
                    // keys and previously defined transform variables.
                    for fv in &fn_vars {
                        if !(group_keys.contains(fv) || block_defs.contains(fv)) {
                            return Err(anyhow!(
                                "variable {} in transform of rule for {} must be either part of group_by or defined in the transform",
                                self.var_name(*fv),
                                pred_name
                            ));
                        }
                    }
                }
                defs.insert(v);
                block_defs.insert(v);
            }
            last_group_keys = Some(group_keys);
            last_block_defs = block_defs;
            // After a group_by block, only its keys and definitions survive
            // as columns of the materialized relation.
            available = last_group_keys
                .as_ref()
                .unwrap()
                .union(&last_block_defs)
                .copied()
                .collect();
        }

        // --- Head variables. ---
        // With a group_by transform, head variables must be group keys or
        // transform definitions of the final block (matching mangle-go);
        // otherwise they must be bound by the body or a transform let.
        for v in &head_vars {
            let ok = match &last_group_keys {
                Some(keys) => keys.contains(v) || last_block_defs.contains(v),
                None => bound.contains(v) || defs.contains(v),
            };
            if !ok {
                if last_group_keys.is_some() {
                    return Err(anyhow!(
                        "head variable {} of rule for {} is neither part of group_by nor aggregated",
                        self.var_name(*v),
                        pred_name
                    ));
                }
                return Err(anyhow!(
                    "variable {} in head of rule for {} is not bound",
                    self.var_name(*v),
                    pred_name
                ));
            }
        }

        // --- Every variable seen anywhere must be bound somewhere. ---
        for v in &seen {
            if !(bound.contains(v) || defs.contains(v)) {
                return Err(anyhow!(
                    "variable {} in rule for {} is not bound",
                    self.var_name(*v),
                    pred_name
                ));
            }
        }

        // --- Transform uses must refer to clause variables. ---
        for v in &uses {
            if !(seen.contains(v) || defs.contains(v)) {
                return Err(anyhow!(
                    "variable {} used in transform of rule for {} does not appear in clause",
                    self.var_name(*v),
                    pred_name
                ));
            }
        }

        Ok(())
    }

    /// Whether a variable name denotes the anonymous wildcard `_`.
    fn is_wildcard(&self, v: NameId) -> bool {
        self.ir.resolve_name(v) == "_"
    }

    fn var_name(&self, v: NameId) -> &str {
        self.ir.resolve_name(v)
    }

    /// Collects the (non-wildcard) variables of a base-term instruction tree.
    fn term_vars(&self, id: InstId, out: &mut FxHashSet<NameId>) {
        match self.ir.get(id) {
            Inst::Var(v) => {
                if !self.is_wildcard(*v) {
                    out.insert(*v);
                }
            }
            Inst::List(elems) => {
                for e in elems {
                    self.term_vars(*e, out);
                }
            }
            Inst::Map { keys, values } => {
                for k in keys {
                    self.term_vars(*k, out);
                }
                for v in values {
                    self.term_vars(*v, out);
                }
            }
            Inst::Struct { values, .. } => {
                for v in values {
                    self.term_vars(*v, out);
                }
            }
            Inst::ApplyFn { args, .. } => {
                for a in args {
                    self.term_vars(*a, out);
                }
            }
            _ => {}
        }
    }

    /// Check a fact (unit clause head) against declared bound alternatives.
    fn check_fact(&self, head: InstId, alternatives: &[Vec<InstId>]) -> Result<()> {
        let args = self.atom_args(head);
        let pred = self.atom_predicate(head).unwrap();
        if args.is_empty() && alternatives.is_empty() {
            return Ok(());
        }

        let mut errors = Vec::new();
        for alt in alternatives {
            match self.check_fact_against_bound(pred, &args, alt) {
                Ok(()) => return Ok(()),
                Err(e) => errors.push(e.to_string()),
            }
        }

        if errors.is_empty() {
            return Ok(());
        }

        let pred_name = self
            .atom_predicate(head)
            .map(|p| self.ir.resolve_name(p).to_string())
            .unwrap_or_else(|| "?".to_string());
        Err(anyhow!(
            "fact {}(...) matches none of the bound decls: {}",
            pred_name,
            errors.join("; ")
        ))
    }

    /// Check a single fact against one bound alternative.
    fn check_fact_against_bound(
        &self,
        pred: NameId,
        args: &[InstId],
        bound: &[InstId],
    ) -> Result<()> {
        let is_temporal = self.ir.temporal_predicates.contains(&pred);
        let expected_args = if is_temporal {
            bound.len() + 2
        } else {
            bound.len()
        };
        if args.len() != expected_args {
            return Err(anyhow!(
                "arity mismatch: fact has {} args, bound has {}{}",
                args.len(),
                bound.len(),
                if is_temporal { " (+2 temporal)" } else { "" }
            ));
        }
        for (i, (arg, type_expr)) in args.iter().zip(bound.iter()).enumerate() {
            if !type_expr::has_type(self.ir, *type_expr, *arg) {
                let arg_desc = self.describe_inst(*arg);
                let type_desc = self.describe_inst(*type_expr);
                return Err(anyhow!(
                    "argument {} ({}) does not have type {}",
                    i,
                    arg_desc,
                    type_desc
                ));
            }
        }
        Ok(())
    }

    /// Check a rule against declared bound alternatives.
    ///
    /// Uses the inference pipeline: for each premise, infer variable types
    /// via feasible alternatives, then check that head args conform.
    /// Runs the type-inference pipeline for a rule and returns the inferred
    /// head argument types. Also (as a side effect) collects function
    /// argument-type errors via `bound_of_arg`/`bound_of_apply_fn`.
    fn infer_rule_types(
        &mut self,
        head: InstId,
        premises: &[InstId],
        transforms: &[InstId],
    ) -> Result<Vec<InstId>> {
        let head_args = self.atom_args(head);

        // Run inference pipeline.
        let mut state = InferState::new();
        for premise_id in premises {
            state = self.infer_from_premise(*premise_id, state)?;
        }

        // Process transforms.
        for transform_id in transforms {
            if let Inst::Transform { var, app } = self.ir.get(*transform_id) {
                let var = *var;
                let app = *app;
                if let Some(v) = var {
                    let tpe = self.bound_of_arg(app, &state.as_map());
                    self.refine_var(&mut state, v, tpe, "transform")?;
                }
            }
        }

        // Compute head tuple types.
        let var_ranges = state.as_map();
        Ok(head_args
            .iter()
            .map(|arg| self.bound_of_arg(*arg, &var_ranges))
            .collect())
    }

    fn check_rule(
        &mut self,
        head: InstId,
        premises: &[InstId],
        transforms: &[InstId],
        alternatives: &[Vec<InstId>],
    ) -> Result<()> {
        let pred = self.atom_predicate(head).unwrap();
        let is_temporal = self.ir.temporal_predicates.contains(&pred);

        let inferred = self.infer_rule_types(head, premises, transforms)?;

        // For temporal predicates, trim synthetic time columns.
        let check_len = if is_temporal && inferred.len() >= 2 {
            inferred.len() - 2
        } else {
            inferred.len()
        };
        let inferred_trimmed = &inferred[..check_len];

        // Check inferred types against each declared alternative.
        let mut errors = Vec::new();
        for alt in alternatives {
            if alt.len() != inferred_trimmed.len() {
                errors.push(format!(
                    "arity mismatch: head has {} args, bound has {}",
                    inferred_trimmed.len(),
                    alt.len()
                ));
                continue;
            }
            // Build type context: map any type variables in the alt to /any.
            let any = type_expr::find_or_create_name(self.ir, "/any");
            let mut ctx = TypeContext::default();
            for t in alt.iter() {
                let mut vars = FxHashSet::default();
                type_expr::collect_vars(self.ir, *t, &mut vars);
                for v in vars {
                    ctx.entry(v).or_insert(any);
                }
            }
            let all_conform = inferred_trimmed
                .iter()
                .zip(alt.iter())
                .all(|(inf, decl)| type_expr::set_conforms(self.ir, &ctx, *inf, *decl));
            if all_conform {
                return Ok(());
            }
            errors.push(format!(
                "inferred [{}] does not conform to declared [{}]",
                inferred_trimmed
                    .iter()
                    .map(|i| self.describe_inst(*i))
                    .collect::<Vec<_>>()
                    .join(", "),
                alt.iter()
                    .map(|i| self.describe_inst(*i))
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        }

        if errors.is_empty() {
            return Ok(());
        }

        let pred_name = self
            .atom_predicate(head)
            .map(|p| self.ir.resolve_name(p).to_string())
            .unwrap_or_else(|| "?".to_string());
        Err(anyhow!(
            "rule for {}(...) does not conform to declared bounds: {}",
            pred_name,
            errors.join("; ")
        ))
    }

    /// Infer variable types from a single premise, updating the state.
    fn infer_from_premise(
        &mut self,
        premise_id: InstId,
        mut state: InferState,
    ) -> Result<InferState> {
        match self.ir.get(premise_id) {
            Inst::Atom { predicate, args } => {
                let pred = *predicate;
                let args = args.clone();

                // Special case: :match_prefix
                let pred_name = self.ir.resolve_name(pred).to_string();
                if pred_name == ":match_prefix" {
                    return self.infer_match_prefix(&args, state);
                }
                if pred_name == ":match_field" {
                    return self.infer_match_field(&args, state);
                }
                if pred_name == ":match_entry" {
                    return self.infer_match_entry(&args, state);
                }
                if pred_name == ":list:member" {
                    return self.infer_list_member(&args, state);
                }

                // Regular atom: look up or infer alternatives.
                let var_ranges = state.as_map();
                let feasible = self.get_or_infer_alternatives(pred, &args, &var_ranges);

                if !feasible.is_empty() {
                    // Use the first feasible alternative to bind variables.
                    let first = &feasible[0].clone();
                    for (arg, type_id) in args.iter().zip(first.iter()) {
                        if let Inst::Var(v) = self.ir.get(*arg) {
                            let v = *v;
                            state.add_or_refine_with_ir(self.ir, v, *type_id);
                        }
                    }
                } else if let Some(alternatives) = self.rel_type_map.get(&pred).cloned() {
                    // Fallback: no feasible alternative, use first declared alt.
                    if let Some(first_alt) = alternatives.first() {
                        for (arg, type_id) in args.iter().zip(first_alt.iter()) {
                            if let Inst::Var(v) = self.ir.get(*arg) {
                                let v = *v;
                                state.add_or_refine_with_ir(self.ir, v, *type_id);
                            }
                        }
                    }
                }
                Ok(state)
            }
            Inst::NegAtom(inner) => {
                let inner = *inner;
                // Negated atoms: we can refine types via negative information,
                // but don't add new bindings.
                if let Inst::Atom { predicate, args } = self.ir.get(inner) {
                    let pred = *predicate;
                    let args = args.clone();
                    let pred_name = self.ir.resolve_name(pred).to_string();

                    if pred_name == ":match_prefix" && args.len() >= 2 {
                        // Negative :match_prefix: refine away the prefix type.
                        if let Inst::Var(v) = self.ir.get(args[0]) {
                            let v = *v;
                            let bound = self.bound_of_arg(args[1], &state.as_map());
                            if let Some(existing) = state.as_map().get(&v).copied()
                                && type_expr::is_union_type(self.ir, existing)
                            {
                                let refined =
                                    type_expr::remove_from_union_type(self.ir, bound, existing);
                                if !type_expr::is_empty_type(self.ir, refined) {
                                    state.set_var(v, refined);
                                }
                            }
                        }
                    }
                    // Other negated atoms: no type refinement.
                }
                Ok(state)
            }
            Inst::Eq(left, right) => {
                let left = *left;
                let right = *right;
                let var_ranges = state.as_map();

                if let Inst::Var(lv) = self.ir.get(left) {
                    let lv = *lv;
                    let tpe = self.bound_of_arg(right, &var_ranges);
                    self.refine_var(&mut state, lv, tpe, "equality")?;
                }
                if let Inst::Var(rv) = self.ir.get(right) {
                    let rv = *rv;
                    let tpe = self.bound_of_arg(left, &state.as_map());
                    self.refine_var(&mut state, rv, tpe, "equality")?;
                }
                Ok(state)
            }
            Inst::Ineq(left, right) => {
                let left = *left;
                let right = *right;
                let var_ranges = state.as_map();

                // For inequality, both sides must have compatible types.
                let left_tpe = self.bound_of_arg(left, &var_ranges);
                let right_tpe = self.bound_of_arg(right, &var_ranges);
                let ctx = TypeContext::default();
                let meet = type_expr::lower_bound(self.ir, &ctx, &[left_tpe, right_tpe]);
                if !type_expr::is_empty_type(self.ir, meet) {
                    if let Inst::Var(lv) = self.ir.get(left) {
                        let lv = *lv;
                        state.add_or_refine_with_ir(self.ir, lv, meet);
                    }
                    if let Inst::Var(rv) = self.ir.get(right) {
                        let rv = *rv;
                        state.add_or_refine_with_ir(self.ir, rv, meet);
                    }
                }
                Ok(state)
            }
            _ => Ok(state),
        }
    }

    /// Finds feasible alternatives for a subgoal p(e1...eN) with skolemization.
    ///
    /// For each declared alternative:
    /// 1. Builds argument bounds (uses var_ranges for bound vars, declared type for unbound)
    /// 2. Collects type variables from the alternative, creates fresh substitution
    /// 3. Applies substitution to both arg bounds and alternative types
    /// 4. Checks that LowerBound (with extended type context) is non-empty per position
    fn feasible_alternatives(
        &mut self,
        alternatives: &[Vec<InstId>],
        args: &[InstId],
        var_ranges: &FxHashMap<NameId, InstId>,
    ) -> Vec<Vec<InstId>> {
        let mut feasible = Vec::new();

        for alt in alternatives {
            if alt.len() != args.len() {
                continue;
            }

            // Step 1: Build argument bounds.
            // For bound vars: use var_ranges. For unbound vars: use declared type.
            // For constants: use bound_of_arg.
            let mut arg_bound = Vec::new();
            for (i, arg) in args.iter().enumerate() {
                if let Inst::Var(v) = self.ir.get(*arg) {
                    let v = *v;
                    if let Some(&range) = var_ranges.get(&v) {
                        arg_bound.push(range);
                    } else {
                        // Unbound variable: use declared type from this alternative.
                        arg_bound.push(alt[i]);
                    }
                } else {
                    arg_bound.push(self.bound_of_arg(*arg, var_ranges));
                }
            }

            // Step 2: Collect type variables from the alternative.
            let mut type_vars = FxHashSet::default();
            for t in alt {
                type_expr::collect_vars(self.ir, *t, &mut type_vars);
            }

            // Step 3: Skolemize — create fresh variables for each type variable.
            let mut subst: FxHashMap<NameId, InstId> = FxHashMap::default();
            if !type_vars.is_empty() {
                for v in &type_vars {
                    let fresh = self.fresh_var();
                    let fresh_id = self.ir.add_inst(Inst::Var(fresh));
                    subst.insert(*v, fresh_id);
                }
            }

            // Step 4: Apply substitution to arg bounds and alternative.
            let arg_bound_subst: Vec<InstId> = arg_bound
                .iter()
                .map(|t| type_expr::apply_subst(self.ir, *t, &subst))
                .collect();
            let alt_subst: Vec<InstId> = alt
                .iter()
                .map(|t| type_expr::apply_subst(self.ir, *t, &subst))
                .collect();

            // Step 5: Build extended type context with fresh vars -> /any.
            let any = type_expr::find_or_create_name(self.ir, "/any");
            let mut ctx = TypeContext::default();
            for fresh_id in subst.values() {
                if let Inst::Var(v) = self.ir.get(*fresh_id) {
                    ctx.insert(*v, any);
                }
            }

            // Step 6: Per-position feasibility check.
            let mut is_feasible = true;
            let mut result_types = Vec::new();
            for (ab, at) in arg_bound_subst.iter().zip(alt_subst.iter()) {
                let meet = type_expr::lower_bound(self.ir, &ctx, &[*ab, *at]);
                if type_expr::is_empty_type(self.ir, meet) {
                    is_feasible = false;
                    break;
                }
                result_types.push(meet);
            }

            if is_feasible {
                feasible.push(result_types);
            }
        }
        feasible
    }

    /// Looks up or infers type alternatives for a predicate.
    ///
    /// Checks declared types first, then already-inferred types, then infers
    /// from rules. Uses cycle detection to handle recursive predicates.
    fn get_or_infer_alternatives(
        &mut self,
        pred: NameId,
        args: &[InstId],
        var_ranges: &FxHashMap<NameId, InstId>,
    ) -> Vec<Vec<InstId>> {
        // 1. Check declared types.
        if let Some(alts) = self.rel_type_map.get(&pred).cloned() {
            return self.feasible_alternatives(&alts, args, var_ranges);
        }

        // 2. Check already-inferred types.
        if let Some(alts) = self.inferred.get(&pred).cloned() {
            return self.feasible_alternatives(&alts, args, var_ranges);
        }

        // 3. Cycle detection: if we're already visiting this predicate,
        // return [/any ... /any] to break the cycle.
        if self.visiting.contains(&pred) {
            let any = type_expr::find_or_create_name(self.ir, "/any");
            return vec![vec![any; args.len()]];
        }

        // 4. Infer from rules defining this predicate.
        self.visiting.insert(pred);
        let inferred = self.infer_rel_types(pred);
        self.visiting.remove(&pred);

        if !inferred.is_empty() {
            self.inferred.insert(pred, inferred.clone());
            return self.feasible_alternatives(&inferred, args, var_ranges);
        }

        Vec::new()
    }

    /// Infers relation type alternatives for a predicate from its defining rules.
    ///
    /// For each rule defining the predicate, runs inference to determine
    /// the head tuple types, then collects all alternatives.
    fn infer_rel_types(&mut self, pred: NameId) -> Vec<Vec<InstId>> {
        let rules = match self.rules_map.get(&pred) {
            Some(r) => r.clone(),
            None => return Vec::new(),
        };

        let mut alternatives: Vec<Vec<InstId>> = Vec::new();

        for (head, premises, transforms) in &rules {
            // Run inference pipeline on this clause.
            if let Some(inferred) = self.infer_clause(*head, premises, transforms) {
                alternatives.push(inferred);
            }
        }

        alternatives
    }

    /// Runs inference on a single clause, returning inferred head tuple types.
    fn infer_clause(
        &mut self,
        head: InstId,
        premises: &[InstId],
        transforms: &[InstId],
    ) -> Option<Vec<InstId>> {
        let head_args = self.atom_args(head);
        let mut state = InferState::new();

        for premise_id in premises {
            match self.infer_from_premise(*premise_id, state) {
                Ok(new_state) => state = new_state,
                Err(_) => return None,
            }
        }

        // Process transforms.
        for transform_id in transforms {
            if let Inst::Transform { var, app } = self.ir.get(*transform_id) {
                let var = *var;
                let app = *app;
                if let Some(v) = var {
                    let tpe = self.bound_of_arg(app, &state.as_map());
                    state.add_or_refine_with_ir(self.ir, v, tpe);
                }
            }
        }

        // Compute head tuple types.
        let var_ranges = state.as_map();
        let inferred: Vec<InstId> = head_args
            .iter()
            .map(|arg| self.bound_of_arg(*arg, &var_ranges))
            .collect();

        Some(inferred)
    }

    /// Special case inference for `:match_prefix(Name, Prefix)`.
    /// Refines a variable's inferred type with `tpe`, returning an error when
    /// the two are provably disjoint (e.g. a /number variable unified with a
    /// string constant — the rule can never derive anything). Without this,
    /// empty meets were silently dropped and mismatched rules passed the
    /// declared-bounds check.
    fn refine_var(
        &mut self,
        state: &mut InferState,
        var: NameId,
        tpe: InstId,
        context: &str,
    ) -> Result<()> {
        if let Some(&existing) = state.as_map().get(&var) {
            let ctx = TypeContext::default();
            let meet = type_expr::lower_bound(self.ir, &ctx, &[existing, tpe]);
            if type_expr::is_empty_type(self.ir, meet) {
                return Err(anyhow!(
                    "{}: variable {} has type {} but is used as {}",
                    context,
                    self.var_name(var),
                    self.describe_inst(existing),
                    self.describe_inst(tpe)
                ));
            }
        }
        state.add_or_refine_with_ir(self.ir, var, tpe);
        Ok(())
    }

    fn infer_match_prefix(&mut self, args: &[InstId], mut state: InferState) -> Result<InferState> {
        if args.len() != 2 {
            return Ok(state);
        }
        let var_ranges = state.as_map();
        let tpe = self.bound_of_arg(args[0], &var_ranges);
        // The prefix term itself acts as the type (mangle-go meets with the
        // raw `args[1]`): a name constant /foo means "names under /foo",
        // which is tighter than running it through the name trie.
        let prefix = match self.ir.get(args[1]) {
            Inst::Name(_) => args[1],
            _ => self.bound_of_arg(args[1], &var_ranges),
        };

        let ctx = TypeContext::default();
        let meet = type_expr::lower_bound(self.ir, &ctx, &[tpe, prefix]);
        if type_expr::is_empty_type(self.ir, meet) {
            return Err(anyhow!(
                ":match_prefix cannot succeed: type {} is incompatible with {}",
                self.describe_inst(tpe),
                self.describe_inst(prefix)
            ));
        }
        if let Inst::Var(v) = self.ir.get(args[0]) {
            let v = *v;
            state.add_or_refine_with_ir(self.ir, v, meet);
        }
        // Second arg (prefix) is typically a constant; a variable is /name.
        let name_type = type_expr::find_or_create_name(self.ir, "/name");
        if let Inst::Var(v) = self.ir.get(args[1]) {
            let v = *v;
            self.refine_var(&mut state, v, name_type, ":match_prefix")?;
        }
        Ok(state)
    }

    /// Special case inference for `:match_field(Struct, FieldName, Value)`.
    fn infer_match_field(&mut self, args: &[InstId], mut state: InferState) -> Result<InferState> {
        if args.len() != 3 {
            return Ok(state);
        }
        let var_ranges = state.as_map();
        let scrutinee_type = self.bound_of_arg(args[0], &var_ranges);

        // Get field name from args[1] (must be a name constant).
        let field_name_id = match self.ir.get(args[1]) {
            Inst::Name(n) => Some(*n),
            _ => None,
        };

        if let Some(field) = field_name_id
            && (type_expr::is_struct_type(self.ir, scrutinee_type)
                || type_expr::is_tagged_union_type(self.ir, scrutinee_type)
                || type_expr::is_union_type(self.ir, scrutinee_type))
            && let Some(field_type) =
                type_expr::struct_type_field_deep(self.ir, scrutinee_type, field)
        {
            // Bind the value variable.
            let ctx = TypeContext::default();
            let value_bound = self.bound_of_arg(args[2], &state.as_map());
            let meet = type_expr::lower_bound(self.ir, &ctx, &[value_bound, field_type]);
            if type_expr::is_empty_type(self.ir, meet) {
                return Err(anyhow!(
                    ":match_field on args: value type mismatch got {} want {}",
                    self.describe_inst(value_bound),
                    self.describe_inst(field_type)
                ));
            }
            if let Inst::Var(v) = self.ir.get(args[2]) {
                let v = *v;
                state.add_or_refine_with_ir(self.ir, v, meet);
            }
        }
        // Bind first arg if variable.
        let any = type_expr::find_or_create_name(self.ir, "/any");
        if let Inst::Var(v) = self.ir.get(args[0]) {
            let v = *v;
            state.add_or_refine_with_ir(self.ir, v, any);
        }
        // Bind second arg (field name) if variable.
        let name_type = type_expr::find_or_create_name(self.ir, "/name");
        if let Inst::Var(v) = self.ir.get(args[1]) {
            let v = *v;
            state.add_or_refine_with_ir(self.ir, v, name_type);
        }
        Ok(state)
    }

    /// Special case inference for `:match_entry(Map, Key, Value)`.
    fn infer_match_entry(&mut self, args: &[InstId], mut state: InferState) -> Result<InferState> {
        if args.len() != 3 {
            return Ok(state);
        }
        let var_ranges = state.as_map();
        let map_type = self.bound_of_arg(args[0], &var_ranges);

        if type_expr::is_map_type(self.ir, map_type)
            && let Some((key_type, val_type)) = type_expr::map_type_args(self.ir, map_type)
        {
            let ctx = TypeContext::default();

            // Bind key.
            let key_bound = self.bound_of_arg(args[1], &state.as_map());
            let key_meet = type_expr::lower_bound(self.ir, &ctx, &[key_bound, key_type]);
            if type_expr::is_empty_type(self.ir, key_meet) {
                return Err(anyhow!(
                    ":match_entry on args: key type mismatch got {} want {}",
                    self.describe_inst(key_bound),
                    self.describe_inst(key_type)
                ));
            }
            if let Inst::Var(v) = self.ir.get(args[1]) {
                let v = *v;
                state.add_or_refine_with_ir(self.ir, v, key_meet);
            }

            // Bind value.
            let val_bound = self.bound_of_arg(args[2], &state.as_map());
            let val_meet = type_expr::lower_bound(self.ir, &ctx, &[val_bound, val_type]);
            if type_expr::is_empty_type(self.ir, val_meet) {
                return Err(anyhow!(
                    ":match_entry on args: value type mismatch got {} want {}",
                    self.describe_inst(val_bound),
                    self.describe_inst(val_type)
                ));
            }
            if let Inst::Var(v) = self.ir.get(args[2]) {
                let v = *v;
                state.add_or_refine_with_ir(self.ir, v, val_meet);
            }
        }
        Ok(state)
    }

    /// Special case inference for `:list:member(Elem, List)`.
    fn infer_list_member(&mut self, args: &[InstId], mut state: InferState) -> Result<InferState> {
        if args.len() != 2 {
            return Ok(state);
        }
        let var_ranges = state.as_map();
        let list_type = self.bound_of_arg(args[1], &var_ranges);

        if type_expr::is_list_type(self.ir, list_type)
            && let Some(elem_type) = type_expr::list_type_arg(self.ir, list_type)
        {
            let ctx = TypeContext::default();
            let elem_bound = self.bound_of_arg(args[0], &state.as_map());
            let meet = type_expr::lower_bound(self.ir, &ctx, &[elem_bound, elem_type]);
            if type_expr::is_empty_type(self.ir, meet) {
                return Err(anyhow!(
                    ":list:member on args cannot succeed: element type {} is incompatible with {}",
                    self.describe_inst(elem_bound),
                    self.describe_inst(elem_type)
                ));
            }
            if let Inst::Var(v) = self.ir.get(args[0]) {
                let v = *v;
                state.add_or_refine_with_ir(self.ir, v, meet);
            }
        }
        Ok(state)
    }

    /// Infers the type bound for a single argument.
    fn bound_of_arg(&mut self, arg: InstId, var_ranges: &FxHashMap<NameId, InstId>) -> InstId {
        match self.ir.get(arg) {
            Inst::Var(v) => {
                let v = *v;
                if let Some(&range) = var_ranges.get(&v) {
                    range
                } else {
                    type_expr::find_or_create_name(self.ir, "/any")
                }
            }
            Inst::Number(_) => type_expr::find_or_create_name(self.ir, "/number"),
            Inst::Float(_) => type_expr::find_or_create_name(self.ir, "/float64"),
            Inst::String(_) => type_expr::find_or_create_name(self.ir, "/string"),
            Inst::Bool(_) => type_expr::find_or_create_name(self.ir, "/bool"),
            Inst::Time(_) => type_expr::find_or_create_name(self.ir, "/time"),
            Inst::Duration(_) => type_expr::find_or_create_name(self.ir, "/duration"),
            Inst::Bytes(_) => type_expr::find_or_create_name(self.ir, "/bytes"),
            Inst::Name(n) => {
                let name = self.ir.resolve_name(*n).to_string();
                let prefix = self.name_trie.prefix_name(&name);
                type_expr::find_or_create_name(self.ir, &prefix)
            }
            Inst::List(elems) => {
                let elems = elems.clone();
                if elems.is_empty() {
                    let bot = type_expr::find_or_create_name(self.ir, "/bot");
                    return type_expr::new_list_type(self.ir, bot);
                }
                let ctx = TypeContext::default();
                let elem_types: Vec<InstId> = elems
                    .iter()
                    .map(|e| self.bound_of_arg(*e, var_ranges))
                    .collect();
                let elem_type = type_expr::upper_bound(self.ir, &ctx, &elem_types);
                type_expr::new_list_type(self.ir, elem_type)
            }
            Inst::Map { keys, values } => {
                let keys = keys.clone();
                let values = values.clone();
                let ctx = TypeContext::default();
                let key_types: Vec<InstId> = keys
                    .iter()
                    .map(|k| self.bound_of_arg(*k, var_ranges))
                    .collect();
                let val_types: Vec<InstId> = values
                    .iter()
                    .map(|v| self.bound_of_arg(*v, var_ranges))
                    .collect();
                let kt = type_expr::upper_bound(self.ir, &ctx, &key_types);
                let vt = type_expr::upper_bound(self.ir, &ctx, &val_types);
                type_expr::new_map_type(self.ir, kt, vt)
            }
            Inst::Struct { fields, values } => {
                let fields = fields.clone();
                let values = values.clone();
                let mut args = Vec::new();
                for (f, v) in fields.iter().zip(values.iter()) {
                    let fname = self.ir.resolve_name(*f).to_string();
                    let fname_id = type_expr::find_or_create_name(self.ir, &fname);
                    let vtype = self.bound_of_arg(*v, var_ranges);
                    args.push(fname_id);
                    args.push(vtype);
                }
                type_expr::new_struct_type(self.ir, args)
            }
            Inst::ApplyFn { function, args } => {
                let fname = self.ir.resolve_name(*function).to_string();
                let args = args.clone();
                self.bound_of_apply_fn(&fname, &args, var_ranges)
            }
            _ => type_expr::find_or_create_name(self.ir, "/any"),
        }
    }

    /// Infers a type for a function application expression.
    /// Checks that one argument's inferred bound is compatible with an
    /// expected type. Records an error when the two are provably disjoint
    /// (e.g. `fn:plus` applied to a /string variable); an unbound variable
    /// (/any) never errors — inference may simply not know better yet.
    fn expect_arg(
        &mut self,
        fname: &str,
        arg: InstId,
        var_ranges: &FxHashMap<NameId, InstId>,
        expected: &str,
    ) {
        let bound = self.bound_of_arg(arg, var_ranges);
        let expected_t = type_expr::find_or_create_name(self.ir, expected);
        let ctx = TypeContext::default();
        let meet = type_expr::lower_bound(self.ir, &ctx, &[bound, expected_t]);
        if type_expr::is_empty_type(self.ir, meet) {
            let msg = format!(
                "{}: argument has type {}, expected {}",
                fname,
                self.describe_inst(bound),
                expected
            );
            if !self.fn_arg_errors.contains(&msg) {
                self.fn_arg_errors.push(msg);
            }
        }
    }

    /// Checks that every argument conforms to `expected` (varargs form).
    fn expect_all_args(
        &mut self,
        fname: &str,
        args: &[InstId],
        var_ranges: &FxHashMap<NameId, InstId>,
        expected: &str,
    ) {
        for arg in args {
            self.expect_arg(fname, *arg, var_ranges, expected);
        }
    }

    /// Checks arguments positionally against `spec` (fixed-arity form);
    /// extra arguments beyond the spec are not checked.
    fn expect_args(
        &mut self,
        fname: &str,
        args: &[InstId],
        var_ranges: &FxHashMap<NameId, InstId>,
        spec: &[&str],
    ) {
        for (arg, expected) in args.iter().zip(spec.iter()) {
            self.expect_arg(fname, *arg, var_ranges, expected);
        }
    }

    /// Checks that one argument's inferred bound is compatible with a
    /// pre-built expected type expression (e.g. a union).
    fn expect_arg_type(
        &mut self,
        fname: &str,
        arg: InstId,
        var_ranges: &FxHashMap<NameId, InstId>,
        expected: InstId,
        expected_desc: &str,
    ) {
        let bound = self.bound_of_arg(arg, var_ranges);
        let ctx = TypeContext::default();
        let meet = type_expr::lower_bound(self.ir, &ctx, &[bound, expected]);
        if type_expr::is_empty_type(self.ir, meet) {
            let msg = format!(
                "{}: argument has type {}, expected {}",
                fname,
                self.describe_inst(bound),
                expected_desc
            );
            if !self.fn_arg_errors.contains(&msg) {
                self.fn_arg_errors.push(msg);
            }
        }
    }

    /// The union type `/number | /float64` (functions that coerce integers
    /// to floats).
    fn num_or_float_type(&mut self) -> InstId {
        let n = type_expr::find_or_create_name(self.ir, "/number");
        let f = type_expr::find_or_create_name(self.ir, "/float64");
        type_expr::new_union_or_single(self.ir, vec![n, f])
    }

    /// Validates function argument types against the runtime semantics of
    /// the interpreter (the source of truth for this crate), mirroring
    /// mangle-go's `typeOfFn` argument checks. Result types are separate
    /// (see the match below).
    fn check_fn_arg_types(
        &mut self,
        fname: &str,
        args: &[InstId],
        var_ranges: &FxHashMap<NameId, InstId>,
    ) {
        match fname {
            // Integer arithmetic: strictly /number.
            "fn:plus" | "fn:minus" | "fn:mult" | "fn:div" => {
                self.expect_all_args(fname, args, var_ranges, "/number");
            }
            // Float arithmetic and sqrt: coerce /number to f64.
            "fn:float:plus" | "fn:float:minus" | "fn:float:mult" | "fn:float:div" | "fn:sqrt" => {
                let expected = self.num_or_float_type();
                for arg in args {
                    self.expect_arg_type(fname, *arg, var_ranges, expected, "/number or /float64");
                }
            }
            // Integer reducers: strictly /number.
            "fn:sum" | "fn:max" | "fn:min" => {
                self.expect_all_args(fname, args, var_ranges, "/number");
            }
            // Float reducers: coerce /number to f64.
            "fn:float:sum" | "fn:float:max" | "fn:float:min" => {
                let expected = self.num_or_float_type();
                for arg in args {
                    self.expect_arg_type(fname, *arg, var_ranges, expected, "/number or /float64");
                }
            }
            "fn:string:replace" => {
                self.expect_args(
                    fname,
                    args,
                    var_ranges,
                    &["/string", "/string", "/string", "/number"],
                );
            }
            // Time functions (strict argument types, matching the interpreter).
            "fn:time:year"
            | "fn:time:month"
            | "fn:time:day"
            | "fn:time:hour"
            | "fn:time:minute"
            | "fn:time:second"
            | "fn:time:to_unix_nanos"
            | "fn:time:trunc"
            | "fn:time:format" => {
                self.expect_args(fname, args, var_ranges, &["/time"]);
            }
            "fn:time:from_unix_nanos" => {
                self.expect_args(fname, args, var_ranges, &["/number"]);
            }
            "fn:time:parse_rfc3339" => {
                self.expect_args(fname, args, var_ranges, &["/string"]);
            }
            "fn:time:parse_civil" => {
                self.expect_args(fname, args, var_ranges, &["/string", "/string"]);
            }
            "fn:time:format_civil" => {
                self.expect_args(fname, args, var_ranges, &["/time", "/string", "/name"]);
            }
            "fn:time:add" => {
                self.expect_args(fname, args, var_ranges, &["/time", "/duration"]);
            }
            "fn:time:sub" => {
                // (time, time) or (time, duration).
                self.expect_args(fname, args, var_ranges, &["/time"]);
                if let Some(arg1) = args.get(1) {
                    let t = type_expr::find_or_create_name(self.ir, "/time");
                    let d = type_expr::find_or_create_name(self.ir, "/duration");
                    let expected = type_expr::new_union_or_single(self.ir, vec![t, d]);
                    self.expect_arg_type(fname, *arg1, var_ranges, expected, "/time or /duration");
                }
            }
            // Duration functions (strict argument types, matching the interpreter).
            "fn:duration:add" => {
                self.expect_args(fname, args, var_ranges, &["/duration", "/duration"]);
            }
            "fn:duration:mult" => {
                // (duration, number) or (number, duration).
                if let (Some(arg0), Some(arg1)) = (args.first(), args.get(1)) {
                    let d = type_expr::find_or_create_name(self.ir, "/duration");
                    let n = type_expr::find_or_create_name(self.ir, "/number");
                    let expected = type_expr::new_union_or_single(self.ir, vec![d, n]);
                    self.expect_arg_type(
                        fname,
                        *arg0,
                        var_ranges,
                        expected,
                        "/duration or /number",
                    );
                    self.expect_arg_type(
                        fname,
                        *arg1,
                        var_ranges,
                        expected,
                        "/duration or /number",
                    );
                }
            }
            "fn:duration:hours"
            | "fn:duration:minutes"
            | "fn:duration:seconds"
            | "fn:duration:nanos" => {
                self.expect_args(fname, args, var_ranges, &["/duration"]);
            }
            "fn:duration:from_nanos"
            | "fn:duration:from_hours"
            | "fn:duration:from_minutes"
            | "fn:duration:from_seconds" => {
                self.expect_args(fname, args, var_ranges, &["/number"]);
            }
            "fn:duration:parse" => {
                self.expect_args(fname, args, var_ranges, &["/string"]);
            }
            _ => {}
        }
    }

    fn bound_of_apply_fn(
        &mut self,
        fname: &str,
        args: &[InstId],
        var_ranges: &FxHashMap<NameId, InstId>,
    ) -> InstId {
        self.check_fn_arg_types(fname, args, var_ranges);
        match fname {
            "fn:list" => {
                if args.is_empty() {
                    let bot = type_expr::find_or_create_name(self.ir, "/bot");
                    return type_expr::new_list_type(self.ir, bot);
                }
                let ctx = TypeContext::default();
                let arg_types: Vec<InstId> = args
                    .iter()
                    .map(|a| self.bound_of_arg(*a, var_ranges))
                    .collect();
                let elem = type_expr::upper_bound(self.ir, &ctx, &arg_types);
                type_expr::new_list_type(self.ir, elem)
            }
            "fn:map" => {
                let ctx = TypeContext::default();
                let mut key_types = Vec::new();
                let mut val_types = Vec::new();
                let mut i = 0;
                while i + 1 < args.len() {
                    key_types.push(self.bound_of_arg(args[i], var_ranges));
                    val_types.push(self.bound_of_arg(args[i + 1], var_ranges));
                    i += 2;
                }
                let kt = type_expr::upper_bound(self.ir, &ctx, &key_types);
                let vt = type_expr::upper_bound(self.ir, &ctx, &val_types);
                type_expr::new_map_type(self.ir, kt, vt)
            }
            "fn:struct" => {
                let mut struct_args = Vec::new();
                let mut i = 0;
                while i + 1 < args.len() {
                    struct_args.push(args[i]); // field name
                    struct_args.push(self.bound_of_arg(args[i + 1], var_ranges));
                    i += 2;
                }
                type_expr::new_struct_type(self.ir, struct_args)
            }
            "fn:tuple" => {
                // fn:tuple acts as identity (one argument), a pair (two
                // arguments) or nested pairs (more); its bound is the tuple
                // of the argument bounds (matching mangle-go).
                let arg_types: Vec<InstId> = args
                    .iter()
                    .map(|a| self.bound_of_arg(*a, var_ranges))
                    .collect();
                type_expr::new_tuple_type(self.ir, arg_types)
            }
            "fn:pair" if args.len() == 2 => {
                let lt = self.bound_of_arg(args[0], var_ranges);
                let rt = self.bound_of_arg(args[1], var_ranges);
                type_expr::new_pair_type(self.ir, lt, rt)
            }
            "fn:pair:first" | "fn:pair:second" if args.len() == 1 => {
                let pair_type = self.bound_of_arg(args[0], var_ranges);
                match type_expr::pair_type_args(self.ir, pair_type) {
                    Some((lt, rt)) => {
                        if fname == "fn:pair:first" {
                            lt
                        } else {
                            rt
                        }
                    }
                    None => type_expr::find_or_create_name(self.ir, "/any"),
                }
            }
            "fn:struct:get" if args.len() == 2 => {
                let struct_type = self.bound_of_arg(args[0], var_ranges);
                if let Inst::Name(n) = self.ir.get(args[1]) {
                    let field = *n;
                    if let Some(ft) = type_expr::struct_type_field_deep(self.ir, struct_type, field)
                    {
                        return ft;
                    }
                }
                type_expr::find_or_create_name(self.ir, "/any")
            }
            "fn:plus" | "fn:minus" | "fn:mult" | "fn:div" => {
                type_expr::find_or_create_name(self.ir, "/number")
            }
            "fn:float:plus" | "fn:float:minus" | "fn:float:mult" | "fn:float:div" => {
                type_expr::find_or_create_name(self.ir, "/float64")
            }
            "fn:string:concat" | "fn:string:replace" => {
                type_expr::find_or_create_name(self.ir, "/string")
            }
            "fn:number:to_string" | "fn:float64:to_string" | "fn:name:to_string" => {
                type_expr::find_or_create_name(self.ir, "/string")
            }
            "fn:count" | "fn:sum" | "fn:max" | "fn:min" => {
                type_expr::find_or_create_name(self.ir, "/number")
            }
            "fn:float:sum" | "fn:float:max" | "fn:float:min" => {
                type_expr::find_or_create_name(self.ir, "/float64")
            }
            "fn:list:len" | "fn:len" | "fn:map:len" | "fn:struct:len" => {
                type_expr::find_or_create_name(self.ir, "/number")
            }
            "fn:sqrt" => type_expr::find_or_create_name(self.ir, "/float64"),
            "fn:list:get" if args.len() == 2 => {
                // Element type of the list argument, or /any if not a list.
                let list_type = self.bound_of_arg(args[0], var_ranges);
                match type_expr::apply_fn_args(self.ir, list_type) {
                    Some(inner)
                        if type_expr::apply_fn_name(self.ir, list_type)
                            == Some(type_expr::FN_LIST)
                            && inner.len() == 1 =>
                    {
                        inner[0]
                    }
                    _ => type_expr::find_or_create_name(self.ir, "/any"),
                }
            }
            "fn:list:append" if args.len() == 2 => {
                // Widen the list element type to include the appended value.
                let list_type = self.bound_of_arg(args[0], var_ranges);
                let new_elem = self.bound_of_arg(args[1], var_ranges);
                let old_elem = match type_expr::apply_fn_args(self.ir, list_type) {
                    Some(inner)
                        if type_expr::apply_fn_name(self.ir, list_type)
                            == Some(type_expr::FN_LIST)
                            && inner.len() == 1 =>
                    {
                        inner[0]
                    }
                    _ => type_expr::find_or_create_name(self.ir, "/any"),
                };
                let ctx = TypeContext::default();
                let elem = type_expr::upper_bound(self.ir, &ctx, &[old_elem, new_elem]);
                type_expr::new_list_type(self.ir, elem)
            }
            "fn:map:get" if args.len() == 2 => {
                // Value type of the map argument, or /any if not a map.
                let map_type = self.bound_of_arg(args[0], var_ranges);
                match type_expr::map_type_args(self.ir, map_type) {
                    Some((_, vt)) => vt,
                    None => type_expr::find_or_create_name(self.ir, "/any"),
                }
            }
            "fn:map:keys" if args.len() == 1 => {
                let map_type = self.bound_of_arg(args[0], var_ranges);
                let kt = match type_expr::map_type_args(self.ir, map_type) {
                    Some((kt, _)) => kt,
                    None => type_expr::find_or_create_name(self.ir, "/any"),
                };
                type_expr::new_list_type(self.ir, kt)
            }
            "fn:map:values" if args.len() == 1 => {
                let map_type = self.bound_of_arg(args[0], var_ranges);
                let vt = match type_expr::map_type_args(self.ir, map_type) {
                    Some((_, vt)) => vt,
                    None => type_expr::find_or_create_name(self.ir, "/any"),
                };
                type_expr::new_list_type(self.ir, vt)
            }
            "fn:struct:values" if args.len() == 1 => {
                // List of the upper bound of the struct's field types.
                let struct_type = self.bound_of_arg(args[0], var_ranges);
                let field_types: Vec<InstId> = type_expr::struct_type_fields(self.ir, struct_type)
                    .into_iter()
                    .map(|(_, ft, _)| ft)
                    .collect();
                let elem = if field_types.is_empty() {
                    type_expr::find_or_create_name(self.ir, "/any")
                } else {
                    let ctx = TypeContext::default();
                    type_expr::upper_bound(self.ir, &ctx, &field_types)
                };
                type_expr::new_list_type(self.ir, elem)
            }
            "fn:collect" | "fn:collect_distinct" => {
                if args.len() == 1 {
                    let elem_type = self.bound_of_arg(args[0], var_ranges);
                    type_expr::new_list_type(self.ir, elem_type)
                } else {
                    let any = type_expr::find_or_create_name(self.ir, "/any");
                    type_expr::new_list_type(self.ir, any)
                }
            }
            // --- Time functions (result types per mangle-go's builtin table) ---
            "fn:time:now"
            | "fn:time:add"
            | "fn:time:from_unix_nanos"
            | "fn:time:parse_rfc3339"
            | "fn:time:parse_civil"
            | "fn:time:trunc" => type_expr::find_or_create_name(self.ir, "/time"),
            "fn:time:sub" => {
                // (time, time) -> duration; (time, duration) -> time.
                // When the second argument's type is unknown, fall back to
                // /any (over-approximation) instead of guessing.
                if let Some(arg1) = args.get(1) {
                    let arg1_t = self.bound_of_arg(*arg1, var_ranges);
                    let duration_t = type_expr::find_or_create_name(self.ir, "/duration");
                    let ctx = TypeContext::default();
                    if type_expr::set_conforms(self.ir, &ctx, arg1_t, duration_t) {
                        return type_expr::find_or_create_name(self.ir, "/time");
                    }
                    if !type_expr::is_any(self.ir, arg1_t) {
                        return type_expr::find_or_create_name(self.ir, "/duration");
                    }
                }
                type_expr::find_or_create_name(self.ir, "/any")
            }
            "fn:time:year"
            | "fn:time:month"
            | "fn:time:day"
            | "fn:time:hour"
            | "fn:time:minute"
            | "fn:time:second"
            | "fn:time:to_unix_nanos" => type_expr::find_or_create_name(self.ir, "/number"),
            "fn:time:format" | "fn:time:format_civil" => {
                type_expr::find_or_create_name(self.ir, "/string")
            }
            // --- Duration functions ---
            "fn:duration:add"
            | "fn:duration:mult"
            | "fn:duration:from_nanos"
            | "fn:duration:from_hours"
            | "fn:duration:from_minutes"
            | "fn:duration:from_seconds"
            | "fn:duration:parse" => type_expr::find_or_create_name(self.ir, "/duration"),
            "fn:duration:hours" | "fn:duration:minutes" | "fn:duration:seconds" => {
                type_expr::find_or_create_name(self.ir, "/float64")
            }
            "fn:duration:nanos" => type_expr::find_or_create_name(self.ir, "/number"),
            _ => type_expr::find_or_create_name(self.ir, "/any"),
        }
    }

    // -- Helpers --

    fn atom_predicate(&self, atom_id: InstId) -> Option<NameId> {
        if let Inst::Atom { predicate, .. } = self.ir.get(atom_id) {
            Some(*predicate)
        } else {
            None
        }
    }

    fn atom_args(&self, atom_id: InstId) -> Vec<InstId> {
        if let Inst::Atom { args, .. } = self.ir.get(atom_id) {
            args.clone()
        } else {
            Vec::new()
        }
    }

    /// Simple textual description of an IR instruction for error messages.
    fn describe_inst(&self, id: InstId) -> String {
        match self.ir.get(id) {
            Inst::Name(n) => self.ir.resolve_name(*n).to_string(),
            Inst::Number(n) => n.to_string(),
            Inst::Float(f) => f.to_string(),
            Inst::String(s) => format!("{:?}", self.ir.resolve_string(*s)),
            Inst::Bool(b) => b.to_string(),
            Inst::Var(v) => self.ir.resolve_name(*v).to_string(),
            Inst::ApplyFn { function, args } => {
                let fname = self.ir.resolve_name(*function);
                let arg_strs: Vec<String> = args.iter().map(|a| self.describe_inst(*a)).collect();
                format!("{}({})", fname, arg_strs.join(", "))
            }
            _ => format!("inst#{}", id.index()),
        }
    }
}

// ---------------------------------------------------------------------------
// InferState
// ---------------------------------------------------------------------------

/// State of type inference while iterating over premises.
///
/// Tracks variable bindings with their inferred types.
struct InferState {
    /// Variable names (parallel with `var_types`).
    used_vars: Vec<NameId>,
    /// Type bounds for each variable.
    var_types: Vec<InstId>,
}

impl InferState {
    fn new() -> Self {
        Self {
            used_vars: Vec::new(),
            var_types: Vec::new(),
        }
    }

    /// Adds a new variable binding or refines an existing one via LowerBound.
    fn add_or_refine_with_ir(&mut self, ir: &mut Ir, var: NameId, tpe: InstId) {
        if let Some(idx) = self.used_vars.iter().position(|v| *v == var) {
            // Variable already bound: intersect existing type with new type.
            let existing = self.var_types[idx];
            let ctx = TypeContext::default();
            let meet = type_expr::lower_bound(ir, &ctx, &[existing, tpe]);
            if !type_expr::is_empty_type(ir, meet) {
                self.var_types[idx] = meet;
            }
            // If intersection is empty, keep the existing type (conservative).
        } else {
            self.used_vars.push(var);
            self.var_types.push(tpe);
        }
    }

    /// Sets a variable's type directly (for negative refinement).
    fn set_var(&mut self, var: NameId, tpe: InstId) {
        if let Some(idx) = self.used_vars.iter().position(|v| *v == var) {
            self.var_types[idx] = tpe;
        }
    }

    /// Converts the state to a HashMap for lookups.
    fn as_map(&self) -> FxHashMap<NameId, InstId> {
        self.used_vars
            .iter()
            .zip(self.var_types.iter())
            .map(|(v, t)| (*v, *t))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LoweringContext;
    use mangle_ast as ast;
    use mangle_parse::Parser;

    /// Helper: parse source, lower, run bounds checker.
    fn check(source: &str) -> Result<()> {
        let arena = ast::Arena::new_with_global_interner();
        let mut parser = Parser::new(&arena, source.as_bytes(), "test");
        parser.next_token().unwrap();
        let unit = parser.parse_unit().unwrap();
        let ctx = LoweringContext::new(&arena);
        let mut ir = ctx.lower_unit(&unit);
        let mut checker = BoundsChecker::new(&mut ir);
        checker.check()
    }

    // -----------------------------------------------------------------------
    // Basic facts and rules (existing tests, now parser-based)
    // -----------------------------------------------------------------------

    #[test]
    fn check_valid_fact() {
        let arena = ast::Arena::new_with_global_interner();

        // Decl foo(X) bound [/number].
        let foo_sym = arena.predicate_sym("foo", Some(1));
        let var_x = arena.variable("X");
        let atom_foo_x = arena.atom(foo_sym, &[var_x]);
        let num_type = arena.const_(arena.name("/number"));
        let bound_decl = ast::BoundDecl {
            base_terms: arena.alloc_slice_copy(&[num_type]),
        };
        let decl = ast::Decl {
            atom: atom_foo_x,
            descr: &[],
            bounds: Some(arena.alloc_slice_copy(&[arena.alloc(bound_decl)])),
            constraints: None,
            is_temporal: false,
        };

        // foo(42).
        let const_42 = arena.const_(ast::Const::Number(42));
        let atom_foo_42 = arena.atom(foo_sym, &[const_42]);
        let clause = ast::Clause {
            head: atom_foo_42,
            head_time: None,
            premises: &[],
            transform: &[],
        };

        let unit = ast::Unit {
            decls: arena.alloc_slice_copy(&[&decl]),
            clauses: arena.alloc_slice_copy(&[&clause]),
        };

        let ctx = LoweringContext::new(&arena);
        let mut ir = ctx.lower_unit(&unit);
        let mut checker = BoundsChecker::new(&mut ir);
        assert!(checker.check().is_ok());
    }

    #[test]
    fn check_invalid_fact_type_mismatch() {
        let arena = ast::Arena::new_with_global_interner();

        // Decl foo(X) bound [/number].
        let foo_sym = arena.predicate_sym("foo", Some(1));
        let var_x = arena.variable("X");
        let atom_foo_x = arena.atom(foo_sym, &[var_x]);
        let num_type = arena.const_(arena.name("/number"));
        let bound_decl = ast::BoundDecl {
            base_terms: arena.alloc_slice_copy(&[num_type]),
        };
        let decl = ast::Decl {
            atom: atom_foo_x,
            descr: &[],
            bounds: Some(arena.alloc_slice_copy(&[arena.alloc(bound_decl)])),
            constraints: None,
            is_temporal: false,
        };

        // foo("hello"). -> Type mismatch.
        let const_str = arena.const_(ast::Const::String("hello"));
        let atom_foo_bad = arena.atom(foo_sym, &[const_str]);
        let clause = ast::Clause {
            head: atom_foo_bad,
            head_time: None,
            premises: &[],
            transform: &[],
        };

        let unit = ast::Unit {
            decls: arena.alloc_slice_copy(&[&decl]),
            clauses: arena.alloc_slice_copy(&[&clause]),
        };

        let ctx = LoweringContext::new(&arena);
        let mut ir = ctx.lower_unit(&unit);
        let mut checker = BoundsChecker::new(&mut ir);
        let result = checker.check();
        assert!(result.is_err(), "expected type mismatch error");
    }

    #[test]
    fn check_valid_rule() {
        let arena = ast::Arena::new_with_global_interner();

        // Decl src(X) bound [/number].
        let src_sym = arena.predicate_sym("src", Some(1));
        let var_x = arena.variable("X");
        let atom_src_x = arena.atom(src_sym, &[var_x]);
        let num_type = arena.const_(arena.name("/number"));
        let bound_decl = ast::BoundDecl {
            base_terms: arena.alloc_slice_copy(&[num_type]),
        };
        let decl_src = ast::Decl {
            atom: atom_src_x,
            descr: &[],
            bounds: Some(arena.alloc_slice_copy(&[arena.alloc(bound_decl)])),
            constraints: None,
            is_temporal: false,
        };

        // Decl dst(X) bound [/number].
        let dst_sym = arena.predicate_sym("dst", Some(1));
        let var_y = arena.variable("Y");
        let atom_dst_y = arena.atom(dst_sym, &[var_y]);
        let num_type2 = arena.const_(arena.name("/number"));
        let bound_decl2 = ast::BoundDecl {
            base_terms: arena.alloc_slice_copy(&[num_type2]),
        };
        let decl_dst = ast::Decl {
            atom: atom_dst_y,
            descr: &[],
            bounds: Some(arena.alloc_slice_copy(&[arena.alloc(bound_decl2)])),
            constraints: None,
            is_temporal: false,
        };

        // dst(X) :- src(X).
        let var_x2 = arena.variable("X");
        let head = arena.atom(dst_sym, &[var_x2]);
        let var_x3 = arena.variable("X");
        let body = arena.atom(src_sym, &[var_x3]);
        let clause = ast::Clause {
            head,
            head_time: None,
            premises: arena.alloc_slice_copy(&[arena.alloc(ast::Term::Atom(body))]),
            transform: &[],
        };

        let unit = ast::Unit {
            decls: arena.alloc_slice_copy(&[&decl_src, &decl_dst]),
            clauses: arena.alloc_slice_copy(&[&clause]),
        };

        let ctx = LoweringContext::new(&arena);
        let mut ir = ctx.lower_unit(&unit);
        let mut checker = BoundsChecker::new(&mut ir);
        assert!(checker.check().is_ok());
    }

    #[test]
    fn check_arity_mismatch() {
        let arena = ast::Arena::new_with_global_interner();

        // Decl foo(X) bound [/number].
        let foo_sym = arena.predicate_sym("foo", Some(1));
        let var_x = arena.variable("X");
        let atom_foo_x = arena.atom(foo_sym, &[var_x]);
        let num_type = arena.const_(arena.name("/number"));
        let bound_decl = ast::BoundDecl {
            base_terms: arena.alloc_slice_copy(&[num_type]),
        };
        let decl = ast::Decl {
            atom: atom_foo_x,
            descr: &[],
            bounds: Some(arena.alloc_slice_copy(&[arena.alloc(bound_decl)])),
            constraints: None,
            is_temporal: false,
        };

        // foo(42, 43). -> Arity mismatch.
        let const_42 = arena.const_(ast::Const::Number(42));
        let const_43 = arena.const_(ast::Const::Number(43));
        let atom_foo_bad = arena.atom(foo_sym, &[const_42, const_43]);
        let clause = ast::Clause {
            head: atom_foo_bad,
            head_time: None,
            premises: &[],
            transform: &[],
        };

        let unit = ast::Unit {
            decls: arena.alloc_slice_copy(&[&decl]),
            clauses: arena.alloc_slice_copy(&[&clause]),
        };

        let ctx = LoweringContext::new(&arena);
        let mut ir = ctx.lower_unit(&unit);
        let mut checker = BoundsChecker::new(&mut ir);
        let result = checker.check();
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Parser-based tests: multiple bound alternatives
    // -----------------------------------------------------------------------

    #[test]
    fn multiple_alternatives_first_matches() {
        // pair(42, 99) matches first alternative [/number, /number].
        assert!(
            check(
                r#"
            Decl pair(X, Y) bound [/number, /number] bound [/string, /string].
            pair(42, 99).
        "#
            )
            .is_ok()
        );
    }

    #[test]
    fn multiple_alternatives_second_matches() {
        // pair("a", "b") matches second alternative [/string, /string].
        assert!(
            check(
                r#"
            Decl pair(X, Y) bound [/number, /number] bound [/string, /string].
            pair("a", "b").
        "#
            )
            .is_ok()
        );
    }

    #[test]
    fn multiple_alternatives_none_matches() {
        // pair(42, "b") matches neither alternative.
        assert!(
            check(
                r#"
            Decl pair(X, Y) bound [/number, /number] bound [/string, /string].
            pair(42, "b").
        "#
            )
            .is_err()
        );
    }

    // -----------------------------------------------------------------------
    // Rule type inference: variable binding from premises
    // -----------------------------------------------------------------------

    #[test]
    fn rule_infers_type_from_premise() {
        // X gets type /number from src, which conforms to dst's bound.
        assert!(
            check(
                r#"
            Decl src(X) bound [/number].
            Decl dst(X) bound [/number].
            dst(X) :- src(X).
        "#
            )
            .is_ok()
        );
    }

    #[test]
    fn rule_type_mismatch_from_premise() {
        // X inferred as /string from src, but dst expects /number.
        assert!(
            check(
                r#"
            Decl src(X) bound [/string].
            Decl dst(X) bound [/number].
            dst(X) :- src(X).
        "#
            )
            .is_err()
        );
    }

    // -----------------------------------------------------------------------
    // Multiple body atoms refining the same variable (LowerBound)
    // -----------------------------------------------------------------------

    #[test]
    fn two_premises_refine_variable() {
        // X starts as fn:Union(/number, /string) from 'wide',
        // then refined to /number from 'narrow'. Should conform to /number.
        assert!(
            check(
                r#"
            Decl wide(X) bound [fn:Union(/number, /string)].
            Decl narrow(X) bound [/number].
            Decl result(X) bound [/number].
            result(X) :- wide(X), narrow(X).
        "#
            )
            .is_ok()
        );
    }

    #[test]
    fn two_premises_refine_to_incompatible() {
        // X inferred as /string from src1, then /number from src2.
        // Intersection is empty, so X keeps /string (conservative).
        // /string does not conform to /number → error.
        assert!(
            check(
                r#"
            Decl src1(X) bound [/string].
            Decl src2(X) bound [/number].
            Decl dst(X) bound [/number].
            dst(X) :- src1(X), src2(X).
        "#
            )
            .is_err()
        );
    }

    // -----------------------------------------------------------------------
    // Polymorphic type declarations (skolemization)
    // -----------------------------------------------------------------------

    #[test]
    fn polymorphic_identity_number() {
        // T is a type variable. pair(42, 99) should pass: T can be /number.
        assert!(
            check(
                r#"
            Decl pair(X, Y) bound [T, T].
            pair(42, 99).
        "#
            )
            .is_ok()
        );
    }

    #[test]
    fn polymorphic_identity_string() {
        // T is a type variable. pair("a", "b") should pass: T can be /string.
        assert!(
            check(
                r#"
            Decl pair(X, Y) bound [T, T].
            pair("a", "b").
        "#
            )
            .is_ok()
        );
    }

    #[test]
    fn polymorphic_rule_with_inferred_type() {
        // T skolemized to fresh var. X inferred as /number from src.
        // /number conforms to ?X0 (mapped to /any in context) → passes.
        assert!(
            check(
                r#"
            Decl src(X) bound [/number].
            Decl dst(X) bound [T].
            dst(X) :- src(X).
        "#
            )
            .is_ok()
        );
    }

    // -----------------------------------------------------------------------
    // Cross-predicate inference
    // -----------------------------------------------------------------------

    #[test]
    fn cross_predicate_inference_basic() {
        // 'helper' has no declaration. Its type is inferred from its rule
        // (which uses 'src' with bound [/number]). Then 'dst' uses 'helper'.
        assert!(
            check(
                r#"
            Decl src(X) bound [/number].
            Decl dst(X) bound [/number].
            helper(X) :- src(X).
            dst(X) :- helper(X).
        "#
            )
            .is_ok()
        );
    }

    #[test]
    fn cross_predicate_inference_type_mismatch() {
        // 'helper' inferred as /string from src. dst expects /number → error.
        assert!(
            check(
                r#"
            Decl src(X) bound [/string].
            Decl dst(X) bound [/number].
            helper(X) :- src(X).
            dst(X) :- helper(X).
        "#
            )
            .is_err()
        );
    }

    #[test]
    fn cross_predicate_inference_chain() {
        // Chain: src → mid → dst, only src and dst declared.
        assert!(
            check(
                r#"
            Decl src(X) bound [/number].
            Decl dst(X) bound [/number].
            mid(X) :- src(X).
            dst(X) :- mid(X).
        "#
            )
            .is_ok()
        );
    }

    // -----------------------------------------------------------------------
    // Equality and inequality premises
    // -----------------------------------------------------------------------

    #[test]
    fn equality_binds_variable() {
        // X = "hello" gives X type /string.
        assert!(
            check(
                r#"
            Decl src(X) bound [/string].
            Decl dst(X) bound [/string].
            dst(X) :- src(X), X = "hello".
        "#
            )
            .is_ok()
        );
    }

    #[test]
    fn inequality_refines_variable() {
        // X from src is /string, X != "bad" should still be /string.
        assert!(
            check(
                r#"
            Decl src(X) bound [/string].
            Decl dst(X) bound [/string].
            dst(X) :- src(X), X != "bad".
        "#
            )
            .is_ok()
        );
    }

    // -----------------------------------------------------------------------
    // Transform (let) expressions
    // -----------------------------------------------------------------------

    #[test]
    fn transform_arithmetic() {
        // let Y = fn:plus(X, 1) → Y inferred as /number.
        assert!(
            check(
                r#"
            Decl src(X) bound [/number].
            Decl dst(X, Y) bound [/number, /number].
            dst(X, Y) :- src(X) |> let Y = fn:plus(X, 1).
        "#
            )
            .is_ok()
        );
    }

    #[test]
    fn transform_string_concat() {
        // let Y = fn:string:concat(X, "!") → Y inferred as /string.
        assert!(
            check(
                r#"
            Decl src(X) bound [/string].
            Decl dst(X, Y) bound [/string, /string].
            dst(X, Y) :- src(X) |> let Y = fn:string:concat(X, "!").
        "#
            )
            .is_ok()
        );
    }

    #[test]
    fn transform_type_mismatch() {
        // Y = fn:plus(X, 1) → /number, but dst expects /string for Y.
        assert!(
            check(
                r#"
            Decl src(X) bound [/number].
            Decl dst(X, Y) bound [/number, /string].
            dst(X, Y) :- src(X) |> let Y = fn:plus(X, 1).
        "#
            )
            .is_err()
        );
    }

    // -----------------------------------------------------------------------
    // No-declaration predicates (no bounds checking needed)
    // -----------------------------------------------------------------------

    #[test]
    fn undeclared_predicate_passes() {
        // Rules with no declarations should pass without error.
        assert!(
            check(
                r#"
            foo(1).
            bar(X) :- foo(X).
        "#
            )
            .is_ok()
        );
    }

    // -----------------------------------------------------------------------
    // Arity consistency checking (no Decl required)
    // -----------------------------------------------------------------------

    #[test]
    fn arity_mismatch_facts() {
        // Same predicate with different arity: p(1) vs p(2, 3).
        let result = check(
            r#"
            p(1).
            p(2, 3).
        "#,
        );
        assert!(result.is_err(), "expected arity error, got: {:?}", result);
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("inconsistent arity"),
            "error should mention 'inconsistent arity': {}",
            msg
        );
        assert!(
            msg.contains("p"),
            "error should mention predicate name: {}",
            msg
        );
    }

    #[test]
    fn arity_mismatch_fact_and_rule() {
        // Fact p(1) vs rule head p(X, Y) — different arity.
        let result = check(
            r#"
            p(1).
            p(X, Y) :- q(X, Y).
            q(1, 2).
        "#,
        );
        assert!(result.is_err(), "expected arity error, got: {:?}", result);
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("inconsistent arity"),
            "error should mention 'inconsistent arity': {}",
            msg
        );
    }

    #[test]
    fn arity_mismatch_two_rules() {
        // Two rules with different head arity for same predicate.
        let result = check(
            r#"
            p(X) :- q(X).
            p(X, Y) :- r(X, Y).
            q(1).
            r(1, 2).
        "#,
        );
        assert!(result.is_err(), "expected arity error, got: {:?}", result);
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("inconsistent arity"),
            "error should mention 'inconsistent arity': {}",
            msg
        );
    }

    #[test]
    fn consistent_arity_passes() {
        // All uses of p have arity 1 — should pass.
        assert!(
            check(
                r#"
            p(1).
            p(2).
            q(X) :- p(X).
        "#
            )
            .is_ok()
        );
    }

    #[test]
    fn consistent_arity_rules_passes() {
        // All uses of p have arity 2 — should pass.
        assert!(
            check(
                r#"
            p(1, 2).
            p(X, Y) :- q(X), r(Y).
            q(1).
            r(2).
        "#
            )
            .is_ok()
        );
    }

    #[test]
    fn arity_mismatch_undeclared_predicates() {
        // Even without any Decl, arity mismatch should be caught.
        let result = check(
            r#"
            edge(1, 2).
            edge(3, 4, 5).
        "#,
        );
        assert!(result.is_err(), "expected arity error, got: {:?}", result);
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("inconsistent arity"),
            "error should mention 'inconsistent arity': {}",
            msg
        );
    }

    // -----------------------------------------------------------------------
    // Binding (safety) analysis
    // -----------------------------------------------------------------------

    #[test]
    fn binding_unbound_head_var() {
        let result = check("p(1). q(X, Y) :- p(X).");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("Y"), "{msg}");
        assert!(msg.contains("not bound"), "{msg}");
    }

    #[test]
    fn binding_var_only_in_negation() {
        // Negation never binds; Y must be bound elsewhere.
        let result = check("p(1). q(X) :- p(X), !r(Y).");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("Y"), "{msg}");
    }

    #[test]
    fn binding_eq_chain_converges() {
        // X = Y, Y = Z, Z bound by p: all three are bound.
        assert!(check("p(1). q(X) :- p(Z), X = Y, Y = Z.").is_ok());
    }

    #[test]
    fn binding_eq_expr_with_unbound_var() {
        let result = check("p(1). q(X) :- X = fn:plus(Y, 1).");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("Y"), "{msg}");
    }

    #[test]
    fn binding_filter_pred_needs_bound_args() {
        // :lt is a filter: Y must be bound before use (evaluation is
        // left-to-right).
        let result = check("p(1). q(X) :- :lt(Y, 5), p(X), X = Y.");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("move the subgoal to the right"), "{msg}");
    }

    #[test]
    fn binding_list_member_output_mode() {
        // :list:member(E, L) binds E from L.
        assert!(check("p([1, 2]). q(E) :- p(L), :list:member(E, L).").is_ok());
    }

    #[test]
    fn binding_match_field_output_mode() {
        assert!(check(r#"p({/a: 1}). q(V) :- p(S), :match_field(S, /a, V)."#).is_ok());
    }

    #[test]
    fn binding_group_by_key_must_be_bound() {
        let result = check("p(1). q(X, L) :- p(X) |> do fn:group_by(K); let L = fn:collect(X).");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("group_by key K"), "{msg}");
    }

    #[test]
    fn binding_head_var_neither_grouped_nor_aggregated() {
        let result =
            check("p(1, 2). q(K, V) :- p(K, V) |> do fn:group_by(K); let C = fn:count(V).");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("neither part of group_by nor aggregated"),
            "{msg}"
        );
    }

    #[test]
    fn binding_group_by_ok() {
        assert!(
            check("p(1, 2). q(K, S) :- p(K, V) |> do fn:group_by(K); let S = fn:sum(V).").is_ok()
        );
        // Multiple keys and multiple aggregations.
        assert!(check(
            "p(1, 2, 3). q(K, T, M, N) :- p(K, T, V) |> do fn:group_by(K, T); let M = fn:max(V); let N = fn:min(V)."
        )
        .is_ok());
    }

    #[test]
    fn binding_transform_redefines_body_var() {
        let result = check("p(1, 2). q(Y) :- p(X, Y) |> let Y = fn:plus(X, 1).");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("redefines variable Y"), "{msg}");
    }

    #[test]
    fn binding_non_reducer_let_must_use_group_keys() {
        // fn:plus is not a reducer: V is not a group key.
        let result =
            check("p(1, 2). q(K, Z) :- p(K, V) |> do fn:group_by(K); let Z = fn:plus(V, 1).");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("either part of group_by or defined in the transform"),
            "{msg}"
        );
    }

    #[test]
    fn binding_let_after_group_by_can_chain() {
        // Non-reducer lets may use group keys and earlier transform defs.
        assert!(check(
            "p(1, 2). q(K, Z) :- p(K, V) |> do fn:group_by(K); let S = fn:sum(V); let Z = fn:plus(S, 1)."
        )
        .is_ok());
    }

    #[test]
    fn binding_only_group_by_supported_in_do() {
        let result =
            check("p(1, 2). q(K, L) :- p(K, V) |> do fn:sort_by(K); let L = fn:collect(V).");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("only fn:group_by is supported"), "{msg}");
    }

    #[test]
    fn binding_wildcards_exempt() {
        assert!(check("p(1, 2). q(X) :- p(X, _).").is_ok());
    }

    #[test]
    fn binding_fact_with_variable() {
        // Facts are unit clauses: they have no body to bind variables.
        let result = check("p(1). q(X).");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("fact"), "{msg}");
        assert!(msg.contains("X"), "{msg}");
        assert!(msg.contains("ground"), "{msg}");
    }

    #[test]
    fn binding_fact_with_wildcard() {
        // A wildcard in a fact is an anonymous variable — equally unbound
        // (mangle-go rejects `foo(_).` too).
        let result = check("q(_).");
        assert!(result.is_err(), "expected error, got: {:?}", result);
    }

    #[test]
    fn binding_fact_with_nested_variable() {
        // Variables nested inside compound fact args are also unbound.
        let result = check("q([1, X]).");
        assert!(result.is_err(), "expected error, got: {:?}", result);
    }

    // -----------------------------------------------------------------------
    // Function bound inference (bound_of_apply_fn)
    // -----------------------------------------------------------------------

    #[test]
    fn bound_of_float_uses_colon_names() {
        // fn:float:plus must infer /float64 (the old list used
        // underscore names that never matched).
        let result = check(
            r#"
            Decl foo(X, Y) bound [/float64, /float64].
            p(1.5).
            foo(A, B) :- p(X), A = fn:float:plus(X, 1.0), B = fn:float:minus(X, 0.5).
        "#,
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn bound_of_float_aggregates() {
        let result = check(
            r#"
            Decl p(K, V) bound [/number, /float64].
            Decl foo(K, S) bound [/number, /float64].
            p(1, 2.5).
            foo(K, S) :- p(K, V) |> do fn:group_by(K); let S = fn:float:sum(V).
        "#,
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn bound_of_to_string_functions() {
        let result = check(
            r#"
            Decl foo(S) bound [/string].
            p(1).
            foo(S) :- p(X), S = fn:number:to_string(X).
        "#,
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn bound_of_time_and_duration_functions() {
        let result = check(
            r#"
            Decl foo(T, D, N, F) bound [/time, /duration, /number, /float64].
            p(1000).
            foo(T, D, N, F) :- p(X), T = fn:time:from_unix_nanos(X), D = fn:duration:from_seconds(1), N = fn:duration:nanos(D), F = fn:duration:seconds(D).
        "#,
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn bound_of_pair_functions() {
        let result = check(
            r#"
            Decl p(X) bound [/number].
            Decl foo(A, B) bound [/number, /string].
            p(1).
            foo(A, B) :- p(X), P = fn:pair(X, "x"), A = fn:pair:first(P), B = fn:name:to_string(/q).
        "#,
        );
        assert!(result.is_ok(), "{result:?}");
    }

    // -----------------------------------------------------------------------
    // Function argument-type checking (mangle-go exprtyping_test.go parity)
    // -----------------------------------------------------------------------

    #[test]
    fn fn_arg_plus_on_string() {
        // fn:plus applied to a /string variable.
        let result = check(
            r#"
            Decl src(X) bound [/string].
            Decl result(Y) bound [/number].
            result(Y) :- src(X) |> let Y = fn:plus(X, 1).
        "#,
        );
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("fn:plus"), "{msg}");
        assert!(msg.contains("expected /number"), "{msg}");
    }

    #[test]
    fn fn_arg_minus_on_string() {
        let result = check(
            r#"
            Decl src(X) bound [/string].
            Decl result(Y) bound [/number].
            result(Y) :- src(X) |> let Y = fn:minus(X, 1).
        "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn fn_arg_mult_on_name() {
        let result = check(
            r#"
            Decl src(X) bound [/name].
            Decl result(Y) bound [/number].
            result(Y) :- src(X) |> let Y = fn:mult(X, 2).
        "#,
        );
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("fn:mult"), "{msg}");
    }

    #[test]
    fn fn_arg_sum_on_strings() {
        // fn:sum over /string values in a group_by transform.
        let result = check(
            r#"
            Decl src(X, Z) bound [/string, /string].
            Decl result(X, Y) bound [/string, /number].
            result(X, Y) :- src(X, Z) |> do fn:group_by(X); let Y = fn:sum(Z).
        "#,
        );
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("fn:sum"), "{msg}");
    }

    #[test]
    fn fn_arg_max_on_strings() {
        let result = check(
            r#"
            Decl src(X, Z) bound [/string, /string].
            Decl result(X, Y) bound [/string, /number].
            result(X, Y) :- src(X, Z) |> do fn:group_by(X); let Y = fn:max(Z).
        "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn fn_arg_plus_on_string_in_body() {
        // mangle-go's neg_plusarg.mg: no declarations at all — the error
        // must surface even for undeclared predicates.
        let result = check("p(X) :- Y = \"A\", X = fn:plus(1, Y).");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("fn:plus"), "{msg}");
    }

    #[test]
    fn fn_arg_float_plus_accepts_number() {
        // Float functions coerce /number to f64: both forms are valid.
        let result = check(
            r#"
            Decl src(X) bound [/number].
            Decl result(Y) bound [/float64].
            result(Y) :- src(X) |> let Y = fn:float:plus(X, 1.5).
        "#,
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn fn_arg_float_plus_on_string() {
        let result = check(
            r#"
            Decl src(X) bound [/string].
            Decl result(Y) bound [/float64].
            result(Y) :- src(X) |> let Y = fn:float:plus(X, 1.5).
        "#,
        );
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("fn:float:plus"), "{msg}");
    }

    #[test]
    fn fn_arg_sqrt_in_body() {
        let result = check(
            r#"
            Decl src(X) bound [/number].
            Decl result(Y) bound [/float64].
            result(Y) :- src(X), Y = fn:sqrt(X).
        "#,
        );
        assert!(result.is_ok(), "{result:?}");

        let result = check(
            r#"
            Decl src(X) bound [/name].
            Decl result(Y) bound [/float64].
            result(Y) :- src(X), Y = fn:sqrt(X).
        "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn fn_arg_union_bound_is_not_an_error() {
        // A variable that may be /number or /string is fine for fn:plus —
        // only provably-disjoint types are rejected.
        let result = check(
            r#"
            Decl src(X) bound [.Union</number, /string>].
            Decl result(Y) bound [/number].
            result(Y) :- src(X) |> let Y = fn:plus(X, 1).
        "#,
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn fn_arg_time_functions_strict() {
        // fn:time:year expects /time, not /number.
        let result = check(
            r#"
            Decl src(X) bound [/number].
            Decl result(Y) bound [/number].
            result(Y) :- src(X), Y = fn:time:year(X).
        "#,
        );
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("fn:time:year"), "{msg}");
    }

    #[test]
    fn fn_arg_duration_from_seconds_strict() {
        // The interpreter requires a /number here (mangle-go takes float64;
        // we follow our runtime).
        let result = check(
            r#"
            Decl result(D) bound [/duration].
            result(D) :- D = fn:duration:from_seconds(1.0).
        "#,
        );
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("fn:duration:from_seconds"), "{msg}");
    }

    #[test]
    fn fn_arg_time_sub_accepts_both_forms() {
        // (time, time) and (time, duration) are both valid for fn:time:sub.
        let ok1 = check(
            r#"
            Decl src(T) bound [/time].
            Decl result(D) bound [/duration].
            result(D) :- src(T), U = fn:time:from_unix_nanos(0), D = fn:time:sub(T, U).
        "#,
        );
        assert!(ok1.is_ok(), "{ok1:?}");
        let ok2 = check(
            r#"
            Decl src(T) bound [/time].
            Decl result(T2) bound [/time].
            result(T2) :- src(T), D = fn:duration:from_seconds(1), T2 = fn:time:sub(T, D).
        "#,
        );
        assert!(ok2.is_ok(), "{ok2:?}");
    }

    // -----------------------------------------------------------------------
    // Function arity checking (mangle-go checkExprArity parity)
    // -----------------------------------------------------------------------

    #[test]
    fn fn_arity_list_get() {
        // fn:list:get takes exactly 2 arguments.
        let result = check("p([1]). q(X) :- X = fn:list:get([1]).");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("fn:list:get"), "{msg}");
        assert!(msg.contains("expects 2"), "{msg}");
    }

    #[test]
    fn fn_arity_list_get_in_transform() {
        let result = check("p([1]). q(Y) :- p(X) |> let Y = fn:list:get(X).");
        assert!(result.is_err());
    }

    #[test]
    fn fn_arity_nested() {
        // Arity errors inside nested applications are caught.
        let result = check("q(X) :- X = fn:list(fn:list:get(1)).");
        assert!(result.is_err());
    }

    #[test]
    fn fn_arity_collect_no_args() {
        let result = check("p(1). q(X) :- p(Y) |> do fn:group_by(); let X = fn:collect().");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("at least one argument"), "{msg}");
    }

    #[test]
    fn fn_arity_collect_distinct_no_args() {
        let result =
            check("p(1). q(X) :- p(Y) |> do fn:group_by(); let X = fn:collect_distinct().");
        assert!(result.is_err());
    }

    #[test]
    fn fn_arity_struct_odd() {
        let result = check("q(X) :- X = fn:struct(1).");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("even number of arguments"), "{msg}");
    }

    #[test]
    fn fn_arity_map_odd() {
        let result = check("q(X) :- X = fn:map(1, 2, 3).");
        assert!(result.is_err());
    }

    #[test]
    fn fn_arity_unknown_function() {
        // Functions the runtime does not implement are rejected up front
        // (the interpreter would fail with 'Unknown function' at runtime).
        let result =
            check("p(1). q(Y) :- p(X) |> let Y = fn:plus(X, X), let _ = fn:ring_the_alarm().");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("unknown function"), "{msg}");
        assert!(msg.contains("fn:ring_the_alarm"), "{msg}");
    }

    #[test]
    fn fn_arity_mangle_go_only_functions() {
        // Present in mangle-go but not implemented by this runtime.
        let result = check("q(X) :- X = fn:list:cons(fn:list:get([1], 1), []).");
        assert!(result.is_err());
        let result = check("q(X) :- X = fn:mod(5, 2).");
        assert!(result.is_err());
    }

    #[test]
    fn fn_arity_valid_varargs() {
        // Folds and constructors accept any (even) number of arguments.
        assert!(check("q(X) :- X = fn:plus(1, 2, 3).").is_ok());
        assert!(check("q(X) :- X = fn:string:concat(\"a\", \"b\", \"c\").").is_ok());
        assert!(check("q(X) :- X = fn:struct(/a, 1, /b, 2).").is_ok());
        assert!(check("q(X) :- X = fn:map(1, \"a\", 2, \"b\").").is_ok());
        // Wildcards inside valid applications are fine.
        assert!(check("q(X) :- X = fn:pair(1, fn:string:concat()).").is_ok());
    }

    #[test]
    fn fn_arity_time_now() {
        assert!(check("q(X) :- X = fn:time:now().").is_ok());
        let result = check("q(X) :- X = fn:time:now(1).");
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Empty-meet refinement errors
    // -----------------------------------------------------------------------

    #[test]
    fn eq_refines_to_disjoint_type() {
        // mangle-go TestBoundsAnalyzerNegative: X is /number from bar, then
        // unified with a string — the rule can never derive anything.
        let result = check(
            r#"
            Decl foo(X) bound [/number].
            Decl bar(X) bound [/number].
            foo(X) :- bar(X), X = "hello".
        "#,
        );
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("equality"), "{msg}");
        assert!(msg.contains("X"), "{msg}");
    }

    #[test]
    fn eq_with_compatible_type_still_ok() {
        let result = check(
            r#"
            Decl foo(X) bound [/number].
            Decl bar(X) bound [/number].
            foo(X) :- bar(X), X = 3.
        "#,
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn match_prefix_incompatible_prefix() {
        // mangle-go TestBoundsAnalyzerNegative: X is /bar, prefix /foo.
        let result = check(
            r#"
            Decl foo(X) bound [/bar].
            Decl bar(X) bound [/bar].
            foo(X) :- bar(X), :match_prefix(X, /foo).
        "#,
        );
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains(":match_prefix"), "{msg}");
        assert!(msg.contains("incompatible"), "{msg}");
    }

    #[test]
    fn match_prefix_compatible_prefix() {
        // mangle-go TestBoundsAnalyzer: X is /foo, prefix /foo.
        let result = check(
            r#"
            Decl foo(X) bound [/name].
            Decl bar(X) bound [/foo].
            foo(X) :- bar(X), :match_prefix(X, /foo).
        "#,
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn transform_let_conflicting_types() {
        // A transform let refining a variable to a disjoint type is an error.
        let result = check(
            r#"
            Decl src(X) bound [/number].
            Decl result(X, Y) bound [/number, /string].
            result(X, Y) :- src(X) |> let Y = fn:plus(X, 1), let Z = fn:string:concat(Y, "!").
        "#,
        );
        assert!(result.is_err());
        // The compatible form passes.
        let ok = check(
            r#"
            Decl src(X) bound [/number].
            Decl result(X, Y) bound [/number, /string].
            result(X, Y) :- src(X) |> let Y = fn:number:to_string(X), let Z = fn:string:concat(Y, "!").
        "#,
        );
        assert!(ok.is_ok(), "{ok:?}");
    }

    #[test]
    fn list_member_disjoint_element() {
        // Element var is /number, list is .List</string>.
        let result = check(
            r#"
            Decl foo(X) bound [/number].
            Decl bar(X) bound [.List</string>].
            foo(X) :- bar(X), :list:member(X, X).
        "#,
        );
        assert!(result.is_err());
    }
}
