use anyhow::{Result, anyhow};
use mangle_ast::Arena;
use mangle_common::Value;
use mangle_db::{
    Database, DatabaseConfig, FileEdbSource, FileIdbBackend, IdbMode, RecomputeStrategy,
    StoreBackend,
};
use mangle_interpreter::MemStore;
use mangle_proto::{Command, MutateCommand, MutationOpKind};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::mutations::MutationLog;
use crate::query::{ParsedQuery, filter_tuples, parse_query};

/// Shared handle to the [`ProgramStore`] used by the RPC service.
pub type AppState = std::sync::Arc<std::sync::RwLock<ProgramStore>>;

/// The replicated-fact ledger of one program (DISTRIBUTED_PLAN.md §3.6).
///
/// Tracks which facts came through the replicated write path, so that
/// (a) `MutationSummary`'s `num_skipped` accounting is defined by the
/// command sequence alone, and (b) snapshots can serialize exactly the
/// replicated facts, separately from facts loaded from external EDB
/// sources.
///
/// - `replicated`: facts inserted via commands and currently present.
/// - `retracted`: facts from program source / external EDB sources that
///   were retracted via a command. They must be re-retracted after a
///   snapshot restore, when the external sources are re-read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FactLedger {
    replicated: BTreeMap<String, BTreeSet<Vec<Value>>>,
    retracted: BTreeMap<String, BTreeSet<Vec<Value>>>,
}

impl FactLedger {
    /// Whether the ledger tracks nothing.
    pub fn is_empty(&self) -> bool {
        self.replicated.is_empty() && self.retracted.is_empty()
    }

    /// Iterate the replicated fact sets: (relation, tuples).
    pub fn replicated_sets(&self) -> impl Iterator<Item = (&String, &BTreeSet<Vec<Value>>)> {
        self.replicated.iter()
    }

    /// Iterate the retracted-external fact sets: (relation, tuples).
    pub fn retracted_sets(&self) -> impl Iterator<Item = (&String, &BTreeSet<Vec<Value>>)> {
        self.retracted.iter()
    }

    /// Whether the tuple is currently a replicated fact of the relation.
    pub fn replicated_contains(&self, relation: &str, tuple: &[Value]) -> bool {
        self.replicated
            .get(relation)
            .is_some_and(|set| set.contains(tuple))
    }

    /// Whether the tuple is an externally-sourced fact that was retracted
    /// by a command (and must be re-retracted after a restore).
    pub fn retracted_contains(&self, relation: &str, tuple: &[Value]) -> bool {
        self.retracted
            .get(relation)
            .is_some_and(|set| set.contains(tuple))
    }

    /// Record an applied insert: the tuple becomes a replicated fact and
    /// forgets any earlier retraction.
    fn note_insert(&mut self, relation: &str, tuple: &[Value]) {
        self.replicated
            .entry(relation.to_string())
            .or_default()
            .insert(tuple.to_vec());
        if let Some(set) = self.retracted.get_mut(relation) {
            set.retain(|t| t != tuple);
        }
    }

    /// Record an applied retract. A replicated fact simply stops being
    /// one; a fact from an external source is remembered as retracted so a
    /// restore can re-apply the retraction.
    fn note_retract(&mut self, relation: &str, tuple: &[Value]) {
        let replicated = self
            .replicated
            .get_mut(relation)
            .is_some_and(|set| set.remove(tuple));
        if !replicated {
            self.retracted
                .entry(relation.to_string())
                .or_default()
                .insert(tuple.to_vec());
        }
    }
}

/// The result of applying one [`Command`] to the store (see
/// `mangle_proto::ApplyOutcome`). Deterministic: applying the same
/// command to two stores in the same state yields the same outcome.
pub use mangle_proto::ApplyOutcome;

/// Metadata about a loaded program.
pub struct StoredProgram {
    pub source: String,
    pub predicates: Vec<String>,
    pub db: Database,
    /// Facts inserted/retracted through the command path (§3.6).
    pub ledger: FactLedger,
}

/// In-memory registry of named Mangle programs.
pub struct ProgramStore {
    programs: HashMap<String, StoredProgram>,
    programs_dir: Option<PathBuf>,
    edb_dir: Option<PathBuf>,
    idb_cache_dir: Option<PathBuf>,
    mutation_log: Option<MutationLog>,
}

/// Info returned when listing programs.
#[derive(Clone, Debug, PartialEq)]
pub struct ProgramInfo {
    pub name: String,
    pub predicates: Vec<String>,
}

/// Parse a query string, falling back to simple predicate extraction.
pub(crate) fn parse_query_lenient(query: &str) -> Result<ParsedQuery> {
    parse_query(query).or_else(|_| {
        let trimmed = query.trim();
        let paren = trimmed
            .find('(')
            .ok_or_else(|| anyhow!("invalid query: cannot extract predicate from '{}'", query))?;
        let name = trimmed[..paren].trim();
        if name.is_empty() {
            return Err(anyhow!("invalid query: empty predicate name"));
        }
        Ok(ParsedQuery {
            predicate: name.to_string(),
            args: vec![],
        })
    })
}

