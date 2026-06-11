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

//! The `Database` abstraction: compiles a Mangle program, loads EDB facts,
//! executes the program, and serves queries from the resulting store.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Result, anyhow};
use fxhash::FxHashSet;
use mangle_analysis::{BoundsChecker, LoweringContext, Program, StratifiedProgram, rewrite_unit};
use mangle_ast::{self as ast, Arena};
use mangle_common::{Store, Value};
use mangle_interpreter::MemStore;
use mangle_ir::Ir;
use mangle_parse::Parser;
use sha2::{Digest, Sha256};

use crate::backend::IdbBackend;
use crate::provenance::ProvenanceIndex;
use crate::source::{EdbSource, Fingerprint};

/// How IDB (derived facts) are handled across restarts.
pub enum IdbMode {
    /// IDB is purely in-memory. Lost on drop, recomputed on open.
    InMemory,
    /// IDB is cached by the given backend. Loaded if valid, else recomputed.
    Cached(Arc<dyn IdbBackend>),
}

/// How IDB is recomputed when EDB changes.
pub enum RecomputeStrategy {
    /// Clear all IDB and recompute from scratch. Simple, always correct.
    Full,
    /// Track provenance during execution. On EDB mutation, use DRed
    /// to incrementally maintain IDB.
    Incremental,
}

/// Where the working store lives during and after execution.
pub enum StoreBackend {
    /// All facts in memory (HashMap-based MemStore).
    InMemory,
    /// Disk-backed store for large datasets (requires `disk` feature).
    Disk(PathBuf),
}

/// Configuration for opening a `Database`.
pub struct DatabaseConfig {
    pub name: String,
    pub source: String,
    pub edb_sources: Vec<Arc<dyn EdbSource>>,
    pub idb_mode: IdbMode,
    pub recompute: RecomputeStrategy,
    pub store_backend: StoreBackend,
}

struct DatabaseState {
    /// The working store — holds all facts (EDB + IDB) after execution.
    store: Box<dyn Store + Send + Sync>,
    edb_relations: HashSet<String>,
    idb_relations: HashSet<String>,
    edb_fingerprint: Option<Fingerprint>,
    program_hash: [u8; 32],
    /// Provenance data, populated only when RecomputeStrategy::Incremental.
    provenance: Option<ProvenanceIndex>,
}

/// A compiled and executed Mangle database.
///
/// Thread-safe: queries take a read lock, mutations take a write lock.
pub struct Database {
    config_name: String,
    config_source: String,
    edb_sources: Vec<Arc<dyn EdbSource>>,
    idb_mode_is_cached: bool,
    idb_backend: Option<Arc<dyn IdbBackend>>,
    recompute_is_incremental: bool,
    state: RwLock<DatabaseState>,
}

impl Database {
    /// Open a database: compile the program, load EDB, execute, serve queries.
    pub fn open(config: DatabaseConfig) -> Result<Self> {
        let program_hash = compute_program_hash(&config.source);
        let edb_fingerprint = compute_edb_fingerprint(&config.edb_sources)?;

        let (idb_mode_is_cached, idb_backend) = match &config.idb_mode {
            IdbMode::InMemory => (false, None),
            IdbMode::Cached(backend) => (true, Some(Arc::clone(backend))),
        };
        let recompute_is_incremental = matches!(config.recompute, RecomputeStrategy::Incremental);

        // Create the working store
        let mut store: Box<dyn Store + Send + Sync> = match &config.store_backend {
            StoreBackend::InMemory => Box::new(MemStore::new()),
            #[cfg(feature = "disk")]
            StoreBackend::Disk(path) => Box::new(crate::disk_store::DiskStore::open(path)?),
            #[cfg(not(feature = "disk"))]
            StoreBackend::Disk(_) => {
                return Err(anyhow!("Disk store requires the 'disk' feature"));
            }
        };

        // Try loading cached IDB
        let mut cache_hit = false;
        if let Some(ref backend) = idb_backend {
            if let Some((meta, snapshot)) = backend.load(&config.name)? {
                if meta.program_hash == program_hash
                    && edb_fingerprint
                        .as_ref()
                        .is_some_and(|fp| fp.0 == meta.edb_fingerprint)
                {
                    // Cache is valid — load EDB from sources, then IDB from cache
                    load_edb_into_store(&config.edb_sources, &mut *store)?;
                    for (rel_name, facts) in snapshot.relations {
                        store.create_relation(&rel_name);
                        for tuple in facts {
                            store.insert(&rel_name, tuple)?;
                        }
                    }
                    store.merge_deltas();
                    cache_hit = true;
                }
            }
        }

        let (edb_relations, idb_relations, provenance) = if !cache_hit {
            // Load EDB
            load_edb_into_store(&config.edb_sources, &mut *store)?;
            // Compile and execute
            full_recompute(&config.source, &mut *store)?
        } else {
            // We loaded from cache — figure out relation sets from the store
            // For now, we re-derive them by compiling (without executing)
            let (edb_rels, idb_rels) = extract_relation_names(&config.source)?;
            (edb_rels, idb_rels, None)
        };

        // Save to cache if needed and we just computed
        if !cache_hit {
            if let Some(ref backend) = idb_backend {
                if let Some(ref fp) = edb_fingerprint {
                    let meta = crate::backend::CacheMeta {
                        program_hash,
                        edb_fingerprint: fp.0.clone(),
                        created_at: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    };
                    let snapshot = extract_idb_snapshot(&*store, &idb_relations);
                    backend.save(&config.name, &meta, &snapshot)?;
                }
            }
        }

        let state = DatabaseState {
            store,
            edb_relations,
            idb_relations,
            edb_fingerprint,
            program_hash,
            provenance,
        };

        Ok(Database {
            config_name: config.name,
            config_source: config.source,
            edb_sources: config.edb_sources,
            idb_mode_is_cached: idb_mode_is_cached,
            idb_backend,
            recompute_is_incremental,
            state: RwLock::new(state),
        })
    }

