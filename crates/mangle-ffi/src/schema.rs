//! Schema cache + introspection.
//!
//! After `mangle_load_rules` compiles a program, the engine walks the
//! resulting `StratifiedProgram` (+ its arena) once and caches a
//! [`Schema`] alongside the interpreter. The cache is used by every
//! entry point that names a relation:
//!
//!   - `mangle_query`, `mangle_save_relation_mgr`, `mangle_insert_fact`,
//!     `mangle_retract_fact`, `mangle_load_facts_mgr` all consult the
//!     schema before touching the store. Unknown relations get
//!     [`crate::MANGLE_ERR_UNKNOWN_RELATION`] with a precise message.
//!   - `mangle_query_dump_mgr` uses the schema's arity for empty-result
//!     exports (fixing the M7 SimpleRow arity-from-first-tuple limit).
//!   - `mangle_schema_snapshot` and `mangle_relation_names` serialize
//!     the cache for the workbench's schema-graph visualization.
//!
//! Type signatures (`type_args`) are emitted as `null` for now. The
//! Ir does carry type-checked annotations via `Decl.descr` /
//! `Decl.bounds`, but extracting them cleanly requires more Ir-walker
//! work than M8 needs to do; we can fill them in later without an ABI
//! change.

use mangle_analysis::StratifiedProgram;
use mangle_ast::{Atom, Term};
use std::collections::BTreeMap;

/// Kind of a predicate. EDB = extensional (loaded as facts); IDB =
/// intensional (defined by one or more rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PredicateKind {
    Edb,
    Idb,
}

impl PredicateKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PredicateKind::Edb => "edb",
            PredicateKind::Idb => "idb",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PredicateSchema {
    pub arity: usize,
    /// Type signature, one entry per argument. `None` when the
    /// program didn't supply explicit type annotations or the
    /// type-checker couldn't infer one. Per-arg `None` entries are
    /// allowed when only some args have type info — but for M8 we
    /// emit the whole field as `None` because we don't yet walk the
    /// Ir's `Decl.bounds`/`descr` for type information.
    #[allow(dead_code)] // populated in a later milestone
    pub type_args: Option<Vec<String>>,
    pub kind: PredicateKind,
}

#[derive(Debug, Clone)]
pub(crate) struct RuleEdges {
    pub rule_id: usize,
    pub head: String,
    pub body: Vec<String>,
}

/// Cached schema for a Loaded engine.
///
/// `predicates` is a `BTreeMap` so the snapshot output ordering is
/// stable (sorted by name). `rules` is in load order with global ids.
#[derive(Debug, Clone, Default)]
pub(crate) struct Schema {
    pub predicates: BTreeMap<String, PredicateSchema>,
    pub rules: Vec<RuleEdges>,
}