impl Default for ProgramStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgramStore {
    pub fn new() -> Self {
        Self {
            programs: HashMap::new(),
            programs_dir: None,
            edb_dir: None,
            idb_cache_dir: None,
            mutation_log: None,
        }
    }

    pub fn with_programs_dir(mut self, dir: PathBuf) -> Self {
        self.programs_dir = Some(dir);
        self
    }

    pub fn with_edb_dir(mut self, dir: PathBuf) -> Self {
        self.edb_dir = Some(dir);
        self
    }

    pub fn with_idb_cache_dir(mut self, dir: PathBuf) -> Self {
        self.idb_cache_dir = Some(dir);
        self
    }

    pub fn with_mutation_log(mut self, log: MutationLog) -> Self {
        self.mutation_log = Some(log);
        self
    }

    pub fn programs_dir(&self) -> Option<&PathBuf> {
        self.programs_dir.as_ref()
    }

    /// Build a DatabaseConfig for the given program name and source.
    fn make_db_config(&self, name: &str, source: &str) -> DatabaseConfig {
        let mut edb_sources: Vec<Arc<dyn mangle_db::EdbSource>> = vec![];

        // If edb_dir is configured and a per-program subdirectory exists, attach it
        if let Some(ref edb_dir) = self.edb_dir {
            let program_edb_dir = edb_dir.join(name);
            if program_edb_dir.is_dir() {
                edb_sources.push(Arc::new(FileEdbSource::new(name, program_edb_dir)));
            }
        }

        let idb_mode = if let Some(ref cache_dir) = self.idb_cache_dir {
            IdbMode::Cached(Arc::new(FileIdbBackend::new(cache_dir)))
        } else {
            IdbMode::InMemory
        };

        DatabaseConfig {
            name: name.to_string(),
            source: source.to_string(),
            edb_sources,
            idb_mode,
            recompute: RecomputeStrategy::Full,
            store_backend: StoreBackend::InMemory,
        }
    }

    /// Compile source, execute, and store the resulting Database.
    pub fn load(&mut self, name: &str, source: &str) -> Result<ProgramInfo> {
        let config = self.make_db_config(name, source);
        eprintln!(
            "[mangle] Loading program '{}': {} EDB sources, idb_mode={}, store_backend={}",
            name,
            config.edb_sources.len(),
            match config.idb_mode {
                IdbMode::InMemory => "InMemory",
                IdbMode::Cached(_) => "Cached",
            },
            match config.store_backend {
                StoreBackend::InMemory => "InMemory",
                StoreBackend::Disk(_) => "Disk",
            },
        );
        let db = Database::open(config)?;

        // Sort the predicate list: relation names come from the store's
        // HashMap and would otherwise be in iteration order, which differs
        // between two otherwise-identical stores (caught by the
        // determinism tests) and would leak into program-info responses.
        let mut predicates = db.relation_names()?;
        predicates.sort();

        let info = ProgramInfo {
            name: name.to_string(),
            predicates: predicates.clone(),
        };

        self.programs.insert(
            name.to_string(),
            StoredProgram {
                source: source.to_string(),
                predicates,
                db,
                // A (re-)load resets replicated-fact tracking: the store is
                // rebuilt from sources, so prior mutations are not reflected.
                // (Reload is a local admin op; it is disabled on the
                // replicated path, DISTRIBUTED_PLAN.md §3.1.)
                ledger: FactLedger::default(),
            },
        );

        Ok(info)
    }

    pub fn get(&self, name: &str) -> Option<&StoredProgram> {
        self.programs.get(name)
    }

    pub fn list(&self) -> Vec<ProgramInfo> {
        self.programs
            .iter()
            .map(|(name, prog)| ProgramInfo {
                name: name.clone(),
                predicates: prog.predicates.clone(),
            })
            .collect()
    }

    /// Remove a program from the in-memory store.
    pub fn remove(&mut self, name: &str) -> bool {
        self.programs.remove(name).is_some()
    }

    /// Reload a program from `{programs_dir}/{name}.mg`.
    pub fn reload(&mut self, name: &str) -> Result<ProgramInfo> {
        let dir = self
            .programs_dir
            .as_ref()
            .ok_or_else(|| anyhow!("no programs directory configured"))?;
        let path = dir.join(format!("{}.mg", name));
        let source = std::fs::read_to_string(&path)
            .map_err(|e| anyhow!("cannot read {}: {}", path.display(), e))?;
        self.load(name, &source)
    }

