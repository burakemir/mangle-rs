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

//! Disk-backed Store implementation using redb.
//!
//! Each relation is stored as a redb table mapping auto-increment u64 keys
//! to JSON-serialized tuples. This enables datasets larger than available RAM
//! with OS-managed memory-mapped paging.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Result, anyhow};
use mangle_common::{Store, Value};
use redb::{Database, ReadableTable, TableDefinition};

/// Tuple serialization: each `Vec<Value>` is serialized as a JSON array.
fn serialize_tuple(tuple: &[Value]) -> Vec<u8> {
    serde_json::to_vec(
        &tuple
            .iter()
            .map(|v| match v {
                Value::Number(n) => serde_json::Value::Number((*n).into()),
                Value::String(s) => serde_json::Value::String(s.clone()),
                Value::Null => serde_json::Value::Null,
            })
            .collect::<Vec<_>>(),
    )
    .expect("tuple serialization should not fail")
}

fn deserialize_tuple(data: &[u8]) -> Result<Vec<Value>> {
    let arr: Vec<serde_json::Value> = serde_json::from_slice(data)?;
    Ok(arr
        .into_iter()
        .map(|v| match v {
            serde_json::Value::Number(n) => Value::Number(n.as_i64().unwrap_or(0)),
            serde_json::Value::String(s) => Value::String(s),
            serde_json::Value::Null => Value::Null,
            _ => Value::Null,
        })
        .collect())
}

/// Meta table tracking which relations exist and their next row ID.
const META_TABLE: TableDefinition<&str, u64> = TableDefinition::new("__meta__");

/// Get the table definition for a relation's stable facts.
fn stable_table(relation: &str) -> String {
    format!("stable:{relation}")
}

/// Get the table definition for a relation's delta facts.
fn delta_table(relation: &str) -> String {
    format!("delta:{relation}")
}

/// Get the table definition for a relation's next_delta facts.
fn next_delta_table(relation: &str) -> String {
    format!("next_delta:{relation}")
}

/// A Store implementation backed by redb (an embedded key-value database).
///
/// Uses memory-mapped I/O for crash-safe, OS-managed paging.
/// Each relation is stored as a separate redb table.
pub struct DiskStore {
    db: Database,
    /// Track known relations (to avoid scanning meta table repeatedly).
    relations: HashSet<String>,
    /// In-memory indexes for the current stable+delta facts.
    /// Rebuilt on merge_deltas(). Maps (relation, col_idx) -> { value -> [row_keys] }
    stable_indexes: HashMap<(String, usize), HashMap<Value, Vec<u64>>>,
    delta_indexes: HashMap<(String, usize), HashMap<Value, Vec<u64>>>,
    /// Next row ID per relation (monotonically increasing).
    next_ids: HashMap<String, u64>,
}

impl DiskStore {
    /// Open or create a disk-backed store at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::create(path)?;

        // Initialize meta table
        let write_txn = db.begin_write()?;
        {
            let _table = write_txn.open_table(META_TABLE)?;
        }
        write_txn.commit()?;

        // Load existing relations
        let mut relations = HashSet::new();
        let mut next_ids = HashMap::new();
        {
            let read_txn = db.begin_read()?;
            let table = read_txn.open_table(META_TABLE)?;
            let iter = table.iter()?;
            for entry in iter {
                let entry = entry?;
                let name = entry.0.value().to_string();
                let next_id = entry.1.value();
                next_ids.insert(name.clone(), next_id);
                relations.insert(name);
            }
        }

