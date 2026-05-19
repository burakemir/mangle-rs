//! Provenance / derivation tree export.
//!
//! When the engine is constructed with `enable_provenance = 1`,
//! `load_rules` captures the interpreter's recorded
//! `ProvenanceEntry` list into a [`DerivationIndex`] keyed by derived
//! fact. `mangle_derivation_tree` parses a fact atom (using the same
//! lenient parser as queries), looks it up in the index, and walks
//! the premises recursively into a JSON tree.
//!
//! Output shape (recursive):
//! ```json
//! {
//!   "fact": { "relation": "reachable", "tuple": [1, 3] },
//!   "derivations": [
//!     {
//!       "rule_id": null,
//!       "premises": [
//!         { "fact": {...}, "derivations": [...] },
//!         { "fact": {...}, "derivations": [...] }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! EDB facts (or any fact with no recorded derivations) emit
//! `"derivations": []`. When `max_depth` is reached, deeper subtrees
//! are emitted as `"derivations": null` so the consumer can tell
//! truncation apart from a true EDB leaf.
//!
//! Note: `rule_id` is currently always `null`. The interpreter's
//! `ProvenanceEntry` doesn't carry the rule identifier (only
//! `(derived, premises)`), so the field is reserved in the JSON
//! shape but not populated. Walker is cycle-safe via a visited-set
//! threaded through the recursion.

use std::collections::{HashMap, HashSet};

use mangle_common::{CompoundKind, Value};
use mangle_interpreter::ProvenanceEntry;

use crate::buffer::{MangleBuffer, write_buffer};
use crate::engine::MangleEngine;
use crate::error::{panic_boundary, set_error_msg};
use crate::query::{ParsedQuery, QueryArg, parse_query_lenient};
use crate::{
    MANGLE_ERR_FACT_NOT_FOUND, MANGLE_ERR_INVALID_ARG, MANGLE_ERR_NO_PROVENANCE,
    MANGLE_ERR_NO_RULES, MANGLE_ERR_PARSE, MANGLE_OK,
};

/// Hashable key for a fact: relation name + ground tuple.
pub(crate) type FactKey = (String, Vec<Value>);

/// One derivation: the list of premise facts that produced the target.
type Derivation = Vec<FactKey>;

/// Lookup table: derived fact → list of derivations.
///
/// Built once at `load_rules` time from the interpreter's
/// `ProvenanceRecorder.entries`. Multiple derivations of the same
/// fact (from different rules or different bindings) are all
/// captured.
#[derive(Debug, Default)]
pub(crate) struct DerivationIndex {
    by_derived: HashMap<FactKey, Vec<Derivation>>,
}

impl DerivationIndex {
    pub(crate) fn build(entries: &[ProvenanceEntry]) -> Self {
        let mut by_derived: HashMap<FactKey, Vec<Derivation>> = HashMap::new();
        for e in entries {
            by_derived
                .entry(e.derived.clone())
                .or_default()
                .push(e.premises.clone());
        }
        Self { by_derived }
    }

    /// True iff at least one derivation is recorded for `fact`.
    pub(crate) fn has_derivations(&self, fact: &FactKey) -> bool {
        self.by_derived.get(fact).is_some_and(|v| !v.is_empty())
    }
}

/// Parse a fact atom (e.g. `reachable(1, 3)`) into a [`FactKey`].
/// Reuses the lenient query parser, then rejects any non-constant
/// arguments (variables aren't allowed in fact lookups).
pub(crate) fn parse_fact_atom(s: &str) -> Result<FactKey, String> {
    let ParsedQuery { predicate, args } = parse_query_lenient(s).map_err(|e| format!("{e:#}"))?;
    let mut tuple: Vec<Value> = Vec::with_capacity(args.len());
    for (i, arg) in args.iter().enumerate() {
        match arg {
            QueryArg::Variable => {
                return Err(format!(
                    "argument {i} of `{s}` is a variable; fact atoms must be fully ground"
                ));
            }
            QueryArg::StringConst(v) => tuple.push(Value::String(v.clone())),
            QueryArg::NameConst(v) => tuple.push(Value::Name(v.clone())),
            QueryArg::NumberConst(n) => tuple.push(Value::Number(*n)),
        }
    }
    Ok((predicate, tuple))
}

/// Render a single `Value` as JSON. Scalars become primitives;
/// non-primitives become tagged objects so the consumer can tell
/// `String("x")` from `Name("/x")` etc.
fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Number(n) => serde_json::json!(*n),
        Value::Float(f) => serde_json::json!(*f),
        Value::String(s) => serde_json::json!(s),
        Value::Name(n) => serde_json::json!({ "name": n }),
        Value::Time(ns) => serde_json::json!({ "time_ns": ns }),
        Value::Duration(ns) => serde_json::json!({ "duration_ns": ns }),
        Value::Compound(kind, elems) => {
            let subkind = match kind {
                CompoundKind::List => "list",
                CompoundKind::Pair => "pair",
                CompoundKind::Map => "map",
                CompoundKind::Struct => "struct",
            };
            let elems: Vec<serde_json::Value> = elems.iter().map(value_to_json).collect();
            serde_json::json!({ "compound": subkind, "elems": elems })
        }
    }
}