    /// Query all tuples in a relation.
    pub fn query(&self, relation: &str) -> Result<Vec<Vec<Value>>> {
        let state = self.state.read().map_err(|_| anyhow!("lock poisoned"))?;
        let iter = state.store.scan(relation)?;
        Ok(iter.collect())
    }

    /// Insert a fact into an EDB relation and recompute IDB.
    pub fn insert(&self, relation: &str, tuple: Vec<Value>) -> Result<()> {
        let mut state = self.state.write().map_err(|_| anyhow!("lock poisoned"))?;
        state.store.insert(relation, tuple)?;
        state.store.merge_deltas();

        if state.edb_relations.contains(relation) {
            // EDB changed — recompute IDB
            self.recompute_idb(&mut state)?;
        }
        Ok(())
    }

    /// Retract a fact from an EDB relation and recompute IDB.
    pub fn retract(&self, relation: &str, tuple: &[Value]) -> Result<()> {
        let mut state = self.state.write().map_err(|_| anyhow!("lock poisoned"))?;
        state.store.retract(relation, tuple)?;

        if state.edb_relations.contains(relation) {
            self.recompute_idb(&mut state)?;
        }
        Ok(())
    }

    /// Create a batch for deferred recomputation.
    pub fn batch(&self) -> Batch<'_> {
        Batch { db: self }
    }

    /// Force reload from sources and recompute everything.
    pub fn reload(&self) -> Result<()> {
        let mut state = self.state.write().map_err(|_| anyhow!("lock poisoned"))?;

        // Clear everything
        let all_rels: Vec<String> = state.store.relation_names();
        for rel in &all_rels {
            state.store.clear(rel);
        }

        // Reload EDB
        load_edb_into_store(&self.edb_sources, &mut *state.store)?;

        // Recompute
        let (edb_rels, idb_rels, provenance) =
            full_recompute(&self.config_source, &mut *state.store)?;
        state.edb_relations = edb_rels;
        state.idb_relations = idb_rels;
        state.provenance = provenance;
        state.edb_fingerprint = compute_edb_fingerprint(&self.edb_sources)?;
        state.program_hash = compute_program_hash(&self.config_source);

        // Update cache
        if let Some(ref backend) = self.idb_backend {
            if let Some(ref fp) = state.edb_fingerprint {
                let meta = crate::backend::CacheMeta {
                    program_hash: state.program_hash,
                    edb_fingerprint: fp.0.clone(),
                    created_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                };
                let snapshot = extract_idb_snapshot(&*state.store, &state.idb_relations);
                backend.save(&self.config_name, &meta, &snapshot)?;
            }
        }

        Ok(())
    }

    /// Returns the names of all relations in the store.
    pub fn relation_names(&self) -> Result<Vec<String>> {
        let state = self.state.read().map_err(|_| anyhow!("lock poisoned"))?;
        Ok(state.store.relation_names())
    }

    fn recompute_idb(&self, state: &mut DatabaseState) -> Result<()> {
        // Clear IDB relations
        for rel in &state.idb_relations {
            state.store.clear(rel);
        }

        // Re-execute
        let (_, idb_rels, provenance) = full_recompute(&self.config_source, &mut *state.store)?;
        state.idb_relations = idb_rels;
        state.provenance = provenance;

        // Update cache
        if let Some(ref backend) = self.idb_backend {
            if let Some(ref fp) = state.edb_fingerprint {
                let meta = crate::backend::CacheMeta {
                    program_hash: state.program_hash,
                    edb_fingerprint: fp.0.clone(),
                    created_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                };
                let snapshot = extract_idb_snapshot(&*state.store, &state.idb_relations);
                backend.save(&self.config_name, &meta, &snapshot)?;
            }
        }

        Ok(())
    }
}