        Ok(Self {
            db,
            relations,
            stable_indexes: HashMap::new(),
            delta_indexes: HashMap::new(),
            next_ids,
        })
    }

    fn alloc_id(&mut self, relation: &str) -> u64 {
        let id = self.next_ids.entry(relation.to_string()).or_insert(0);
        let result = *id;
        *id += 1;
        result
    }

    fn read_table_tuples(&self, table_name: &str) -> Result<Vec<(u64, Vec<Value>)>> {
        let read_txn = self.db.begin_read()?;
        let table_def: TableDefinition<u64, &[u8]> = TableDefinition::new(table_name);
        let table = match read_txn.open_table(table_def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(vec![]),
            Err(e) => return Err(anyhow!("failed to open table {table_name}: {e}")),
        };
        let mut result = Vec::new();
        let iter = table.iter()?;
        for entry in iter {
            let entry = entry?;
            let key = entry.0.value();
            let tuple = deserialize_tuple(entry.1.value())?;
            result.push((key, tuple));
        }
        Ok(result)
    }

    fn contains_tuple(&self, table_name: &str, tuple: &[Value]) -> Result<bool> {
        let tuples = self.read_table_tuples(table_name)?;
        let serialized = serialize_tuple(tuple);
        Ok(tuples.iter().any(|(_, t)| serialize_tuple(t) == serialized))
    }

    fn rebuild_indexes_for(&mut self, relation: &str) -> Result<()> {
        // Clear existing indexes for this relation
        self.stable_indexes.retain(|(rel, _), _| rel != relation);
        self.delta_indexes.retain(|(rel, _), _| rel != relation);

        // Rebuild stable indexes
        let stable_name = stable_table(relation);
        for (row_idx, tuple) in self.read_table_tuples(&stable_name)? {
            for (col_idx, val) in tuple.iter().enumerate() {
                self.stable_indexes
                    .entry((relation.to_string(), col_idx))
                    .or_default()
                    .entry(val.clone())
                    .or_default()
                    .push(row_idx);
            }
        }

        // Rebuild delta indexes
        let delta_name = delta_table(relation);
        for (row_idx, tuple) in self.read_table_tuples(&delta_name)? {
            for (col_idx, val) in tuple.iter().enumerate() {
                self.delta_indexes
                    .entry((relation.to_string(), col_idx))
                    .or_default()
                    .entry(val.clone())
                    .or_default()
                    .push(row_idx);
            }
        }

        Ok(())
    }

    fn save_meta(&self) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(META_TABLE)?;
            for (name, next_id) in &self.next_ids {
                table.insert(name.as_str(), *next_id)?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }
}

impl Store for DiskStore {
    fn scan(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        let mut all = Vec::new();
        for (_, tuple) in self.read_table_tuples(&stable_table(relation))? {
            all.push(tuple);
        }
        for (_, tuple) in self.read_table_tuples(&delta_table(relation))? {
            all.push(tuple);
        }
        Ok(Box::new(all.into_iter()))
    }

    fn scan_delta(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        let tuples: Vec<Vec<Value>> = self
            .read_table_tuples(&delta_table(relation))?
            .into_iter()
            .map(|(_, t)| t)
            .collect();
        Ok(Box::new(tuples.into_iter()))
    }

    fn scan_next_delta(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        let tuples: Vec<Vec<Value>> = self
            .read_table_tuples(&next_delta_table(relation))?
            .into_iter()
            .map(|(_, t)| t)
            .collect();
        Ok(Box::new(tuples.into_iter()))
    }

    fn scan_index(
        &self,
        relation: &str,
        col_idx: usize,
        key: &Value,
    ) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        // For disk store, fall back to full scan with filter
        // (indexes are in-memory and may not be complete for disk data)
        let mut results = Vec::new();

        // Check stable
        for (_, tuple) in self.read_table_tuples(&stable_table(relation))? {
            if tuple.get(col_idx) == Some(key) {
                results.push(tuple);
            }
        }
        // Check delta
        for (_, tuple) in self.read_table_tuples(&delta_table(relation))? {
            if tuple.get(col_idx) == Some(key) {
                results.push(tuple);
            }
        }

