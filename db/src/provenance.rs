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

//! Provenance tracking and the DRed (Delete/Re-derive) algorithm
//! for incremental IDB maintenance.

use std::collections::{HashMap, HashSet};

use mangle_factstore::Value;

/// Identifies a fact by relation name + tuple.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FactKey {
    pub relation: String,
    pub tuple: Vec<Value>,
}

/// One derivation of an IDB fact: the rule that fired and the premise facts used.
#[derive(Debug, Clone)]
pub struct Derivation {
    pub rule_id: usize,
    pub premises: Vec<FactKey>,
}

/// Index from derived facts to their derivations, and from facts to their dependents.
pub struct ProvenanceIndex {
    /// For each IDB fact, all ways it can be derived.
    derivations: HashMap<FactKey, Vec<Derivation>>,
    /// Reverse index: for each fact F, which IDB facts have F as a premise.
    dependents: HashMap<FactKey, HashSet<FactKey>>,
}

impl ProvenanceIndex {
    pub fn new() -> Self {
        Self {
            derivations: HashMap::new(),
            dependents: HashMap::new(),
        }
    }

    /// Record a derivation: the fact `derived` was produced by `rule_id`
    /// using the given `premises`.
    pub fn record(&mut self, derived: FactKey, rule_id: usize, premises: Vec<FactKey>) {
        // Update reverse index
        for premise in &premises {
            self.dependents
                .entry(premise.clone())
                .or_default()
                .insert(derived.clone());
        }

        // Record the derivation
        self.derivations
            .entry(derived)
            .or_default()
            .push(Derivation { rule_id, premises });
    }

    /// Get all derivations for a fact.
    pub fn get_derivations(&self, fact: &FactKey) -> Option<&[Derivation]> {
        self.derivations.get(fact).map(|v| v.as_slice())
    }

    /// Get all IDB facts that depend on the given fact as a premise.
    pub fn get_dependents(&self, fact: &FactKey) -> Option<&HashSet<FactKey>> {
        self.dependents.get(fact)
    }

    /// DRed delete phase: given a retracted fact, find all IDB facts
    /// that may need to be deleted. Returns the set of facts to delete.
    pub fn delete_phase(&mut self, retracted: &FactKey) -> HashSet<FactKey> {
        let mut to_delete = HashSet::new();
        let mut worklist = vec![retracted.clone()];

        while let Some(fact) = worklist.pop() {
            // Find dependents of this fact
            let dependents = self.dependents.get(&fact).cloned().unwrap_or_default();

            for dependent in dependents {
                if to_delete.contains(&dependent) {
                    continue;
                }

                // Remove derivations that use the retracted fact
                if let Some(derivations) = self.derivations.get_mut(&dependent) {
                    derivations.retain(|d| !d.premises.contains(&fact));

                    if derivations.is_empty() {
                        // No remaining derivations — mark for deletion
                        to_delete.insert(dependent.clone());
                        worklist.push(dependent);
                    }
                }
            }
        }

        // Clean up reverse index for deleted facts
        for fact in &to_delete {
            self.derivations.remove(fact);
            self.dependents.remove(fact);
        }
        // Also clean up references from dependents to deleted facts
        for deps in self.dependents.values_mut() {
            deps.retain(|d| !to_delete.contains(d));
        }

        to_delete
    }
}

impl Default for ProvenanceIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(rel: &str, tuple: Vec<Value>) -> FactKey {
        FactKey {
            relation: rel.to_string(),
            tuple,
        }
    }

    #[test]
    fn test_provenance_basic() {
        let mut idx = ProvenanceIndex::new();

        let edge12 = fact("edge", vec![Value::Number(1), Value::Number(2)]);
        let reach12 = fact("reachable", vec![Value::Number(1), Value::Number(2)]);

        idx.record(reach12.clone(), 0, vec![edge12.clone()]);

        assert_eq!(idx.get_derivations(&reach12).unwrap().len(), 1);
        assert!(idx.get_dependents(&edge12).unwrap().contains(&reach12));
    }

    #[test]
    fn test_dred_simple() {
        let mut idx = ProvenanceIndex::new();

        let edge12 = fact("edge", vec![Value::Number(1), Value::Number(2)]);
        let edge23 = fact("edge", vec![Value::Number(2), Value::Number(3)]);
        let reach12 = fact("reachable", vec![Value::Number(1), Value::Number(2)]);
        let reach23 = fact("reachable", vec![Value::Number(2), Value::Number(3)]);
        let reach13 = fact("reachable", vec![Value::Number(1), Value::Number(3)]);

        // reachable(1,2) derived from edge(1,2)
        idx.record(reach12.clone(), 0, vec![edge12.clone()]);
        // reachable(2,3) derived from edge(2,3)
        idx.record(reach23.clone(), 0, vec![edge23.clone()]);
        // reachable(1,3) derived from reachable(1,2), edge(2,3)
        idx.record(
            reach13.clone(),
            1,
            vec![reach12.clone(), edge23.clone()],
        );

        // Retract edge(1,2) — should cascade to reachable(1,2) and reachable(1,3)
        let deleted = idx.delete_phase(&edge12);

        assert!(deleted.contains(&reach12));
        assert!(deleted.contains(&reach13));
        assert!(!deleted.contains(&reach23)); // unaffected
    }

    #[test]
    fn test_dred_multiple_derivations() {
        let mut idx = ProvenanceIndex::new();

        let a = fact("a", vec![Value::Number(1)]);
        let b = fact("b", vec![Value::Number(1)]);
        let c = fact("c", vec![Value::Number(1)]);

        // c(1) can be derived two ways: from a(1) and from b(1)
        idx.record(c.clone(), 0, vec![a.clone()]);
        idx.record(c.clone(), 1, vec![b.clone()]);

        // Retract a(1) — c(1) should survive because it has another derivation from b(1)
        let deleted = idx.delete_phase(&a);

        assert!(deleted.is_empty()); // c(1) survives
        assert_eq!(idx.get_derivations(&c).unwrap().len(), 1);
    }
}