/// Batch operations with deferred recomputation.
pub struct Batch<'a> {
    db: &'a Database,
}

impl<'a> Batch<'a> {
    pub fn insert(&self, relation: &str, tuple: Vec<Value>) -> Result<()> {
        let mut state = self
            .db
            .state
            .write()
            .map_err(|_| anyhow!("lock poisoned"))?;
        state.store.insert(relation, tuple)?;
        state.store.merge_deltas();
        Ok(())
    }

    pub fn retract(&self, relation: &str, tuple: &[Value]) -> Result<()> {
        let mut state = self
            .db
            .state
            .write()
            .map_err(|_| anyhow!("lock poisoned"))?;
        state.store.retract(relation, tuple)?;
        Ok(())
    }

    /// Apply all batched changes and recompute IDB.
    pub fn commit(self) -> Result<()> {
        let mut state = self
            .db
            .state
            .write()
            .map_err(|_| anyhow!("lock poisoned"))?;
        self.db.recompute_idb(&mut state)
    }
}

// --- Internal helpers ---

fn compute_program_hash(source: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    hasher.finalize().into()
}

fn compute_edb_fingerprint(sources: &[Arc<dyn EdbSource>]) -> Result<Option<Fingerprint>> {
    let mut hasher = Sha256::new();
    for source in sources {
        match source.fingerprint()? {
            Some(fp) => hasher.update(&fp.0),
            None => return Ok(None), // Any source without fingerprint → always recompute
        }
    }
    Ok(Some(Fingerprint(hasher.finalize().to_vec())))
}

fn load_edb_into_store(sources: &[Arc<dyn EdbSource>], store: &mut dyn Store) -> Result<()> {
    for source in sources {
        let relations = source.relations()?;
        for rel_info in &relations {
            store.create_relation(&rel_info.name);
            let tuples = source.scan(&rel_info.name)?;
            for tuple in tuples {
                store.insert(&rel_info.name, tuple)?;
            }
        }
    }
    store.merge_deltas();
    Ok(())
}

fn extract_idb_snapshot(
    store: &dyn Store,
    idb_relations: &HashSet<String>,
) -> crate::backend::IdbSnapshot {
    let mut relations = Vec::new();
    for rel in idb_relations {
        if let Ok(iter) = store.scan(rel) {
            let facts: Vec<Vec<Value>> = iter.collect();
            if !facts.is_empty() {
                relations.push((rel.clone(), facts));
            }
        }
    }
    crate::backend::IdbSnapshot { relations }
}

/// Extract EDB and IDB relation names by compiling (but not executing) the program.
fn extract_relation_names(source: &str) -> Result<(HashSet<String>, HashSet<String>)> {
    let arena = Arena::new_with_global_interner();
    let (_ir, stratified) = compile_source(source, &arena)?;

    let mut edb_names = HashSet::new();
    for pred in stratified.extensional_preds() {
        if let Some(name) = arena.predicate_name(pred) {
            edb_names.insert(name.to_string());
        }
    }

    let mut idb_names = HashSet::new();
    for stratum in stratified.strata() {
        for pred in &stratum {
            if let Some(name) = arena.predicate_name(*pred) {
                idb_names.insert(name.to_string());
            }
        }
    }

    Ok((edb_names, idb_names))
}