    /// Reload all `.mg` files from `programs_dir`.
    pub fn reload_all(&mut self) -> Result<Vec<ProgramInfo>> {
        let dir = self
            .programs_dir
            .as_ref()
            .ok_or_else(|| anyhow!("no programs directory configured"))?
            .clone();

        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .map_err(|e| anyhow!("cannot read directory {}: {}", dir.display(), e))?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "mg"))
            .collect();
        entries.sort_by_key(|e| e.file_name());

        self.programs.clear();

        let mut loaded = Vec::new();
        for entry in entries {
            let path = entry.path();
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
            let source = std::fs::read_to_string(&path)
                .map_err(|e| anyhow!("cannot read {}: {}", path.display(), e))?;
            loaded.push(self.load(&name, &source)?);
        }
        Ok(loaded)
    }

    /// Query a loaded program's database — fast scan + filter.
    pub fn execute_query(&self, name: &str, query: &str) -> Result<Vec<Vec<Value>>> {
        let prog = self
            .programs
            .get(name)
            .ok_or_else(|| anyhow!("program '{}' not found", name))?;

        let parsed = parse_query_lenient(query)?;
        let tuples = prog.db.query(&parsed.predicate)?;
        Ok(filter_tuples(tuples, &parsed))
    }

    /// Query a loaded program's database with structured argument patterns.
    ///
    /// `patterns[i]` is `None` for a wildcard (unbound) position and
    /// `Some(v)` for a constant that must equal the value at column `i`.
    /// Trailing entries may be omitted (treated as wildcards).
    pub fn execute_query_patterns(
        &self,
        name: &str,
        predicate: &str,
        patterns: &[Option<Value>],
    ) -> Result<Vec<Vec<Value>>> {
        let prog = self
            .programs
            .get(name)
            .ok_or_else(|| anyhow!("program '{}' not found", name))?;

        let tuples = prog.db.query(predicate)?;
        Ok(tuples
            .into_iter()
            .filter(|t| {
                patterns
                    .iter()
                    .enumerate()
                    .all(|(i, p)| p.as_ref().is_none_or(|v| t.get(i) == Some(v)))
            })
            .collect())
    }

    /// Apply one replicated command (DISTRIBUTED_PLAN.md §3.1, §3.4).
    ///
    /// This is the state machine's single write path: every mutation of
    /// the program registry or of a program's EDB facts goes through here,
    /// and applying the same command sequence to two stores in the same
    /// state produces identical state and identical [`ApplyOutcome`]s.
    /// `Mutate` commands apply all their ops in one `Database::batch()` —
    /// one IDB recompute per command, all-or-nothing semantics.
    pub fn apply(&mut self, cmd: &Command) -> Result<ApplyOutcome> {
        match cmd {
            Command::Mutate(m) => self.apply_mutate(m),
            Command::LoadProgram(l) => {
                let info = self.load(&l.name, &l.source)?;
                Ok(ApplyOutcome::Loaded {
                    name: info.name,
                    predicates: info.predicates,
                })
            }
            Command::DeleteProgram(d) => {
                if self.remove(&d.name) {
                    Ok(ApplyOutcome::Deleted)
                } else {
                    Err(anyhow!("program '{}' not found", d.name))
                }
            }
            Command::Noop => Ok(ApplyOutcome::Noop),
        }
    }

    /// Apply a `Mutate` command: validate ops (rejecting mutations of
    /// source-defined, re-derived relations), classify the rest against
    /// the current store state (skipping no-ops), apply the effective ops
    /// as one batch, then update the ledger and the (interim, pre-WAL)
    /// mutation log.
    fn apply_mutate(&mut self, cmd: &MutateCommand) -> Result<ApplyOutcome> {
        let ProgramStore {
            programs,
            mutation_log,
            ..
        } = self;

        let prog = programs
            .get_mut(&cmd.program)
            .ok_or_else(|| anyhow!("program '{}' not found", cmd.program))?;

        // Validate before touching anything (atomic semantics): relations
        // defined by the program source are re-derived on every recompute,
        // so mutations to them cannot persist — reject rather than lose
        // the write silently.
        for op in &cmd.ops {
            if !prog.db.is_mutable_relation(&op.relation) {
                return Err(anyhow!(
                    "relation '{}' is defined by the program source; \
                     only EDB relations can be mutated",
                    op.relation
                ));
            }
        }

        // Membership snapshot of every touched relation, from the current
        // store state. Ops are evaluated against this set as they would be
        // against the store, so duplicates within the command and
        // insert/retract interleavings are classified exactly.
        let mut membership: HashMap<String, HashSet<Vec<Value>>> = HashMap::new();
        for op in &cmd.ops {
            if !membership.contains_key(&op.relation) {
                let tuples: HashSet<Vec<Value>> = prog.db.query(&op.relation)?.into_iter().collect();
                membership.insert(op.relation.clone(), tuples);
            }
        }

        let mut applied: Vec<&mangle_proto::MutationOp> = Vec::new();
        let mut num_skipped: u64 = 0;
        for op in &cmd.ops {
            let set = membership.get_mut(&op.relation).expect("relation pre-seeded");
            let present = set.contains(&op.tuple);
            match op.kind {
                MutationOpKind::Insert => {
                    if present {
                        num_skipped += 1; // already a fact
                    } else {
                        set.insert(op.tuple.clone());
                        applied.push(op);
                    }
                }
                MutationOpKind::Retract => {
                    if present {
                        set.remove(&op.tuple);
                        applied.push(op); // retract-of-absent is a no-op (§7.4)
                    } else {
                        num_skipped += 1;
                    }
                }
            }
        }

        if !applied.is_empty() {
            let batch = prog.db.batch();
            for op in &applied {
                match op.kind {
                    MutationOpKind::Insert => batch.insert(&op.relation, op.tuple.clone())?,
                    MutationOpKind::Retract => batch.retract(&op.relation, &op.tuple)?,
                }
            }
            batch.commit()?; // single IDB recompute for the whole command

            for op in &applied {
                match op.kind {
                    MutationOpKind::Insert => prog.ledger.note_insert(&op.relation, &op.tuple),
                    MutationOpKind::Retract => prog.ledger.note_retract(&op.relation, &op.tuple),
                }
                if let Some(log) = mutation_log {
                    match op.kind {
                        MutationOpKind::Insert => {
                            log.append_insert(&cmd.program, &op.relation, &op.tuple)?
                        }
                        MutationOpKind::Retract => {
                            log.append_retract(&cmd.program, &op.relation, &op.tuple)?
                        }
                    }
                }
            }
        }

        Ok(ApplyOutcome::Mutated {
            num_applied: applied.len() as u64,
            num_skipped,
        })
    }

    /// Insert a fact into a program's EDB and recompute IDB.
    ///
    /// Convenience wrapper: routes through [`ProgramStore::apply`] as a
    /// single-op `Mutate` command, so all writes share one path.
    pub fn insert_fact(&mut self, name: &str, relation: &str, tuple: Vec<Value>) -> Result<()> {
        self.apply(&mutate_one(name, relation, tuple, MutationOpKind::Insert))
            .map(|_| ())
    }

    /// Retract a fact from a program's EDB and recompute IDB.
    ///
    /// Convenience wrapper: routes through [`ProgramStore::apply`] as a
    /// single-op `Mutate` command (retract-of-absent is a no-op, §7.4).
    pub fn retract_fact(&mut self, name: &str, relation: &str, tuple: &[Value]) -> Result<()> {
        self.apply(&mutate_one(
            name,
            relation,
            tuple.to_vec(),
            MutationOpKind::Retract,
        ))
        .map(|_| ())
    }

    /// The FSM state in snapshot form (DISTRIBUTED_PLAN.md §3.6): program
    /// registry (name → source) plus the replicated-fact ledgers. IDB is
    /// derived and excluded; external-EDB facts are per-node inputs and
    /// excluded (re-read from local config on restore, with retracted-
    /// external entries re-applied afterwards).
    pub fn snapshot_state(&self) -> mangle_proto::Snapshot {
        mangle_proto::Snapshot {
            programs: self
                .programs
                .iter()
                .map(|(name, prog)| mangle_proto::SnapshotProgram {
                    name: name.clone(),
                    source: prog.source.clone(),
                    ledger: ledger_to_snapshot(&prog.ledger),
                })
                .collect(),
        }
    }

    /// Replace the entire store state from a snapshot: clear everything,
    /// then rebuild each program from its source and replay its ledger
    /// (replicated facts inserted, retracted-external facts re-retracted)
    /// in one batch per program — one IDB recompute each.
    pub fn install_snapshot_state(&mut self, snap: &mangle_proto::Snapshot) -> Result<()> {
        self.programs.clear();
        for prog in &snap.programs {
            let info = self.load(&prog.name, &prog.source)?;
            let ledger = ledger_from_snapshot(&prog.ledger);
            let stored = self
                .programs
                .get_mut(&info.name)
                .expect("just loaded");
            if !ledger.is_empty() {
                let batch = stored.db.batch();
                for (relation, tuples) in ledger.replicated_sets() {
                    for tuple in tuples {
                        batch.insert(relation, tuple.to_vec())?;
                    }
                }
                for (relation, tuples) in ledger.retracted_sets() {
                    for tuple in tuples {
                        batch.retract(relation, tuple)?;
                    }
                }
                batch.commit()?;
            }
            stored.ledger = ledger;
        }
        Ok(())
    }

    /// Canonical, order-independent view of the whole FSM state: program
    /// registry (name → source), every relation's facts, and the fact
    /// ledgers. Used by the determinism tests; the snapshot serializer
    /// (Phase 1) builds on the same view.
    pub fn canonical_state(&self) -> BTreeMap<String, CanonicalProgramState> {
        self.programs
            .iter()
            .map(|(name, prog)| {
                let mut relations = BTreeMap::new();
                // Relation-name or query errors here would mean a broken
                // store; treat them as empty rather than failing the view.
                for rel in prog.db.relation_names().unwrap_or_default() {
                    let tuples: BTreeSet<Vec<Value>> =
                        prog.db.query(&rel).unwrap_or_default().into_iter().collect();
                    relations.insert(rel, tuples);
                }
                (
                    name.clone(),
                    CanonicalProgramState {
                        source: prog.source.clone(),
                        relations,
                        ledger: prog.ledger.clone(),
                    },
                )
            })
            .collect()
    }
}