fn fact_to_json(fact: &FactKey) -> serde_json::Value {
    let tuple: Vec<serde_json::Value> = fact.1.iter().map(value_to_json).collect();
    serde_json::json!({
        "relation": fact.0,
        "tuple": tuple,
    })
}

/// Walk the derivation tree for `root` and produce JSON.
///
/// `max_depth` is the maximum nesting depth — `0` means "just the
/// root, no derivations" (returns the root with `derivations: null`).
/// `u32::MAX` is "no cap".
fn walk(index: &DerivationIndex, root: &FactKey, max_depth: u32) -> serde_json::Value {
    let mut visited: HashSet<FactKey> = HashSet::new();
    walk_inner(index, root, max_depth, 0, &mut visited)
}

fn walk_inner(
    index: &DerivationIndex,
    fact: &FactKey,
    max_depth: u32,
    depth: u32,
    visited: &mut HashSet<FactKey>,
) -> serde_json::Value {
    // Truncation: emit `null` instead of an empty list so the consumer
    // can distinguish "EDB leaf" (empty list) from "depth cutoff hit".
    if depth >= max_depth {
        return serde_json::json!({
            "fact": fact_to_json(fact),
            "derivations": serde_json::Value::Null,
        });
    }
    // Cycle safety: if we re-enter the same fact, emit the leaf
    // representation rather than recursing.
    if !visited.insert(fact.clone()) {
        return serde_json::json!({
            "fact": fact_to_json(fact),
            "derivations": serde_json::Value::Null,
        });
    }
    let derivations: Vec<serde_json::Value> = match index.by_derived.get(fact) {
        Some(list) => list
            .iter()
            .map(|premises| {
                let premise_trees: Vec<serde_json::Value> = premises
                    .iter()
                    .map(|p| walk_inner(index, p, max_depth, depth + 1, visited))
                    .collect();
                serde_json::json!({
                    "rule_id": serde_json::Value::Null,
                    "premises": premise_trees,
                })
            })
            .collect(),
        None => Vec::new(),
    };
    visited.remove(fact);
    serde_json::json!({
        "fact": fact_to_json(fact),
        "derivations": derivations,
    })
}

/// Emit the derivation tree for a given fact as JSON.
///
/// The fact is parsed as a Mangle atom — same syntax as queries (see
/// `mangle_query`) — but variables aren't allowed: the fact must be
/// fully ground.
///
/// Requires the engine was constructed with `enable_provenance = 1`.
/// Returns:
/// - [`MANGLE_OK`] on success; JSON buffer owned by caller.
/// - [`MANGLE_ERR_NO_RULES`] when no program is loaded.
/// - [`MANGLE_ERR_NO_PROVENANCE`] when the engine was built without
///   provenance.
/// - [`MANGLE_ERR_PARSE`] for a malformed atom or a variable arg.
/// - [`MANGLE_ERR_FACT_NOT_FOUND`] when the fact has no recorded
///   derivations (EDB facts with no IDB consumers may not appear at
///   all in the provenance index; user typos likewise).
/// - [`MANGLE_ERR_INVALID_ARG`] for null pointers.
///
/// `max_depth` is the maximum nesting depth of the returned tree.
/// Use `0xFFFF_FFFF` (`UINT32_MAX`) for unlimited.
///
/// # Safety
/// `engine` must be a live handle. `fact` must point to `len`
/// readable UTF-8 bytes. `out` must be non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_derivation_tree(
    engine: *mut MangleEngine,
    fact: *const u8,
    len: usize,
    max_depth: u32,
    out: *mut MangleBuffer,
) -> i32 {
    panic_boundary!(engine, {
        if out.is_null() {
            set_error_msg("mangle_derivation_tree: out pointer is null");
            return MANGLE_ERR_INVALID_ARG;
        }
        let fact_str = if len == 0 {
            ""
        } else if fact.is_null() {
            set_error_msg("mangle_derivation_tree: fact pointer is null but len > 0");
            return MANGLE_ERR_INVALID_ARG;
        } else {
            // SAFETY: caller's contract.
            let slice = unsafe { std::slice::from_raw_parts(fact, len) };
            match std::str::from_utf8(slice) {
                Ok(s) => s,
                Err(e) => {
                    set_error_msg(format!(
                        "mangle_derivation_tree: fact is not valid UTF-8: {e}"
                    ));
                    return MANGLE_ERR_INVALID_ARG;
                }
            }
        };

        // SAFETY: engine non-null and not poisoned per panic_boundary.
        let eng = unsafe { &*engine };
        let prov = match eng.provenance() {
            Some(p) => p,
            None => {
                // Distinguish "engine has no program" from "engine has
                // a program but provenance wasn't enabled."
                if eng.schema().is_none() {
                    set_error_msg("mangle_derivation_tree: engine has no rules loaded");
                    return MANGLE_ERR_NO_RULES;
                }
                set_error_msg(
                    "mangle_derivation_tree: engine was built without provenance; \
                     pass enable_provenance=1 to mangle_engine_new",
                );
                return MANGLE_ERR_NO_PROVENANCE;
            }
        };

        let target = match parse_fact_atom(fact_str) {
            Ok(t) => t,
            Err(e) => {
                set_error_msg(format!("mangle_derivation_tree: {e}"));
                return MANGLE_ERR_PARSE;
            }
        };

        // If the target has no recorded derivations *and* isn't
        // referenced as a premise of anything else, treat it as
        // "fact not found" rather than emitting an empty-derivations
        // tree (which would happen for true EDB facts too — see
        // below for the EDB nuance).
        if !prov.has_derivations(&target) && !appears_as_premise(prov, &target) {
            set_error_msg(format!(
                "mangle_derivation_tree: no derivation recorded for `{}({:?})`",
                target.0, target.1
            ));
            return MANGLE_ERR_FACT_NOT_FOUND;
        }

        let tree = walk(prov, &target, max_depth);
        let bytes = serde_json::to_vec(&tree).expect("serde_json serialize");
        // SAFETY: out non-null per the precondition.
        unsafe { write_buffer(out, bytes) };
        MANGLE_OK
    })
}

