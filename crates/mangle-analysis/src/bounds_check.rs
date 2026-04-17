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
use fxhash::{FxHashMap, FxHashSet};
use mangle_ir::{Inst, InstId, Ir, NameId};

use crate::name_trie::NameTrie;
use crate::type_expr::{self, TypeContext};

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
        }
    }

    /// Main entry point: collect declarations, build rules map, check all clauses.
    pub fn check(&mut self) -> Result<()> {
        self.collect_declarations()?;
        self.build_rules_map();
        self.check_all_clauses()
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
                if !premises.is_empty() || !transform.is_empty() {
                    if let Some(pred) = self.atom_predicate(*head) {
                        self.rules_map
                            .entry(pred)
                            .or_default()
                            .push((*head, premises.clone(), transform.clone()));
                    }
                }
            }
        }
    }

    /// Pass 2: Check all unit clauses and rules against declared bounds.
    fn check_all_clauses(&mut self) -> Result<()> {
        let insts: Vec<Inst> = self.ir.insts.clone();
        for inst in &insts {
            match inst {
                Inst::Rule {
                    head,
                    premises,
                    transform,
                } => {
                    let head = *head;
                    let premises = premises.clone();
                    let transform = transform.clone();
                    if let Some(pred) = self.atom_predicate(head) {
                        if let Some(alternatives) = self.rel_type_map.get(&pred).cloned() {
                            if premises.is_empty() && transform.is_empty() {
                                self.check_fact(head, &alternatives)?;
                            } else {
                                self.check_rule(head, &premises, &transform, &alternatives)?;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
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
    fn check_rule(
        &mut self,
        head: InstId,
        premises: &[InstId],
        transforms: &[InstId],
        alternatives: &[Vec<InstId>],
    ) -> Result<()> {
        let head_args = self.atom_args(head);
        let pred = self.atom_predicate(head).unwrap();
        let is_temporal = self.ir.temporal_predicates.contains(&pred);

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
                let feasible =
                    self.get_or_infer_alternatives(pred, &args, &var_ranges);

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
                            if let Some(existing) = state.as_map().get(&v).copied() {
                                if type_expr::is_union_type(self.ir, existing) {
                                    let refined =
                                        type_expr::remove_from_union_type(self.ir, bound, existing);
                                    if !type_expr::is_empty_type(self.ir, refined) {
                                        state.set_var(v, refined);
                                    }
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
                    state.add_or_refine_with_ir(self.ir, lv, tpe);
                }
                if let Inst::Var(rv) = self.ir.get(right) {
                    let rv = *rv;
                    let tpe = self.bound_of_arg(left, &state.as_map());
                    state.add_or_refine_with_ir(self.ir, rv, tpe);
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
    fn infer_match_prefix(
        &mut self,
        args: &[InstId],
        mut state: InferState,
    ) -> Result<InferState> {
        if args.len() != 2 {
            return Ok(state);
        }
        let var_ranges = state.as_map();
        let tpe = self.bound_of_arg(args[0], &var_ranges);
        let prefix = self.bound_of_arg(args[1], &var_ranges);

        let ctx = TypeContext::default();
        let meet = type_expr::lower_bound(self.ir, &ctx, &[tpe, prefix]);
        if !type_expr::is_empty_type(self.ir, meet) {
            if let Inst::Var(v) = self.ir.get(args[0]) {
                let v = *v;
                state.add_or_refine_with_ir(self.ir, v, meet);
            }
            // Second arg (prefix) is typically a constant.
            let name_type = type_expr::find_or_create_name(self.ir, "/name");
            if let Inst::Var(v) = self.ir.get(args[1]) {
                let v = *v;
                state.add_or_refine_with_ir(self.ir, v, name_type);
            }
        }
        Ok(state)
    }

    /// Special case inference for `:match_field(Struct, FieldName, Value)`.
    fn infer_match_field(
        &mut self,
        args: &[InstId],
        mut state: InferState,
    ) -> Result<InferState> {
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

        if let Some(field) = field_name_id {
            if type_expr::is_struct_type(self.ir, scrutinee_type)
                || type_expr::is_tagged_union_type(self.ir, scrutinee_type)
                || type_expr::is_union_type(self.ir, scrutinee_type)
            {
                if let Some(field_type) =
                    type_expr::struct_type_field_deep(self.ir, scrutinee_type, field)
                {
                    // Bind the value variable.
                    let ctx = TypeContext::default();
                    let value_bound = self.bound_of_arg(args[2], &state.as_map());
                    let meet =
                        type_expr::lower_bound(self.ir, &ctx, &[value_bound, field_type]);
                    if !type_expr::is_empty_type(self.ir, meet) {
                        if let Inst::Var(v) = self.ir.get(args[2]) {
                            let v = *v;
                            state.add_or_refine_with_ir(self.ir, v, meet);
                        }
                    }
                }
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
    fn infer_match_entry(
        &mut self,
        args: &[InstId],
        mut state: InferState,
    ) -> Result<InferState> {
        if args.len() != 3 {
            return Ok(state);
        }
        let var_ranges = state.as_map();
        let map_type = self.bound_of_arg(args[0], &var_ranges);

        if type_expr::is_map_type(self.ir, map_type) {
            if let Some((key_type, val_type)) = type_expr::map_type_args(self.ir, map_type) {
                let ctx = TypeContext::default();

                // Bind key.
                let key_bound = self.bound_of_arg(args[1], &state.as_map());
                let key_meet =
                    type_expr::lower_bound(self.ir, &ctx, &[key_bound, key_type]);
                if !type_expr::is_empty_type(self.ir, key_meet) {
                    if let Inst::Var(v) = self.ir.get(args[1]) {
                        let v = *v;
                        state.add_or_refine_with_ir(self.ir, v, key_meet);
                    }
                }

                // Bind value.
                let val_bound = self.bound_of_arg(args[2], &state.as_map());
                let val_meet =
                    type_expr::lower_bound(self.ir, &ctx, &[val_bound, val_type]);
                if !type_expr::is_empty_type(self.ir, val_meet) {
                    if let Inst::Var(v) = self.ir.get(args[2]) {
                        let v = *v;
                        state.add_or_refine_with_ir(self.ir, v, val_meet);
                    }
                }
            }
        }
        Ok(state)
    }

    /// Special case inference for `:list:member(Elem, List)`.
    fn infer_list_member(
        &mut self,
        args: &[InstId],
        mut state: InferState,
    ) -> Result<InferState> {
        if args.len() != 2 {
            return Ok(state);
        }
        let var_ranges = state.as_map();
        let list_type = self.bound_of_arg(args[1], &var_ranges);

        if type_expr::is_list_type(self.ir, list_type) {
            if let Some(elem_type) = type_expr::list_type_arg(self.ir, list_type) {
                let ctx = TypeContext::default();
                let elem_bound = self.bound_of_arg(args[0], &state.as_map());
                let meet =
                    type_expr::lower_bound(self.ir, &ctx, &[elem_bound, elem_type]);
                if !type_expr::is_empty_type(self.ir, meet) {
                    if let Inst::Var(v) = self.ir.get(args[0]) {
                        let v = *v;
                        state.add_or_refine_with_ir(self.ir, v, meet);
                    }
                }
            }
        }
        Ok(state)
    }

    /// Infers the type bound for a single argument.
    fn bound_of_arg(
        &mut self,
        arg: InstId,
        var_ranges: &FxHashMap<NameId, InstId>,
    ) -> InstId {
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
    fn bound_of_apply_fn(
        &mut self,
        fname: &str,
        args: &[InstId],
        var_ranges: &FxHashMap<NameId, InstId>,
    ) -> InstId {
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
                let arg_types: Vec<InstId> = args
                    .iter()
                    .map(|a| self.bound_of_arg(*a, var_ranges))
                    .collect();
                type_expr::new_tuple_type(self.ir, arg_types)
            }
            "fn:struct_get" if args.len() == 2 => {
                let struct_type = self.bound_of_arg(args[0], var_ranges);
                if let Inst::Name(n) = self.ir.get(args[1]) {
                    let field = *n;
                    if let Some(ft) =
                        type_expr::struct_type_field_deep(self.ir, struct_type, field)
                    {
                        return ft;
                    }
                }
                type_expr::find_or_create_name(self.ir, "/any")
            }
            "fn:plus" | "fn:minus" | "fn:mult" | "fn:div" => {
                type_expr::find_or_create_name(self.ir, "/number")
            }
            "fn:float_plus" | "fn:float_mult" | "fn:float_div" => {
                type_expr::find_or_create_name(self.ir, "/float64")
            }
            "fn:string:concat" | "fn:string:replace" => {
                type_expr::find_or_create_name(self.ir, "/string")
            }
            "fn:count" | "fn:sum" | "fn:max" | "fn:min" => {
                type_expr::find_or_create_name(self.ir, "/number")
            }
            "fn:list:len" | "fn:len" | "fn:map:len" | "fn:struct:len" => {
                type_expr::find_or_create_name(self.ir, "/number")
            }
            "fn:sqrt" => type_expr::find_or_create_name(self.ir, "/float64"),
            "fn:list:get" if args.len() == 2 => {
                // Element type of the list argument, or /any if not a list.
                let list_type = self.bound_of_arg(args[0], var_ranges);
                match type_expr::apply_fn_args(self.ir, list_type) {
                    Some(inner) if type_expr::apply_fn_name(self.ir, list_type)
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
                    Some(inner) if type_expr::apply_fn_name(self.ir, list_type)
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
            "fn:collect" | "fn:collect_distinct" => {
                if args.len() == 1 {
                    let elem_type = self.bound_of_arg(args[0], var_ranges);
                    type_expr::new_list_type(self.ir, elem_type)
                } else {
                    let any = type_expr::find_or_create_name(self.ir, "/any");
                    type_expr::new_list_type(self.ir, any)
                }
            }
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
                let arg_strs: Vec<String> =
                    args.iter().map(|a| self.describe_inst(*a)).collect();
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
        assert!(check(r#"
            Decl pair(X, Y) bound [/number, /number] bound [/string, /string].
            pair(42, 99).
        "#).is_ok());
    }

    #[test]
    fn multiple_alternatives_second_matches() {
        // pair("a", "b") matches second alternative [/string, /string].
        assert!(check(r#"
            Decl pair(X, Y) bound [/number, /number] bound [/string, /string].
            pair("a", "b").
        "#).is_ok());
    }

    #[test]
    fn multiple_alternatives_none_matches() {
        // pair(42, "b") matches neither alternative.
        assert!(check(r#"
            Decl pair(X, Y) bound [/number, /number] bound [/string, /string].
            pair(42, "b").
        "#).is_err());
    }

    // -----------------------------------------------------------------------
    // Rule type inference: variable binding from premises
    // -----------------------------------------------------------------------

    #[test]
    fn rule_infers_type_from_premise() {
        // X gets type /number from src, which conforms to dst's bound.
        assert!(check(r#"
            Decl src(X) bound [/number].
            Decl dst(X) bound [/number].
            dst(X) :- src(X).
        "#).is_ok());
    }

    #[test]
    fn rule_type_mismatch_from_premise() {
        // X inferred as /string from src, but dst expects /number.
        assert!(check(r#"
            Decl src(X) bound [/string].
            Decl dst(X) bound [/number].
            dst(X) :- src(X).
        "#).is_err());
    }

    // -----------------------------------------------------------------------
    // Multiple body atoms refining the same variable (LowerBound)
    // -----------------------------------------------------------------------

    #[test]
    fn two_premises_refine_variable() {
        // X starts as fn:Union(/number, /string) from 'wide',
        // then refined to /number from 'narrow'. Should conform to /number.
        assert!(check(r#"
            Decl wide(X) bound [fn:Union(/number, /string)].
            Decl narrow(X) bound [/number].
            Decl result(X) bound [/number].
            result(X) :- wide(X), narrow(X).
        "#).is_ok());
    }

    #[test]
    fn two_premises_refine_to_incompatible() {
        // X inferred as /string from src1, then /number from src2.
        // Intersection is empty, so X keeps /string (conservative).
        // /string does not conform to /number → error.
        assert!(check(r#"
            Decl src1(X) bound [/string].
            Decl src2(X) bound [/number].
            Decl dst(X) bound [/number].
            dst(X) :- src1(X), src2(X).
        "#).is_err());
    }

    // -----------------------------------------------------------------------
    // Polymorphic type declarations (skolemization)
    // -----------------------------------------------------------------------

    #[test]
    fn polymorphic_identity_number() {
        // T is a type variable. pair(42, 99) should pass: T can be /number.
        assert!(check(r#"
            Decl pair(X, Y) bound [T, T].
            pair(42, 99).
        "#).is_ok());
    }

    #[test]
    fn polymorphic_identity_string() {
        // T is a type variable. pair("a", "b") should pass: T can be /string.
        assert!(check(r#"
            Decl pair(X, Y) bound [T, T].
            pair("a", "b").
        "#).is_ok());
    }

    #[test]
    fn polymorphic_rule_with_inferred_type() {
        // T skolemized to fresh var. X inferred as /number from src.
        // /number conforms to ?X0 (mapped to /any in context) → passes.
        assert!(check(r#"
            Decl src(X) bound [/number].
            Decl dst(X) bound [T].
            dst(X) :- src(X).
        "#).is_ok());
    }

    // -----------------------------------------------------------------------
    // Cross-predicate inference
    // -----------------------------------------------------------------------

    #[test]
    fn cross_predicate_inference_basic() {
        // 'helper' has no declaration. Its type is inferred from its rule
        // (which uses 'src' with bound [/number]). Then 'dst' uses 'helper'.
        assert!(check(r#"
            Decl src(X) bound [/number].
            Decl dst(X) bound [/number].
            helper(X) :- src(X).
            dst(X) :- helper(X).
        "#).is_ok());
    }

    #[test]
    fn cross_predicate_inference_type_mismatch() {
        // 'helper' inferred as /string from src. dst expects /number → error.
        assert!(check(r#"
            Decl src(X) bound [/string].
            Decl dst(X) bound [/number].
            helper(X) :- src(X).
            dst(X) :- helper(X).
        "#).is_err());
    }

    #[test]
    fn cross_predicate_inference_chain() {
        // Chain: src → mid → dst, only src and dst declared.
        assert!(check(r#"
            Decl src(X) bound [/number].
            Decl dst(X) bound [/number].
            mid(X) :- src(X).
            dst(X) :- mid(X).
        "#).is_ok());
    }

    // -----------------------------------------------------------------------
    // Equality and inequality premises
    // -----------------------------------------------------------------------

    #[test]
    fn equality_binds_variable() {
        // X = "hello" gives X type /string.
        assert!(check(r#"
            Decl src(X) bound [/string].
            Decl dst(X) bound [/string].
            dst(X) :- src(X), X = "hello".
        "#).is_ok());
    }

    #[test]
    fn inequality_refines_variable() {
        // X from src is /string, X != "bad" should still be /string.
        assert!(check(r#"
            Decl src(X) bound [/string].
            Decl dst(X) bound [/string].
            dst(X) :- src(X), X != "bad".
        "#).is_ok());
    }

    // -----------------------------------------------------------------------
    // Transform (let) expressions
    // -----------------------------------------------------------------------

    #[test]
    fn transform_arithmetic() {
        // let Y = fn:plus(X, 1) → Y inferred as /number.
        assert!(check(r#"
            Decl src(X) bound [/number].
            Decl dst(X, Y) bound [/number, /number].
            dst(X, Y) :- src(X) |> let Y = fn:plus(X, 1).
        "#).is_ok());
    }

    #[test]
    fn transform_string_concat() {
        // let Y = fn:string:concat(X, "!") → Y inferred as /string.
        assert!(check(r#"
            Decl src(X) bound [/string].
            Decl dst(X, Y) bound [/string, /string].
            dst(X, Y) :- src(X) |> let Y = fn:string:concat(X, "!").
        "#).is_ok());
    }

    #[test]
    fn transform_type_mismatch() {
        // Y = fn:plus(X, 1) → /number, but dst expects /string for Y.
        assert!(check(r#"
            Decl src(X) bound [/number].
            Decl dst(X, Y) bound [/number, /string].
            dst(X, Y) :- src(X) |> let Y = fn:plus(X, 1).
        "#).is_err());
    }

    // -----------------------------------------------------------------------
    // No-declaration predicates (no bounds checking needed)
    // -----------------------------------------------------------------------

    #[test]
    fn undeclared_predicate_passes() {
        // Rules with no declarations should pass without error.
        assert!(check(r#"
            foo(1).
            bar(X) :- foo(X).
        "#).is_ok());
    }
}