/// One program's canonical state (see [`ProgramStore::canonical_state`]).
#[derive(Clone, Debug, PartialEq)]
pub struct CanonicalProgramState {
    pub source: String,
    pub relations: BTreeMap<String, BTreeSet<Vec<Value>>>,
    pub ledger: FactLedger,
}

/// Convert a [`FactLedger`] to its snapshot (proto-mirror) form.
fn ledger_to_snapshot(ledger: &FactLedger) -> mangle_proto::SnapshotLedger {
    use mangle_proto::{SnapshotLedgerEntry, SnapshotLedger as SL};
    fn entries(map: &BTreeMap<String, BTreeSet<Vec<Value>>>) -> Vec<SnapshotLedgerEntry> {
        map.iter()
            .flat_map(|(relation, tuples)| {
                tuples.iter().map(move |tuple| SnapshotLedgerEntry {
                    relation: relation.clone(),
                    tuple: tuple.clone(),
                })
            })
            .collect()
    }
    SL {
        replicated: entries(&ledger.replicated),
        retracted: entries(&ledger.retracted),
    }
}

/// Convert a snapshot (proto-mirror) ledger back into a [`FactLedger`].
fn ledger_from_snapshot(sl: &mangle_proto::SnapshotLedger) -> FactLedger {
    let mut ledger = FactLedger::default();
    for e in &sl.replicated {
        ledger
            .replicated
            .entry(e.relation.clone())
            .or_default()
            .insert(e.tuple.clone());
    }
    for e in &sl.retracted {
        ledger
            .retracted
            .entry(e.relation.clone())
            .or_default()
            .insert(e.tuple.clone());
    }
    ledger
}