/// True iff `fact` appears as a premise of any recorded derivation.
/// Used to distinguish "leaf EDB fact in a provenance tree" from
/// "unknown fact not in the tree at all."
fn appears_as_premise(index: &DerivationIndex, fact: &FactKey) -> bool {
    for derivations in index.by_derived.values() {
        for premises in derivations {
            if premises.contains(fact) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fk(name: &str, tuple: &[i64]) -> FactKey {
        (
            name.to_string(),
            tuple.iter().copied().map(Value::Number).collect(),
        )
    }

    #[test]
    fn empty_index_returns_no_derivations() {
        let idx = DerivationIndex::default();
        assert!(!idx.has_derivations(&fk("reachable", &[1, 2])));
    }

    #[test]
    fn build_groups_entries_by_derived_fact() {
        let entries = vec![
            ProvenanceEntry {
                derived: ("reachable".into(), vec![Value::Number(1), Value::Number(3)]),
                premises: vec![("edge".into(), vec![Value::Number(1), Value::Number(2)])],
            },
            ProvenanceEntry {
                derived: ("reachable".into(), vec![Value::Number(1), Value::Number(3)]),
                premises: vec![("edge".into(), vec![Value::Number(1), Value::Number(3)])],
            },
        ];
        let idx = DerivationIndex::build(&entries);
        let target = fk("reachable", &[1, 3]);
        assert!(idx.has_derivations(&target));
        assert_eq!(idx.by_derived.get(&target).unwrap().len(), 2);
    }

    #[test]
    fn parse_fact_rejects_variable() {
        let r = parse_fact_atom("reachable(X, 3)");
        assert!(r.is_err(), "variable should be rejected");
    }

    #[test]
    fn parse_fact_accepts_ground_atom() {
        let r = parse_fact_atom("reachable(1, 3)").unwrap();
        assert_eq!(r.0, "reachable");
        assert_eq!(r.1, vec![Value::Number(1), Value::Number(3)]);
    }

    #[test]
    fn parse_fact_string_const() {
        let r = parse_fact_atom(r#"route("GET", "/")"#).unwrap();
        assert_eq!(r.0, "route");
        assert_eq!(
            r.1,
            vec![Value::String("GET".into()), Value::String("/".into())]
        );
    }

    #[test]
    fn walk_emits_truncation_at_max_depth() {
        let entries = vec![ProvenanceEntry {
            derived: ("reachable".into(), vec![Value::Number(1), Value::Number(2)]),
            premises: vec![("edge".into(), vec![Value::Number(1), Value::Number(2)])],
        }];
        let idx = DerivationIndex::build(&entries);
        // max_depth=0 → root with derivations: null.
        let v = walk(&idx, &fk("reachable", &[1, 2]), 0);
        assert!(v["derivations"].is_null());
    }

    #[test]
    fn walk_emits_empty_for_leaf() {
        let entries = vec![ProvenanceEntry {
            derived: ("reachable".into(), vec![Value::Number(1), Value::Number(2)]),
            premises: vec![("edge".into(), vec![Value::Number(1), Value::Number(2)])],
        }];
        let idx = DerivationIndex::build(&entries);
        // walk the EDB leaf — has no entries, emits derivations: [].
        let v = walk(&idx, &fk("edge", &[1, 2]), 10);
        assert_eq!(v["derivations"], serde_json::json!([]));
    }
}