impl Schema {
    /// Build the schema from a freshly-compiled `StratifiedProgram`.
    /// Called from `MangleEngine::load_rules` after `compile_units`
    /// returns; the result is cached on the engine.
    pub(crate) fn build(stratified: &StratifiedProgram<'_>) -> Self {
        let arena = stratified.arena();
        let mut predicates: BTreeMap<String, PredicateSchema> = BTreeMap::new();

        // Both edb_preds and idb_preds yield PredicateIndex from the
        // arena's predicate table. `Arena::predicate_arity` is `None`
        // for predicates the parser didn't tag with explicit arity
        // (which is most fact-only sources), so we walk every atom
        // (rule heads + body premises) once and record arity by
        // sym from the actual argument count. The walker covers
        // both EDB and IDB predicates this way.
        let edb = stratified.extensional_preds();
        let idb = stratified.intensional_preds();

        let mut atom_arity: std::collections::HashMap<ast_predicate_index::PredicateIndex, usize> =
            std::collections::HashMap::new();
        // A predicate is *really* IDB (in the user-visible sense) iff
        // at least one of its clauses has non-empty premises. Mangle's
        // internal `rules` map keys all predicates that ever appear as
        // a clause head — including pure facts like `edge(1, 2).` which
        // are head-only clauses with no body. For the workbench
        // visualization "EDB vs IDB" should distinguish facts from
        // derived predicates, so we recompute IDB-ness here.
        let mut has_real_rule: std::collections::HashSet<ast_predicate_index::PredicateIndex> =
            std::collections::HashSet::new();
        for sym in &idb {
            for clause in stratified.rules(*sym) {
                atom_arity.insert(clause.head.sym, clause.head.args.len());
                if !clause.premises.is_empty() {
                    has_real_rule.insert(clause.head.sym);
                }
                for premise in clause.premises {
                    if let Some(atom) = atom_of_term(premise) {
                        atom_arity.insert(atom.sym, atom.args.len());
                    }
                }
            }
        }

        let resolve_arity = |sym: ast_predicate_index::PredicateIndex| -> usize {
            arena
                .predicate_arity(sym)
                .map(usize::from)
                .or_else(|| atom_arity.get(&sym).copied())
                .unwrap_or(0)
        };

        for sym in &idb {
            let Some(name) = arena.predicate_name(*sym) else {
                continue;
            };
            let kind = if has_real_rule.contains(sym) {
                PredicateKind::Idb
            } else {
                // Fact-only predicate — head-only clauses are facts,
                // not derivations.
                PredicateKind::Edb
            };
            predicates.insert(
                name.to_string(),
                PredicateSchema {
                    arity: resolve_arity(*sym),
                    type_args: None,
                    kind,
                },
            );
        }

        for sym in &edb {
            let Some(name) = arena.predicate_name(*sym) else {
                continue;
            };
            // EDB/IDB shouldn't overlap, but if the IDB pass already
            // recorded this name, leave it alone.
            if predicates.contains_key(name) {
                continue;
            }
            predicates.insert(
                name.to_string(),
                PredicateSchema {
                    arity: resolve_arity(*sym),
                    type_args: None,
                    kind: PredicateKind::Edb,
                },
            );
        }

        // Rules: globally numbered, in (head name, rule order) order.
        // Only true IDB rules (with non-empty premises) — fact-only
        // "rules" don't appear in the rule list.
        let mut rules: Vec<RuleEdges> = Vec::new();
        let mut head_names: Vec<(ast_predicate_index::PredicateIndex, String)> = idb
            .iter()
            .filter(|sym| has_real_rule.contains(sym))
            .filter_map(|sym| arena.predicate_name(*sym).map(|n| (*sym, n.to_string())))
            .collect();
        head_names.sort_by(|a, b| a.1.cmp(&b.1));
        for (sym, head_name) in &head_names {
            for clause in stratified.rules(*sym) {
                if clause.premises.is_empty() {
                    continue;
                }
                let body = collect_body_predicates(clause.premises, arena);
                rules.push(RuleEdges {
                    rule_id: rules.len(),
                    head: head_name.clone(),
                    body,
                });
            }
        }

        Schema { predicates, rules }
    }

    /// Serialize the schema as JSON (the shape exposed via
    /// `mangle_schema_snapshot`).
    pub(crate) fn to_json(&self) -> Vec<u8> {
        let preds: Vec<serde_json::Value> = self
            .predicates
            .iter()
            .map(|(name, p)| {
                serde_json::json!({
                    "name": name,
                    "arity": p.arity,
                    "type_args": p.type_args,
                    "kind": p.kind.as_str(),
                })
            })
            .collect();
        let rules: Vec<serde_json::Value> = self
            .rules
            .iter()
            .map(|r| {
                serde_json::json!({
                    "rule_id": r.rule_id,
                    "head": r.head,
                    "body": r.body,
                })
            })
            .collect();
        let doc = serde_json::json!({
            "predicates": preds,
            "rules": rules,
        });
        serde_json::to_vec(&doc).expect("schema JSON serialization")
    }

    /// Serialize just the relation-name list (sorted), for the cheap
    /// picker-UI snapshot.
    pub(crate) fn relation_names_json(&self) -> Vec<u8> {
        let names: Vec<&str> = self.predicates.keys().map(String::as_str).collect();
        serde_json::to_vec(&names).expect("names JSON serialization")
    }

    /// True iff the predicate is declared in the loaded program.
    pub(crate) fn knows(&self, predicate: &str) -> bool {
        self.predicates.contains_key(predicate)
    }

