use anyhow::{Result, anyhow};
use mangle_ast::Arena;
use mangle_factstore::Value;
use mangle_interpreter::MemStore;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::query::{ParsedQuery, filter_tuples, parse_query};

/// Metadata about a loaded program (stored without compiled form).
pub struct StoredProgram {
    pub source: String,
    pub predicates: Vec<String>,
}

/// In-memory registry of named Mangle programs.
pub struct ProgramStore {
    programs: HashMap<String, StoredProgram>,
    programs_dir: Option<PathBuf>,
}

/// Info returned when listing programs.
#[derive(Clone, Debug)]
pub struct ProgramInfo {
    pub name: String,
    pub predicates: Vec<String>,
}

/// Parse a query string, falling back to simple predicate extraction.
fn parse_query_lenient(query: &str) -> Result<ParsedQuery> {
    parse_query(query).or_else(|_| {
        let trimmed = query.trim();
        let paren = trimmed.find('(')
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

impl ProgramStore {
    pub fn new() -> Self {
        Self {
            programs: HashMap::new(),
            programs_dir: None,
        }
    }

    pub fn with_programs_dir(mut self, dir: PathBuf) -> Self {
        self.programs_dir = Some(dir);
        self
    }

    pub fn programs_dir(&self) -> Option<&PathBuf> {
        self.programs_dir.as_ref()
    }

    /// Compile source to extract predicate names, then store source + names.
    /// The compiled form is discarded (avoids lifetime issues with Arena-borrowing types).
    pub fn load(&mut self, name: &str, source: &str) -> Result<ProgramInfo> {
        let arena = Arena::new_with_global_interner();
        let (_ir, stratified) = mangle_driver::compile(source, &arena)?;

        // Collect IDB predicate names from strata
        let mut predicates = Vec::new();
        for stratum in stratified.strata() {
            for pred in &stratum {
                if let Some(pred_name) = arena.predicate_name(*pred) {
                    predicates.push(pred_name.to_string());
                }
            }
        }

        let info = ProgramInfo {
            name: name.to_string(),
            predicates: predicates.clone(),
        };

        self.programs.insert(
            name.to_string(),
            StoredProgram {
                source: source.to_string(),
                predicates,
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
        let dir = self.programs_dir.as_ref()
            .ok_or_else(|| anyhow!("no programs directory configured"))?;
        let path = dir.join(format!("{}.mg", name));
        let source = std::fs::read_to_string(&path)
            .map_err(|e| anyhow!("cannot read {}: {}", path.display(), e))?;
        self.load(name, &source)
    }

    /// Reload all `.mg` files from `programs_dir`.
    pub fn reload_all(&mut self) -> Result<Vec<ProgramInfo>> {
        let dir = self.programs_dir.as_ref()
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

    // TODO: Persistence: add Postgres-backed storage (mangle-server already
    // shares the Postgres instance via the backend network)

    /// Recompile from stored source, execute, and scan the queried relation.
    /// Parses query arguments and filters results to match constant positions.
    pub fn execute_query(
        &self,
        name: &str,
        query: &str,
    ) -> Result<Vec<Vec<Value>>> {
        let prog = self
            .programs
            .get(name)
            .ok_or_else(|| anyhow!("program '{}' not found", name))?;

        let parsed = parse_query_lenient(query)?;

        let arena = Arena::new_with_global_interner();
        let (mut ir, stratified) = mangle_driver::compile(&prog.source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = mangle_driver::execute(&mut ir, &stratified, store)?;

        let tuples: Vec<Vec<Value>> = interpreter.store().scan(&parsed.predicate)?.collect();
        Ok(filter_tuples(tuples, &parsed))
    }
}

/// Compile and execute ephemeral source, returning results for the queried relation.
pub fn eval_source(source: &str, query: Option<&str>) -> Result<Vec<Vec<Value>>> {
    let arena = Arena::new_with_global_interner();
    let (mut ir, stratified) = mangle_driver::compile(source, &arena)?;
    let store = Box::new(MemStore::new());
    let interpreter = mangle_driver::execute(&mut ir, &stratified, store)?;

    if let Some(q) = query {
        let parsed = parse_query_lenient(q)?;
        let tuples: Vec<Vec<Value>> = interpreter.store().scan(&parsed.predicate)?.collect();
        Ok(filter_tuples(tuples, &parsed))
    } else {
        let mut all = Vec::new();
        for name in interpreter.store().relation_names() {
            let tuples: Vec<Vec<Value>> = interpreter.store().scan(&name)?.collect();
            all.extend(tuples);
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

        let mut store = ProgramStore::new()
            .with_programs_dir(dir.path().to_path_buf());

        let info = store.reload("sample").unwrap();
        assert_eq!(info.name, "sample");
        assert!(store.get("sample").is_some());
    }

    #[test]
    fn test_reload_all() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mg"), "p(1).").unwrap();
        std::fs::write(dir.path().join("b.mg"), "q(2).").unwrap();

        let mut store = ProgramStore::new()
            .with_programs_dir(dir.path().to_path_buf());

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
        let gets = store.execute_query("routes", r#"route("GET", P, H)"#).unwrap();
        assert_eq!(gets.len(), 2);

        // Specific route
        let specific = store.execute_query("routes", r#"route("POST", "/api", H)"#).unwrap();
        assert_eq!(specific.len(), 1);
    }
}