fn compile_source<'a>(source: &str, arena: &'a Arena) -> Result<(Ir, StratifiedProgram<'a>)> {
    let mut parser = Parser::new(arena, source.as_bytes(), "source");
    parser.next_token().map_err(|e| anyhow!(e))?;
    let unit = parser.parse_unit()?;

    let rewritten_unit = rewrite_unit(arena, unit);
    let unit = &rewritten_unit;

    let mut program = Program::new(arena);
    let mut all_preds = FxHashSet::default();
    let mut idb_preds = FxHashSet::default();

    for clause in unit.clauses {
        program.add_clause(arena, clause);
        idb_preds.insert(clause.head.sym);
        all_preds.insert(clause.head.sym);
        for premise in clause.premises {
            if let ast::Term::Atom(atom) = premise {
                all_preds.insert(atom.sym);
            } else if let ast::Term::NegAtom(atom) = premise {
                all_preds.insert(atom.sym);
            }
        }
    }

    for pred in all_preds {
        if !idb_preds.contains(&pred) {
            program.ext_preds.push(pred);
        }
    }

    let stratified = program.stratify().map_err(|e| anyhow!(e))?;
    let ctx = LoweringContext::new(arena);
    let mut ir = ctx.lower_unit(unit);

    // Validate arity consistency and type bounds.
    let mut checker = BoundsChecker::new(&mut ir);
    checker.check()?;

    Ok((ir, stratified))
}

