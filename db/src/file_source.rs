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

//! File-based EDB source: reads `.mgr` (simplerow) and `.mg` (Mangle source) files.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Result, anyhow};
use mangle_factstore::Value;
use sha2::{Digest, Sha256};

use crate::simplerow;
use crate::source::{EdbSource, Fingerprint, RelationInfo};

/// An EDB source that reads facts from a directory of `.mgr` and `.mg` files.
///
/// - `.mgr` files are SimpleRow data files (fast loading).
/// - `.mg` files are Mangle programs (compiled + executed to extract facts).
pub struct FileEdbSource {
    name: String,
    dir: PathBuf,
    /// Cached table data (relation_name -> facts). Populated on first access.
    cache: Mutex<Option<HashMap<String, Vec<Vec<Value>>>>>,
}

impl FileEdbSource {
    pub fn new(name: impl Into<String>, dir: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            dir: dir.into(),
            cache: Mutex::new(None),
        }
    }

    fn load_data(&self) -> Result<HashMap<String, Vec<Vec<Value>>>> {
        let mut tables = HashMap::new();

        if !self.dir.exists() {
            return Err(anyhow!("EDB source directory does not exist: {:?}", self.dir));
        }

        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().is_some_and(|ext| ext == "mgr") {
                // SimpleRow file
                let data = std::fs::read(&path)?;
                let sr_data = simplerow::read_from_bytes(&data)?;
                for (name, facts) in sr_data.tables {
                    tables.entry(name).or_insert_with(Vec::new).extend(facts);
                }
            } else if path.extension().is_some_and(|ext| ext == "mg") {
                // Mangle source file — compile and execute to extract facts
                let source = std::fs::read_to_string(&path)?;
                let facts = execute_source_for_facts(&source)?;
                for (name, tuples) in facts {
                    tables.entry(name).or_insert_with(Vec::new).extend(tuples);
                }
            }
        }

        Ok(tables)
    }

    fn ensure_loaded(&self) -> Result<()> {
        let mut cache = self.cache.lock().map_err(|_| anyhow!("lock poisoned"))?;
        if cache.is_none() {
            *cache = Some(self.load_data()?);
        }
        Ok(())
    }
}

impl EdbSource for FileEdbSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn relations(&self) -> Result<Vec<RelationInfo>> {
        self.ensure_loaded()?;
        let cache = self.cache.lock().map_err(|_| anyhow!("lock poisoned"))?;
        let data = cache.as_ref().unwrap();
        Ok(data
            .iter()
            .map(|(name, facts)| RelationInfo {
                name: name.clone(),
                estimated_rows: facts.len(),
            })
            .collect())
    }

    fn scan(&self, relation: &str) -> Result<Vec<Vec<Value>>> {
        self.ensure_loaded()?;
        let cache = self.cache.lock().map_err(|_| anyhow!("lock poisoned"))?;
        let data = cache.as_ref().unwrap();
        Ok(data.get(relation).cloned().unwrap_or_default())
    }

    fn fingerprint(&self) -> Result<Option<Fingerprint>> {
        if !self.dir.exists() {
            return Ok(None);
        }

        let mut hasher = Sha256::new();
        let mut entries: Vec<_> = std::fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                let path = e.path();
                path.extension()
                    .is_some_and(|ext| ext == "mgr" || ext == "mg")
            })
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();
            hasher.update(path.file_name().unwrap().as_encoded_bytes());
            let meta = std::fs::metadata(&path)?;
            hasher.update(meta.len().to_le_bytes());
            if let Ok(mtime) = meta.modified() {
                let secs = mtime
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                hasher.update(secs.to_le_bytes());
            }
        }

        Ok(Some(Fingerprint(hasher.finalize().to_vec())))
    }
}

/// Execute a Mangle source string and return all derived facts.
fn execute_source_for_facts(source: &str) -> Result<HashMap<String, Vec<Vec<Value>>>> {
    use mangle_ast::Arena;
    use mangle_interpreter::MemStore;

    let arena = Arena::new_with_global_interner();
    let (mut ir, stratified) = mangle_driver::compile(source, &arena)?;

    // Collect IDB predicate names while we still have the arena
    let mut idb_names = Vec::new();
    for stratum in stratified.strata() {
        for pred in &stratum {
            if let Some(name) = arena.predicate_name(*pred) {
                idb_names.push(name.to_string());
            }
        }
    }

    let store = Box::new(MemStore::new());
    let interpreter = mangle_driver::execute(&mut ir, &stratified, store)?;

    let mut result = HashMap::new();
    // Use IDB names (which include facts defined by unit clauses)
    // plus any relation_names from the store
    let mut all_names: Vec<String> = interpreter.store().relation_names();
    for name in &idb_names {
        if !all_names.contains(name) {
            all_names.push(name.clone());
        }
    }

    for name in &all_names {
        if let Ok(iter) = interpreter.store().scan(name) {
            let facts: Vec<Vec<Value>> = iter.collect();
            if !facts.is_empty() {
                result.insert(name.clone(), facts);
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_file_edb_source_mgr() -> Result<()> {
        let dir = tempfile::tempdir()?;

        // Write a .mgr file
        let mgr_path = dir.path().join("edges.mgr");
        let mut f = std::fs::File::create(&mgr_path)?;
        let tables = vec![(
            "edge".to_string(),
            vec![
                vec![Value::Number(1), Value::Number(2)],
                vec![Value::Number(2), Value::Number(3)],
            ],
        )];
        simplerow::write_simple_row(&mut f, &tables)?;

        let source = FileEdbSource::new("test", dir.path());

        let relations = source.relations()?;
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].name, "edge");
        assert_eq!(relations[0].estimated_rows, 2);

        let facts = source.scan("edge")?;
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0], vec![Value::Number(1), Value::Number(2)]);

        let fp = source.fingerprint()?;
        assert!(fp.is_some());

        Ok(())
    }

    #[test]
    fn test_file_edb_source_mg() -> Result<()> {
        let dir = tempfile::tempdir()?;

        // Write a .mg file — use separate lines so parsing is unambiguous
        let mg_path = dir.path().join("data.mg");
        std::fs::write(&mg_path, "p(1).\np(2).\np(3).\n")?;

        let source = FileEdbSource::new("test", dir.path());

        let relations = source.relations()?;
        assert!(relations.iter().any(|r| r.name == "p"));

        let facts = source.scan("p")?;
        assert_eq!(facts.len(), 3);

        Ok(())
    }
}