        Ok(Box::new(results.into_iter()))
    }

    fn scan_delta_index(
        &self,
        relation: &str,
        col_idx: usize,
        key: &Value,
    ) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        let mut results = Vec::new();
        for (_, tuple) in self.read_table_tuples(&delta_table(relation))? {
            if tuple.get(col_idx) == Some(key) {
                results.push(tuple);
            }
        }
        Ok(Box::new(results.into_iter()))
    }

    fn insert(&mut self, relation: &str, tuple: Vec<Value>) -> Result<bool> {
        // Check dedup across all tiers
        if self.contains_tuple(&stable_table(relation), &tuple)?
            || self.contains_tuple(&delta_table(relation), &tuple)?
            || self.contains_tuple(&next_delta_table(relation), &tuple)?
        {
            return Ok(false);
        }

        let id = self.alloc_id(relation);
        let serialized = serialize_tuple(&tuple);
        let table_name = next_delta_table(relation);

        let write_txn = self.db.begin_write()?;
        {
            let table_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&table_name);
            let mut table = write_txn.open_table(table_def)?;
            table.insert(id, serialized.as_slice())?;
        }
        write_txn.commit()?;

        Ok(true)
    }

    fn merge_deltas(&mut self) {
        for relation in self.relations.clone() {
            let stable_name = stable_table(&relation);
            let delta_name = delta_table(&relation);
            let nd_name = next_delta_table(&relation);

            // 1. Move delta → stable
            if let Ok(delta_tuples) = self.read_table_tuples(&delta_name) {
                if !delta_tuples.is_empty() {
                    let write_txn = self.db.begin_write().unwrap();
                    {
                        let stable_def: TableDefinition<u64, &[u8]> =
                            TableDefinition::new(&stable_name);
                        let mut stable = write_txn.open_table(stable_def).unwrap();
                        for (_, tuple) in &delta_tuples {
                            let id = self.alloc_id(&relation);
                            let data = serialize_tuple(tuple);
                            stable.insert(id, data.as_slice()).unwrap();
                        }
                        // Clear delta
                        let delta_def: TableDefinition<u64, &[u8]> =
                            TableDefinition::new(&delta_name);
                        let _delta = write_txn.delete_table(delta_def);
                    }
                    write_txn.commit().unwrap();
                }
            }

            // 2. Move next_delta → delta
            if let Ok(nd_tuples) = self.read_table_tuples(&nd_name) {
                if !nd_tuples.is_empty() {
                    let write_txn = self.db.begin_write().unwrap();
                    {
                        let delta_def: TableDefinition<u64, &[u8]> =
                            TableDefinition::new(&delta_name);
                        let mut delta = write_txn.open_table(delta_def).unwrap();
                        for (_, tuple) in &nd_tuples {
                            let id = self.alloc_id(&relation);
                            let data = serialize_tuple(tuple);
                            delta.insert(id, data.as_slice()).unwrap();
                        }
                        // Clear next_delta
                        let nd_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&nd_name);
                        let _nd = write_txn.delete_table(nd_def);
                    }
                    write_txn.commit().unwrap();
                }
            }

            // Rebuild indexes
            let _ = self.rebuild_indexes_for(&relation);
        }

        let _ = self.save_meta();
    }

    fn create_relation(&mut self, relation: &str) {
        if self.relations.insert(relation.to_string()) {
            self.next_ids.entry(relation.to_string()).or_insert(0);
            // Create the stable table
            let table_name = stable_table(relation);
            let write_txn = self.db.begin_write().unwrap();
            {
                let table_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&table_name);
                let _table = write_txn.open_table(table_def).unwrap();
            }
            write_txn.commit().unwrap();
            let _ = self.save_meta();
        }
    }

    fn retract(&mut self, relation: &str, tuple: &[Value]) -> Result<bool> {
        let serialized = serialize_tuple(tuple);

        // Check stable
        let stable_name = stable_table(relation);
        let stable_tuples = self.read_table_tuples(&stable_name)?;
        for (key, t) in &stable_tuples {
            if serialize_tuple(t) == serialized {
                let write_txn = self.db.begin_write()?;
                {
                    let table_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&stable_name);
                    let mut table = write_txn.open_table(table_def)?;
                    table.remove(*key)?;
                }
                write_txn.commit()?;
                let _ = self.rebuild_indexes_for(relation);
                return Ok(true);
            }
        }

        // Check delta
        let delta_name = delta_table(relation);
        let delta_tuples = self.read_table_tuples(&delta_name)?;
        for (key, t) in &delta_tuples {
            if serialize_tuple(t) == serialized {
                let write_txn = self.db.begin_write()?;
                {
                    let table_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&delta_name);
                    let mut table = write_txn.open_table(table_def)?;
                    table.remove(*key)?;
                }
                write_txn.commit()?;
                let _ = self.rebuild_indexes_for(relation);
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn clear(&mut self, relation: &str) {
        let sn = stable_table(relation);
        let dn = delta_table(relation);
        let ndn = next_delta_table(relation);
        let write_txn = self.db.begin_write().unwrap();
        {
            let stable_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&sn);
            let _ = write_txn.delete_table(stable_def);
            let delta_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&dn);
            let _ = write_txn.delete_table(delta_def);
            let nd_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&ndn);
            let _ = write_txn.delete_table(nd_def);
        }
        write_txn.commit().unwrap();

        self.stable_indexes.retain(|(rel, _), _| rel != relation);
        self.delta_indexes.retain(|(rel, _), _| rel != relation);
    }

    fn relation_names(&self) -> Vec<String> {
        self.relations.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disk_store_basic() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("test.redb");

        let mut store = DiskStore::open(&db_path)?;
        store.create_relation("edge");

        store.insert("edge", vec![Value::Number(1), Value::Number(2)])?;
        store.insert("edge", vec![Value::Number(2), Value::Number(3)])?;
        store.merge_deltas();

        let facts: Vec<_> = store.scan("edge")?.collect();
        assert_eq!(facts.len(), 2);

        Ok(())
    }

    #[test]
    fn test_disk_store_dedup() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("test.redb");

        let mut store = DiskStore::open(&db_path)?;
        store.create_relation("r");

        let inserted1 = store.insert("r", vec![Value::Number(1)])?;
        assert!(inserted1);

        let inserted2 = store.insert("r", vec![Value::Number(1)])?;
        assert!(!inserted2);

        store.merge_deltas();

        let inserted3 = store.insert("r", vec![Value::Number(1)])?;
        assert!(!inserted3);

        let facts: Vec<_> = store.scan("r")?.collect();
        assert_eq!(facts.len(), 1);

        Ok(())
    }

    #[test]
    fn test_disk_store_retract() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("test.redb");

        let mut store = DiskStore::open(&db_path)?;
        store.create_relation("r");

        store.insert("r", vec![Value::Number(1)])?;
        store.insert("r", vec![Value::Number(2)])?;
        store.merge_deltas();

        let removed = store.retract("r", &[Value::Number(1)])?;
        assert!(removed);

        let facts: Vec<_> = store.scan("r")?.collect();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0], vec![Value::Number(2)]);

        Ok(())
    }

    #[test]
    fn test_disk_store_with_strings() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("test.redb");

        let mut store = DiskStore::open(&db_path)?;
        store.create_relation("user");

        store.insert(
            "user",
            vec![Value::String("Alice".to_string()), Value::Number(30)],
        )?;
        store.insert(
            "user",
            vec![Value::String("Bob".to_string()), Value::Number(25)],
        )?;
        store.merge_deltas();

        let facts: Vec<_> = store.scan("user")?.collect();
        assert_eq!(facts.len(), 2);

        Ok(())
    }

    #[test]
    fn test_disk_store_semi_naive() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("test.redb");

        let mut store = DiskStore::open(&db_path)?;
        store.create_relation("r");

        // Insert and merge (simulating semi-naive iteration)
        store.insert("r", vec![Value::Number(1)])?;
        store.merge_deltas();

        // Delta should now have the fact, next insert goes to next_delta
        let delta: Vec<_> = store.scan_delta("r")?.collect();
        assert_eq!(delta.len(), 1);

        store.insert("r", vec![Value::Number(2)])?;
        store.merge_deltas();

        // Now both facts should be in stable or delta
        let all: Vec<_> = store.scan("r")?.collect();
        assert_eq!(all.len(), 2);

        Ok(())
    }
}