/// Compile and execute the program, transferring results into the target store.
///
/// Uses `mangle_driver::execute()` with a fresh MemStore, then copies
/// the resulting IDB facts into the target store.
fn full_recompute(
    source: &str,
    store: &mut dyn Store,
) -> Result<(HashSet<String>, HashSet<String>, Option<ProvenanceIndex>)> {
    let arena = Arena::new_with_global_interner();
    let (mut ir, stratified) = compile_source(source, &arena)?;

    // Extract predicate names before they go out of scope with the arena
    let mut edb_names = HashSet::new();
    for pred in stratified.extensional_preds() {
        if let Some(name) = arena.predicate_name(pred) {
            edb_names.insert(name.to_string());
        }
    }

    let mut idb_names = HashSet::new();
    for stratum in stratified.strata() {
        for pred in &stratum {
            if let Some(name) = arena.predicate_name(*pred) {
                idb_names.insert(name.to_string());
            }
        }
    }

    // Build a MemStore with EDB facts copied from the target store
    let mut exec_store = MemStore::new();
    for rel in &edb_names {
        exec_store.create_relation(rel);
        if let Ok(iter) = store.scan(rel) {
            for tuple in iter {
                exec_store.insert(rel, tuple)?;
            }
        }
    }
    exec_store.merge_deltas();

    // Execute using the driver
    let interpreter = mangle_driver::execute(&mut ir, &stratified, Box::new(exec_store))?;

    // Copy IDB results back into the target store
    for rel in &idb_names {
        store.create_relation(rel);
        if let Ok(iter) = interpreter.store().scan(rel) {
            for tuple in iter {
                store.insert(rel, tuple)?;
            }
        }
    }
    store.merge_deltas();

    Ok((edb_names, idb_names, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_database_basic() -> Result<()> {
        let config = DatabaseConfig {
            name: "test".to_string(),
            source: r#"
                p(1). p(2).
                q(X) :- p(X).
            "#
            .to_string(),
            edb_sources: vec![],
            idb_mode: IdbMode::InMemory,
            recompute: RecomputeStrategy::Full,
            store_backend: StoreBackend::InMemory,
        };

        let db = Database::open(config)?;

        let facts = db.query("q")?;
        let mut values: Vec<i64> = facts
            .iter()
            .map(|t| match t[0] {
                Value::Number(n) => n,
                _ => panic!("expected number"),
            })
            .collect();
        values.sort();
        assert_eq!(values, vec![1, 2]);

        Ok(())
    }

    #[test]
    fn test_database_reachability() -> Result<()> {
        let config = DatabaseConfig {
            name: "test".to_string(),
            source: r#"
                edge(1, 2). edge(2, 3). edge(3, 4).
                reachable(X, Y) :- edge(X, Y).
                reachable(X, Z) :- reachable(X, Y), edge(Y, Z).
            "#
            .to_string(),
            edb_sources: vec![],
            idb_mode: IdbMode::InMemory,
            recompute: RecomputeStrategy::Full,
            store_backend: StoreBackend::InMemory,
        };

        let db = Database::open(config)?;

        let facts = db.query("reachable")?;
        assert_eq!(facts.len(), 6); // (1,2),(1,3),(1,4),(2,3),(2,4),(3,4)

        Ok(())
    }

    #[test]
    fn test_database_insert_recompute() -> Result<()> {
        let config = DatabaseConfig {
            name: "test".to_string(),
            source: r#"
                q(X) :- p(X).
            "#
            .to_string(),
            edb_sources: vec![],
            idb_mode: IdbMode::InMemory,
            recompute: RecomputeStrategy::Full,
            store_backend: StoreBackend::InMemory,
        };

        let db = Database::open(config)?;

        // Initially q is empty (p has no facts from source, but we can insert)
        let facts = db.query("q")?;
        assert!(facts.is_empty());

        // Insert into EDB relation p
        db.insert("p", vec![Value::Number(42)])?;

        // q should now contain 42
        let facts = db.query("q")?;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0], vec![Value::Number(42)]);

        Ok(())
    }

    #[test]
    fn test_database_with_edb_source() -> Result<()> {
        // Create a programmatic EDB source
        struct TestSource {
            facts: Vec<Vec<Value>>,
        }
        impl crate::source::EdbSource for TestSource {
            fn name(&self) -> &str {
                "test_source"
            }
            fn relations(&self) -> Result<Vec<crate::source::RelationInfo>> {
                Ok(vec![crate::source::RelationInfo {
                    name: "edge".to_string(),
                    estimated_rows: self.facts.len(),
                }])
            }
            fn scan(&self, relation: &str) -> Result<Vec<Vec<Value>>> {
                if relation == "edge" {
                    Ok(self.facts.clone())
                } else {
                    Ok(vec![])
                }
            }
            fn fingerprint(&self) -> Result<Option<crate::source::Fingerprint>> {
                Ok(Some(crate::source::Fingerprint(vec![1, 2, 3])))
            }
        }

        let source = TestSource {
            facts: vec![
                vec![Value::Number(1), Value::Number(2)],
                vec![Value::Number(2), Value::Number(3)],
                vec![Value::Number(3), Value::Number(4)],
            ],
        };

        let config = DatabaseConfig {
            name: "test".to_string(),
            source: r#"
                reachable(X, Y) :- edge(X, Y).
                reachable(X, Z) :- reachable(X, Y), edge(Y, Z).
            "#
            .to_string(),
            edb_sources: vec![Arc::new(source)],
            idb_mode: IdbMode::InMemory,
            recompute: RecomputeStrategy::Full,
            store_backend: StoreBackend::InMemory,
        };

        let db = Database::open(config)?;

        let facts = db.query("reachable")?;
        assert_eq!(facts.len(), 6);

        // Check edge facts are accessible too
        let edges = db.query("edge")?;
        assert_eq!(edges.len(), 3);

        Ok(())
    }

    #[test]
    fn test_database_with_file_idb_cache() -> Result<()> {
        let cache_dir = tempfile::tempdir()?;
        let backend = Arc::new(crate::file_backend::FileIdbBackend::new(cache_dir.path()));

        // First open: computes and caches
        let config1 = DatabaseConfig {
            name: "cached_test".to_string(),
            source: r#"
                p(1). p(2). p(3).
                q(X) :- p(X).
            "#
            .to_string(),
            edb_sources: vec![],
            idb_mode: IdbMode::Cached(backend.clone()),
            recompute: RecomputeStrategy::Full,
            store_backend: StoreBackend::InMemory,
        };

        let db1 = Database::open(config1)?;
        let facts1 = db1.query("q")?;
        assert_eq!(facts1.len(), 3);
        drop(db1);

        // Verify cache files exist
        assert!(cache_dir.path().join("cached_test.meta.json").exists());
        assert!(cache_dir.path().join("cached_test.idb.mgr").exists());

        // Second open: should load from cache
        let config2 = DatabaseConfig {
            name: "cached_test".to_string(),
            source: r#"
                p(1). p(2). p(3).
                q(X) :- p(X).
            "#
            .to_string(),
            edb_sources: vec![],
            idb_mode: IdbMode::Cached(backend.clone()),
            recompute: RecomputeStrategy::Full,
            store_backend: StoreBackend::InMemory,
        };

        let db2 = Database::open(config2)?;
        let facts2 = db2.query("q")?;
        assert_eq!(facts2.len(), 3);

        Ok(())
    }

    #[test]
    fn test_database_retract_recompute() -> Result<()> {
        // Use an EDB source so that retracted facts don't come back from source text
        struct TestEdgeSource;
        impl crate::source::EdbSource for TestEdgeSource {
            fn name(&self) -> &str {
                "edges"
            }
            fn relations(&self) -> Result<Vec<crate::source::RelationInfo>> {
                Ok(vec![crate::source::RelationInfo {
                    name: "edge".to_string(),
                    estimated_rows: 2,
                }])
            }
            fn scan(&self, relation: &str) -> Result<Vec<Vec<Value>>> {
                if relation == "edge" {
                    Ok(vec![
                        vec![Value::Number(1), Value::Number(2)],
                        vec![Value::Number(2), Value::Number(3)],
                    ])
                } else {
                    Ok(vec![])
                }
            }
            fn fingerprint(&self) -> Result<Option<crate::source::Fingerprint>> {
                Ok(None) // Always recompute
            }
        }

        let config = DatabaseConfig {
            name: "test".to_string(),
            source: r#"
                reachable(X, Y) :- edge(X, Y).
                reachable(X, Z) :- reachable(X, Y), edge(Y, Z).
            "#
            .to_string(),
            edb_sources: vec![Arc::new(TestEdgeSource)],
            idb_mode: IdbMode::InMemory,
            recompute: RecomputeStrategy::Full,
            store_backend: StoreBackend::InMemory,
        };

        let db = Database::open(config)?;

        // Initially: reachable(1,2), reachable(2,3), reachable(1,3)
        let facts = db.query("reachable")?;
        assert_eq!(facts.len(), 3);

        // Retract edge(2,3) from the store
        db.retract("edge", &[Value::Number(2), Value::Number(3)])?;

        // After retract + recompute, only edge(1,2) remains → reachable(1,2) only
        let facts = db.query("reachable")?;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0], vec![Value::Number(1), Value::Number(2)]);

        Ok(())
    }

    #[test]
    fn test_database_reload() -> Result<()> {
        let config = DatabaseConfig {
            name: "test".to_string(),
            source: r#"
                p(1). p(2).
                q(X) :- p(X).
            "#
            .to_string(),
            edb_sources: vec![],
            idb_mode: IdbMode::InMemory,
            recompute: RecomputeStrategy::Full,
            store_backend: StoreBackend::InMemory,
        };

        let db = Database::open(config)?;

        let facts = db.query("q")?;
        assert_eq!(facts.len(), 2);

        // Reload should re-derive the same results
        db.reload()?;
        let facts = db.query("q")?;
        assert_eq!(facts.len(), 2);

        Ok(())
    }

    #[test]
    fn test_database_relation_names() -> Result<()> {
        let config = DatabaseConfig {
            name: "test".to_string(),
            source: r#"
                edge(1, 2).
                reachable(X, Y) :- edge(X, Y).
            "#
            .to_string(),
            edb_sources: vec![],
            idb_mode: IdbMode::InMemory,
            recompute: RecomputeStrategy::Full,
            store_backend: StoreBackend::InMemory,
        };

        let db = Database::open(config)?;

        let mut names = db.relation_names()?;
        names.sort();
        assert!(names.contains(&"edge".to_string()));
        assert!(names.contains(&"reachable".to_string()));

        Ok(())
    }

    #[test]
    fn test_database_empty_sources() -> Result<()> {
        let config = DatabaseConfig {
            name: "test".to_string(),
            source: r#"
                q(X) :- p(X).
            "#
            .to_string(),
            edb_sources: vec![],
            idb_mode: IdbMode::InMemory,
            recompute: RecomputeStrategy::Full,
            store_backend: StoreBackend::InMemory,
        };

        let db = Database::open(config)?;

        let facts = db.query("q")?;
        assert!(facts.is_empty());

        Ok(())
    }

    #[test]
    fn test_database_arity_mismatch_error() {
        // Opening a database with inconsistent predicate arity should fail.
        let config = DatabaseConfig {
            name: "test".to_string(),
            source: r#"
                p(1).
                p(2, 3).
            "#
            .to_string(),
            edb_sources: vec![],
            idb_mode: IdbMode::InMemory,
            recompute: RecomputeStrategy::Full,
            store_backend: StoreBackend::InMemory,
        };

        let result = Database::open(config);
        assert!(result.is_err(), "expected arity error");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("inconsistent arity"), "error should mention 'inconsistent arity': {}", msg);
    }
}