    /// Look up a predicate's schema info. `None` for unknown.
    pub(crate) fn lookup(&self, predicate: &str) -> Option<&PredicateSchema> {
        self.predicates.get(predicate)
    }
}

/// Extract an `&Atom` from a `Term`, ignoring the equality/inequality
/// premise variants which don't reference a predicate.
fn atom_of_term<'a>(t: &Term<'a>) -> Option<&'a Atom<'a>> {
    match t {
        Term::Atom(a) | Term::NegAtom(a) | Term::TemporalAtom(a, _) => Some(*a),
        Term::Eq(_, _) | Term::Ineq(_, _) => None,
    }
}

/// Collect the names of predicates appearing as atoms in a rule's
/// premise list, preserving order and including duplicates.
fn collect_body_predicates(premises: &[&Term<'_>], arena: &mangle_ast::Arena) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for premise in premises {
        if let Some(atom) = atom_of_term(premise)
            && let Some(name) = arena.predicate_name(atom.sym)
        {
            out.push(name.to_string());
        }
    }
    out
}

// Disambiguate the PredicateIndex import without forcing ast as a
// pub-use crate-wide.
mod ast_predicate_index {
    pub use mangle_ast::PredicateIndex;
}

// ---- C ABI entry points ----------------------------------------------

use crate::buffer::{MangleBuffer, write_buffer};
use crate::engine::MangleEngine;
use crate::error::{panic_boundary, set_error_msg};
use crate::{MANGLE_ERR_INVALID_ARG, MANGLE_ERR_NO_RULES, MANGLE_OK};

/// Emit the engine's schema as a JSON document.
///
/// Shape:
/// ```json
/// {
///   "predicates": [
///     { "name": "edge", "arity": 2, "type_args": null, "kind": "edb" },
///     { "name": "reachable", "arity": 2, "type_args": null, "kind": "idb" }
///   ],
///   "rules": [
///     { "rule_id": 0, "head": "reachable", "body": ["edge"] },
///     { "rule_id": 1, "head": "reachable", "body": ["edge", "reachable"] }
///   ]
/// }
/// ```
///
/// `kind` is `"edb"` or `"idb"` based on whether the predicate has
/// any defining rules. `type_args` will populate in a later milestone
/// when the Ir's `Decl.bounds` walker is wired up; for now it's
/// `null`.
///
/// Returns [`MANGLE_OK`] on success; the buffer is owned by the
/// caller and must be released with `mangle_buffer_free`. Returns
/// [`MANGLE_ERR_NO_RULES`] when the engine has no program loaded, or
/// [`MANGLE_ERR_INVALID_ARG`] for null `out`.
///
/// # Safety
/// `engine` must be a live handle. `out` must be non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_schema_snapshot(
    engine: *mut MangleEngine,
    out: *mut MangleBuffer,
) -> i32 {
    panic_boundary!(engine, {
        if out.is_null() {
            set_error_msg("mangle_schema_snapshot: out pointer is null");
            return MANGLE_ERR_INVALID_ARG;
        }
        // SAFETY: engine non-null and not poisoned per panic_boundary.
        let eng = unsafe { &*engine };
        let Some(schema) = eng.schema() else {
            set_error_msg("mangle_schema_snapshot: engine has no rules loaded");
            return MANGLE_ERR_NO_RULES;
        };
        let bytes = schema.to_json();
        // SAFETY: out non-null per the precondition.
        unsafe { write_buffer(out, bytes) };
        MANGLE_OK
    })
}

/// Emit the sorted list of declared predicate names as a JSON array.
///
/// Cheap alternative to [`mangle_schema_snapshot`] when you only need
/// the picker UI's relation dropdown — no arity, no rule edges, just
/// the names.
///
/// # Safety
/// `engine` must be a live handle. `out` must be non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_relation_names(
    engine: *mut MangleEngine,
    out: *mut MangleBuffer,
) -> i32 {
    panic_boundary!(engine, {
        if out.is_null() {
            set_error_msg("mangle_relation_names: out pointer is null");
            return MANGLE_ERR_INVALID_ARG;
        }
        let eng = unsafe { &*engine };
        let Some(schema) = eng.schema() else {
            set_error_msg("mangle_relation_names: engine has no rules loaded");
            return MANGLE_ERR_NO_RULES;
        };
        let bytes = schema.relation_names_json();
        unsafe { write_buffer(out, bytes) };
        MANGLE_OK
    })
}