/// Build a single-op `Mutate` command (helper for the convenience
/// wrappers and tests).
fn mutate_one(
    program: &str,
    relation: &str,
    tuple: Vec<Value>,
    kind: MutationOpKind,
) -> Command {
    Command::Mutate(MutateCommand {
        program: program.to_string(),
        ops: vec![mangle_proto::MutationOp {
            kind,
            relation: relation.to_string(),
            tuple,
        }],
    })
}

/// Compile and execute ephemeral source, returning results for the queried relation.
pub fn eval_source(source: &str, query: Option<&str>) -> Result<Vec<Vec<Value>>> {
    eval_source_multi(&[source], query)
}

/// Compile and execute multiple source units, returning results for the queried relation.
pub fn eval_source_multi(sources: &[&str], query: Option<&str>) -> Result<Vec<Vec<Value>>> {
    Ok(eval_source_relations(sources, query)?
        .into_iter()
        .flat_map(|(_, rows)| rows)
        .collect())
}

/// Like [`eval_source_multi`], but keeps results grouped by relation so that
/// they can be streamed as per-relation fact batches (used by the RPC API).
pub fn eval_source_relations(
    sources: &[&str],
    query: Option<&str>,
) -> Result<Vec<(String, Vec<Vec<Value>>)>> {
    let arena = Arena::new_with_global_interner();
    let (mut ir, stratified) = mangle_driver::compile_units(sources, &arena)?;
    let store = Box::new(MemStore::new());
    let interpreter = mangle_driver::execute(&mut ir, &stratified, store)?;

    if let Some(q) = query {
        let parsed = parse_query_lenient(q)?;
        let predicate = parsed.predicate.clone();
        let tuples: Vec<Vec<Value>> = interpreter.store().scan(&predicate)?.collect();
        Ok(vec![(predicate, filter_tuples(tuples, &parsed))])
    } else {
        let mut all = Vec::new();
        for name in interpreter.store().relation_names() {
            let tuples: Vec<Vec<Value>> = interpreter.store().scan(&name)?.collect();
            all.push((name, tuples));
        }
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_and_remove() {
        let mut store = ProgramStore::new();
        let source = r#"p(1). p(2)."#;
        store.load("test", source).unwrap();
        assert!(store.get("test").is_some());
        assert_eq!(store.list().len(), 1);

        assert!(store.remove("test"));
        assert!(store.get("test").is_none());
        assert_eq!(store.list().len(), 0);

        // Removing again returns false
        assert!(!store.remove("test"));
    }

    #[test]
    fn test_reload_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mg_path = dir.path().join("sample.mg");
        std::fs::write(&mg_path, "greeting(\"hello\").").unwrap();

        let mut store = ProgramStore::new().with_programs_dir(dir.path().to_path_buf());

        let info = store.reload("sample").unwrap();
        assert_eq!(info.name, "sample");
        assert!(store.get("sample").is_some());
    }

    #[test]
    fn test_reload_all() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mg"), "p(1).").unwrap();
        std::fs::write(dir.path().join("b.mg"), "q(2).").unwrap();

        let mut store = ProgramStore::new().with_programs_dir(dir.path().to_path_buf());

        let loaded = store.reload_all().unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(store.get("a").is_some());
        assert!(store.get("b").is_some());
    }

    #[test]
    fn test_reload_no_dir_errors() {
        let mut store = ProgramStore::new();
        let err = store.reload("test").unwrap_err();
        assert!(err.to_string().contains("no programs directory"));
    }

    #[test]
    fn test_execute_query_with_filtering() {
        let mut store = ProgramStore::new();
        let source = r#"
            route("GET", "/api", "api_handler").
            route("POST", "/api", "api_post").
            route("GET", "/home", "home_handler").
        "#;
        store.load("routes", source).unwrap();

        // All routes
        let all = store.execute_query("routes", "route(M, P, H)").unwrap();
        assert_eq!(all.len(), 3);

        // Only GET routes
        let gets = store
            .execute_query("routes", r#"route("GET", P, H)"#)
            .unwrap();
        assert_eq!(gets.len(), 2);

        // Specific route
        let specific = store
            .execute_query("routes", r#"route("POST", "/api", H)"#)
            .unwrap();
        assert_eq!(specific.len(), 1);
    }

    #[test]
    fn test_insert_and_query() {
        let mut store = ProgramStore::new();
        let source = r#"q(X) :- p(X)."#;
        store.load("test", source).unwrap();

        // Initially empty
        let results = store.execute_query("test", "q(X)").unwrap();
        assert!(results.is_empty());

        // Insert into EDB relation p
        store
            .insert_fact("test", "p", vec![Value::Number(42)])
            .unwrap();

        // q should now contain 42
        let results = store.execute_query("test", "q(X)").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], vec![Value::Number(42)]);
    }

    #[test]
    fn test_retract_and_query() {
        let mut store = ProgramStore::new();
        let source = r#"
            q(X) :- p(X).
        "#;
        store.load("test", source).unwrap();

        store
            .insert_fact("test", "p", vec![Value::Number(1)])
            .unwrap();
        store
            .insert_fact("test", "p", vec![Value::Number(2)])
            .unwrap();

        let results = store.execute_query("test", "q(X)").unwrap();
        assert_eq!(results.len(), 2);

        store
            .retract_fact("test", "p", &[Value::Number(1)])
            .unwrap();

        let results = store.execute_query("test", "q(X)").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], vec![Value::Number(2)]);
    }

    #[test]
    fn test_with_edb_dir() {
        let programs_dir = tempfile::tempdir().unwrap();
        let edb_dir = tempfile::tempdir().unwrap();

        // Create program source
        std::fs::write(
            programs_dir.path().join("routes.mg"),
            r#"reachable(X, Y) :- edge(X, Y).
               reachable(X, Z) :- reachable(X, Y), edge(Y, Z)."#,
        )
        .unwrap();

        // Create EDB subdirectory with a .mg file
        let routes_edb = edb_dir.path().join("routes");
        std::fs::create_dir(&routes_edb).unwrap();
        std::fs::write(routes_edb.join("data.mg"), "edge(1, 2).\nedge(2, 3).\n").unwrap();

        let mut store = ProgramStore::new()
            .with_programs_dir(programs_dir.path().to_path_buf())
            .with_edb_dir(edb_dir.path().to_path_buf());

        let info = store.reload("routes").unwrap();
        assert_eq!(info.name, "routes");

        let results = store.execute_query("routes", "reachable(X, Y)").unwrap();
        assert_eq!(results.len(), 3); // (1,2), (2,3), (1,3)
    }

    #[test]
    fn test_durable_mutations_roundtrip() {
        use std::collections::HashSet;

        let programs_dir = tempfile::tempdir().unwrap();
        let edb_dir = tempfile::tempdir().unwrap();

        // Program with a rule over an EDB relation
        std::fs::write(
            programs_dir.path().join("runtime.mg"),
            "running(Name) :- container(Name, \"running\").\n",
        )
        .unwrap();

        // Create the EDB subdirectory (required for FileEdbSource)
        std::fs::create_dir(edb_dir.path().join("runtime")).unwrap();

        let mut persist = HashSet::new();
        persist.insert("runtime".to_string());
        let log = MutationLog::new(edb_dir.path().to_path_buf(), persist);

        // Phase 1: Insert facts with mutation log
        {
            let mut store = ProgramStore::new()
                .with_programs_dir(programs_dir.path().to_path_buf())
                .with_edb_dir(edb_dir.path().to_path_buf())
                .with_mutation_log(log);

            store.reload("runtime").unwrap();

            store
                .insert_fact(
                    "runtime",
                    "container",
                    vec![
                        Value::String("web".to_string()),
                        Value::String("running".to_string()),
                    ],
                )
                .unwrap();

            store
                .insert_fact(
                    "runtime",
                    "container",
                    vec![
                        Value::String("db".to_string()),
                        Value::String("running".to_string()),
                    ],
                )
                .unwrap();

            // Retract one
            store
                .retract_fact(
                    "runtime",
                    "container",
                    &[
                        Value::String("web".to_string()),
                        Value::String("running".to_string()),
                    ],
                )
                .unwrap();
        }

        // Phase 2: "Restart" — create a new store, reload from disk
        {
            let mut store = ProgramStore::new()
                .with_programs_dir(programs_dir.path().to_path_buf())
                .with_edb_dir(edb_dir.path().to_path_buf());

            store.reload("runtime").unwrap();

            // Only "db" should remain ("web" was retracted)
            let results = store.execute_query("runtime", "running(Name)").unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0][0], Value::String("db".to_string()));

            // The raw container facts should also reflect the retraction
            let containers = store
                .execute_query("runtime", "container(Name, Status)")
                .unwrap();
            assert_eq!(containers.len(), 1);
            assert_eq!(containers[0][0], Value::String("db".to_string()));
        }
    }

    // --- Command path (Phase 0: FSM extraction) ---------------------------

    use mangle_proto::{Command, LoadProgramCommand, MutateCommand, MutationOp, MutationOpKind};

    fn mutate_cmd(program: &str, ops: Vec<MutationOp>) -> Command {
        Command::Mutate(MutateCommand {
            program: program.to_string(),
            ops,
        })
    }

    fn op(kind: MutationOpKind, relation: &str, tuple: Vec<Value>) -> MutationOp {
        MutationOp {
            kind,
            relation: relation.to_string(),
            tuple,
        }
    }

    #[test]
    fn test_apply_mutate_batch_and_recompute() {
        let mut store = ProgramStore::new();
        store
            .apply(&Command::LoadProgram(LoadProgramCommand {
                name: "t".into(),
                source: "q(X) :- p(X).".into(),
            }))
            .unwrap();

        let outcome = store
            .apply(&mutate_cmd(
                "t",
                vec![
                    op(MutationOpKind::Insert, "p", vec![Value::Number(1)]),
                    op(MutationOpKind::Insert, "p", vec![Value::Number(2)]),
                ],
            ))
            .unwrap();
        assert_eq!(
            outcome,
            ApplyOutcome::Mutated {
                num_applied: 2,
                num_skipped: 0
            }
        );

        // One batch → one recompute: both facts visible in the derived q.
        let qs = store.execute_query("t", "q(X)").unwrap();
        assert_eq!(qs.len(), 2);
    }

    #[test]
    fn test_apply_mutate_skips_duplicates() {
        let mut store = ProgramStore::new();
        store.load("t", "q(X) :- p(X).").unwrap();
        // Seed one fact through the command path.
        store
            .apply(&mutate_cmd(
                "t",
                vec![op(MutationOpKind::Insert, "p", vec![Value::Number(1)])],
            ))
            .unwrap();

        // Duplicate insert within one command: first applied, second skipped.
        let outcome = store
            .apply(&mutate_cmd(
                "t",
                vec![
                    op(MutationOpKind::Insert, "p", vec![Value::Number(2)]),
                    op(MutationOpKind::Insert, "p", vec![Value::Number(2)]),
                ],
            ))
            .unwrap();
        assert_eq!(
            outcome,
            ApplyOutcome::Mutated {
                num_applied: 1,
                num_skipped: 1
            }
        );

        // Insert of an already-present fact: skipped.
        let outcome = store
            .apply(&mutate_cmd(
                "t",
                vec![op(MutationOpKind::Insert, "p", vec![Value::Number(1)])],
            ))
            .unwrap();
        assert_eq!(
            outcome,
            ApplyOutcome::Mutated {
                num_applied: 0,
                num_skipped: 1
            }
        );
    }

    #[test]
    fn test_apply_mutate_retract_semantics() {
        let mut store = ProgramStore::new();
        store.load("t", "q(X) :- p(X).").unwrap();

        // Retract of an absent fact: non-error no-op (§7.4), counted skipped.
        let outcome = store
            .apply(&mutate_cmd(
                "t",
                vec![op(MutationOpKind::Retract, "p", vec![Value::Number(99)])],
            ))
            .unwrap();
        assert_eq!(
            outcome,
            ApplyOutcome::Mutated {
                num_applied: 0,
                num_skipped: 1
            }
        );

        // Insert, then retract: the fact was replicated, so the retract
        // removes it from the replicated set (not retracted-external).
        store
            .apply(&mutate_cmd(
                "t",
                vec![op(MutationOpKind::Insert, "p", vec![Value::Number(1)])],
            ))
            .unwrap();
        let outcome = store
            .apply(&mutate_cmd(
                "t",
                vec![op(MutationOpKind::Retract, "p", vec![Value::Number(1)])],
            ))
            .unwrap();
        assert_eq!(
            outcome,
            ApplyOutcome::Mutated {
                num_applied: 1,
                num_skipped: 0
            }
        );
        let state = store.canonical_state();
        let ledger = &state.get("t").unwrap().ledger;
        assert!(!ledger.replicated_contains("p", &[Value::Number(1)]));
        assert!(!ledger.retracted_contains("p", &[Value::Number(1)]));
    }

    #[test]
    fn test_apply_mutate_retract_of_external_fact() {
        // A fact loaded from an EDB *source file* (not the program source):
        // retracting it is applied and recorded as retracted-external in
        // the ledger, so a future snapshot restore re-applies it.
        let programs_dir = tempfile::tempdir().unwrap();
        let edb_dir = tempfile::tempdir().unwrap();

        std::fs::write(
            programs_dir.path().join("t.mg"),
            "q(X) :- p(X).\n",
        )
        .unwrap();
        let t_edb = edb_dir.path().join("t");
        std::fs::create_dir(&t_edb).unwrap();
        std::fs::write(t_edb.join("data.mg"), "p(1). p(2).\n").unwrap();

        let mut store = ProgramStore::new()
            .with_programs_dir(programs_dir.path().to_path_buf())
            .with_edb_dir(edb_dir.path().to_path_buf());
        store.reload("t").unwrap();

        let outcome = store
            .apply(&mutate_cmd(
                "t",
                vec![op(MutationOpKind::Retract, "p", vec![Value::Number(1)])],
            ))
            .unwrap();
        assert_eq!(
            outcome,
            ApplyOutcome::Mutated {
                num_applied: 1,
                num_skipped: 0
            }
        );

        let state = store.canonical_state();
        let ledger = &state.get("t").unwrap().ledger;
        assert!(ledger.retracted_contains("p", &[Value::Number(1)]));
        assert!(!ledger.replicated_contains("p", &[Value::Number(1)]));

        // The fact is really gone from the store.
        let qs = store.execute_query("t", "q(X)").unwrap();
        assert_eq!(qs, vec![vec![Value::Number(2)]]);
    }

    #[test]
    fn test_apply_mutate_rejects_source_defined_relation() {
        // `p` is defined by a fact in the program source: mutations cannot
        // persist (the recompute re-derives p(1)), so they are rejected —
        // previously they were silently accepted and lost.
        let mut store = ProgramStore::new();
        store.load("t", "p(1). q(X) :- p(X).").unwrap();

        let err = store
            .apply(&mutate_cmd(
                "t",
                vec![op(MutationOpKind::Insert, "p", vec![Value::Number(3)])],
            ))
            .unwrap_err();
        assert!(err.to_string().contains("defined by the program source"));

        // Same for retract, and the whole command fails atomically.
        let err = store
            .apply(&mutate_cmd(
                "t",
                vec![
                    op(MutationOpKind::Insert, "r", vec![Value::Number(1)]),
                    op(MutationOpKind::Retract, "p", vec![Value::Number(1)]),
                ],
            ))
            .unwrap_err();
        assert!(err.to_string().contains("defined by the program source"));

        // State unchanged: nothing from the failed command was applied.
        let state = store.canonical_state();
        let t = state.get("t").unwrap();
        assert!(!t.relations.contains_key("r"));
        assert_eq!(t.relations["p"].len(), 1);
    }

    #[test]
    fn test_apply_mutate_interleaving() {
        // Insert then retract of the same tuple within one command: net
        // effect nothing, both counted applied (each changed the store).
        let mut store = ProgramStore::new();
        store.load("t", "q(X) :- p(X).").unwrap();
        store
            .apply(&mutate_cmd(
                "t",
                vec![op(MutationOpKind::Insert, "p", vec![Value::Number(1)])],
            ))
            .unwrap();

        let outcome = store
            .apply(&mutate_cmd(
                "t",
                vec![
                    op(MutationOpKind::Insert, "p", vec![Value::Number(5)]),
                    op(MutationOpKind::Retract, "p", vec![Value::Number(5)]),
                ],
            ))
            .unwrap();
        assert_eq!(
            outcome,
            ApplyOutcome::Mutated {
                num_applied: 2,
                num_skipped: 0
            }
        );
        let ps = store.execute_query("t", "p(X)").unwrap();
        assert_eq!(ps.len(), 1); // only the seeded fact remains
    }

    #[test]
    fn test_apply_mutate_unknown_program() {
        let mut store = ProgramStore::new();
        let err = store
            .apply(&mutate_cmd(
                "nope",
                vec![op(MutationOpKind::Insert, "p", vec![Value::Number(1)])],
            ))
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_apply_load_delete_noop() {
        let mut store = ProgramStore::new();

        let outcome = store
            .apply(&Command::LoadProgram(LoadProgramCommand {
                name: "t".into(),
                source: "p(1).".into(),
            }))
            .unwrap();
        assert!(
            matches!(outcome, ApplyOutcome::Loaded { ref name, .. } if name == "t")
        );

        assert_eq!(store.apply(&Command::Noop).unwrap(), ApplyOutcome::Noop);

        let outcome = store
            .apply(&Command::DeleteProgram(mangle_proto::DeleteProgramCommand {
                name: "t".into(),
            }))
            .unwrap();
        assert_eq!(outcome, ApplyOutcome::Deleted);
        assert!(store.get("t").is_none());

        // Deleting again is an error (same as the RPC's NotFound).
        assert!(store
            .apply(&Command::DeleteProgram(mangle_proto::DeleteProgramCommand {
                name: "t".into()
            }))
            .is_err());
    }

    #[test]
    fn test_apply_command_proto_roundtrip_through_store() {
        // A command that survives the proto roundtrip applies identically.
        let mut store = ProgramStore::new();
        store.load("t", "q(X) :- p(X).").unwrap();

        let cmd = mutate_cmd(
            "t",
            vec![op(
                MutationOpKind::Insert,
                "p",
                vec![Value::String("web".into())],
            )],
        );
        let wire = cmd.to_proto();
        let decoded = Command::from_proto(&wire).unwrap();
        assert_eq!(cmd, decoded);

        store.apply(&decoded).unwrap();
        let qs = store.execute_query("t", "q(X)").unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0][0], Value::String("web".into()));
    }
}