/// Emit a "facts overview" snapshot — every declared relation with its
/// arity, kind (EDB/IDB), current tuple count, and at most
/// `per_relation_limit` sample tuples.
///
/// Output shape:
/// ```json
/// {
///   "relations": [
///     {
///       "name": "edge",
///       "arity": 2,
///       "kind": "edb",
///       "count": 12345,
///       "sample": [
///         { "tuple": [1, 2] },
///         { "tuple": [2, 3] }
///       ]
///     }
///   ]
/// }
/// ```
///
/// Relations come from the schema cache so declared-but-empty
/// predicates also appear (with `count: 0, sample: []`). Tuple
/// elements use the same value-to-JSON encoding as
/// `mangle_derivation_tree` — scalars as primitives,
/// `Name`/`Time`/`Duration`/`Compound` as tagged objects (lossy but
/// unambiguous for visualization).
///
/// `per_relation_limit = 0` means "don't include any samples, just
/// counts." Pass `UINT32_MAX` for "include everything" (workbench-
/// scale only — large stores should use the batch-encode endpoints
/// instead).
///
/// # Safety
/// `engine` must be a live handle. `out` must be non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_facts_snapshot(
    engine: *mut MangleEngine,
    per_relation_limit: u32,
    out: *mut MangleBuffer,
) -> i32 {
    panic_boundary!(engine, {
        if out.is_null() {
            set_error_msg("mangle_facts_snapshot: out pointer is null");
            return MANGLE_ERR_INVALID_ARG;
        }
        let eng = unsafe { &*engine };
        let Some(schema) = eng.schema() else {
            set_error_msg("mangle_facts_snapshot: engine has no rules loaded");
            return MANGLE_ERR_NO_RULES;
        };

        let limit = per_relation_limit as usize;
        let mut relations: Vec<serde_json::Value> = Vec::new();
        for (name, info) in &schema.predicates {
            let (count, sample) = match eng.count_and_sample(name, limit) {
                Ok(Some((c, s))) => (c, s),
                Ok(None) => {
                    // Schema says rules are loaded, so this shouldn't
                    // happen. Treat defensively as empty.
                    (0, Vec::new())
                }
                Err(e) => {
                    set_error_msg(format!("mangle_facts_snapshot({name}): {e:#}"));
                    return crate::MANGLE_ERR;
                }
            };
            let sample_json: Vec<serde_json::Value> = sample
                .iter()
                .map(|tuple| {
                    let cells: Vec<serde_json::Value> =
                        tuple.iter().map(crate::value::value_to_json).collect();
                    serde_json::json!({ "tuple": cells })
                })
                .collect();
            relations.push(serde_json::json!({
                "name": name,
                "arity": info.arity,
                "kind": info.kind.as_str(),
                "count": count,
                "sample": sample_json,
            }));
        }

        let doc = serde_json::json!({ "relations": relations });
        let bytes = serde_json::to_vec(&doc).expect("snapshot serialize");
        unsafe { write_buffer(out, bytes) };
        MANGLE_OK
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pred_kind_str() {
        assert_eq!(PredicateKind::Edb.as_str(), "edb");
        assert_eq!(PredicateKind::Idb.as_str(), "idb");
    }

    #[test]
    fn empty_schema_json() {
        let s = Schema::default();
        let bytes = s.to_json();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["predicates"], serde_json::json!([]));
        assert_eq!(v["rules"], serde_json::json!([]));
    }

    #[test]
    fn relation_names_empty() {
        let s = Schema::default();
        let bytes = s.relation_names_json();
        assert_eq!(bytes, b"[]");
    }

    // Walker tests need a compiled program — done in tests/schema.rs
    // (integration) so we can use the public mangle_load_rules entry.
}
