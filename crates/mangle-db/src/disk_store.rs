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
//! to postcard-serialized tuples. This enables datasets larger than available
//! RAM with OS-managed memory-mapped paging.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use mangle_common::{CompoundKind, Store, Value};
use redb::{Database, ReadableTable, TableDefinition, TableHandle};
use serde::{Deserialize, Serialize};

/// Current tuple-format version. Stored once per database in `FORMAT_TABLE`
/// (not per tuple — see `open()`). Bump when the wire schema changes in a
/// non-backwards-compatible way.
const TUPLE_FORMAT_VERSION: u64 = 1;

/// Key under which `TUPLE_FORMAT_VERSION` is recorded in `FORMAT_TABLE`.
const FORMAT_VERSION_KEY: &str = "tuple_format_version";

/// On-disk representation of `CompoundKind`. Decoupled from the runtime enum
/// so postcard sees a stable wire schema.
#[derive(Serialize, Deserialize)]
enum CompoundKindRepr {
    List,
    Pair,
    Map,
    Struct,
}

impl From<CompoundKind> for CompoundKindRepr {
    fn from(k: CompoundKind) -> Self {
        match k {
            CompoundKind::List => Self::List,
            CompoundKind::Pair => Self::Pair,
            CompoundKind::Map => Self::Map,
            CompoundKind::Struct => Self::Struct,
        }
    }
}

impl From<CompoundKindRepr> for CompoundKind {
    fn from(k: CompoundKindRepr) -> Self {
        match k {
            CompoundKindRepr::List => Self::List,
            CompoundKindRepr::Pair => Self::Pair,
            CompoundKindRepr::Map => Self::Map,
            CompoundKindRepr::Struct => Self::Struct,
        }
    }
}

/// On-disk representation of `Value`. Postcard serializes this as a compact
/// variant-tagged enum. `f64` is encoded bit-for-bit, so NaN payloads round-trip.
#[derive(Serialize, Deserialize)]
enum ValueRepr {
    Number(i64),
    Float(f64),
    String(String),
    Name(String),
    Time(i64),
    Duration(i64),
    Compound(CompoundKindRepr, Vec<ValueRepr>),
    Null,
}

impl From<&Value> for ValueRepr {
    fn from(v: &Value) -> Self {
        match v {
            Value::Number(n) => Self::Number(*n),
            Value::Float(f) => Self::Float(*f),
            Value::String(s) => Self::String(s.clone()),
            Value::Name(s) => Self::Name(s.clone()),
            Value::Time(t) => Self::Time(*t),
            Value::Duration(d) => Self::Duration(*d),
            Value::Compound(k, vs) => {
                Self::Compound((*k).into(), vs.iter().map(ValueRepr::from).collect())
            }
            Value::Null => Self::Null,
        }
    }
}

impl From<ValueRepr> for Value {
    fn from(v: ValueRepr) -> Self {
        match v {
            ValueRepr::Number(n) => Value::Number(n),
            ValueRepr::Float(f) => Value::Float(f),
            ValueRepr::String(s) => Value::String(s),
            ValueRepr::Name(s) => Value::Name(s),
            ValueRepr::Time(t) => Value::Time(t),
            ValueRepr::Duration(d) => Value::Duration(d),
            ValueRepr::Compound(k, vs) => {
                Value::Compound(k.into(), vs.into_iter().map(Value::from).collect())
            }
            ValueRepr::Null => Value::Null,
        }
    }
}

/// Serialize a tuple to postcard bytes. Format version is tracked once per DB
/// in `FORMAT_TABLE`, not per tuple.
fn serialize_tuple(tuple: &[Value]) -> Vec<u8> {
    let repr: Vec<ValueRepr> = tuple.iter().map(ValueRepr::from).collect();
    postcard::to_allocvec(&repr).expect("postcard tuple serialization should not fail")
}

fn deserialize_tuple(data: &[u8]) -> Result<Vec<Value>> {
    let repr: Vec<ValueRepr> =
        postcard::from_bytes(data).map_err(|e| anyhow!("failed to decode tuple: {e}"))?;
    Ok(repr.into_iter().map(Value::from).collect())
}

/// Meta table tracking which relations exist and their next row ID.
const META_TABLE: TableDefinition<&str, u64> = TableDefinition::new("__meta__");

/// Format table holding a single `FORMAT_VERSION_KEY` entry. Validated on open.
const FORMAT_TABLE: TableDefinition<&str, u64> = TableDefinition::new("__format__");

/// Primary table name for a given `(tier, relation)` pair.
/// Format: `{tier}:{relation}` (e.g. `stable:edge`, `delta:edge`).
fn primary_table_for(tier: &str, relation: &str) -> String {
    format!("{tier}:{relation}")
}

/// Tier identifiers used in table-name prefixes.
const TIER_STABLE: &str = "stable";
const TIER_DELTA: &str = "delta";
const TIER_NEXT_DELTA: &str = "next_delta";

/// Secondary-index table name per `(tier, relation, column)`. Each such table
/// maps `(postcard(value_at_column), row_id) -> ()`. A range scan over all
/// entries with first element equal to a given value returns every row whose
/// column holds that value — this is the point-lookup path used by
/// `scan_index`.
fn index_table(tier: &str, relation: &str, col: usize) -> String {
    format!("idx_{tier}:{relation}:{col}")
}

/// Encode a single value for use as the first element of an index key.
fn encode_index_key(value: &Value) -> Vec<u8> {
    let repr = ValueRepr::from(value);
    postcard::to_allocvec(&repr).expect("postcard value encoding should not fail")
}

/// A Store implementation backed by redb (an embedded key-value database).
///
/// Uses memory-mapped I/O for crash-safe, OS-managed paging.
/// Each relation is stored as a separate redb table.
pub struct DiskStore {
    db: Database,
    /// Track known relations (to avoid scanning meta table repeatedly).
    relations: HashSet<String>,
    /// Next row ID per relation (monotonically increasing).
    next_ids: HashMap<String, u64>,
}

impl DiskStore {
    /// Open or create a disk-backed store at the given path.
    ///
    /// Validates the per-database tuple format version. Legacy databases
    /// (written before per-DB version tracking, i.e. JSON tuples) are detected
    /// by the presence of meta entries with no recorded format version, and
    /// rejected with a clear "recreate the database" error.
    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::create(path)?;

        // Initialize meta + format tables and validate the format version.
        // Both tables must be opened at most once per write transaction.
        let write_txn = db.begin_write()?;
        {
            let mut format = write_txn.open_table(FORMAT_TABLE)?;
            let meta = write_txn.open_table(META_TABLE)?;
            let recorded = format.get(FORMAT_VERSION_KEY)?.map(|g| g.value());
            match recorded {
                Some(v) if v == TUPLE_FORMAT_VERSION => {}
                Some(v) => bail!(
                    "database tuple format version is {v}; this build expects \
                     {TUPLE_FORMAT_VERSION}. Recreate the database."
                ),
                None => {
                    // No format version recorded. Could be a fresh DB or a
                    // legacy JSON-format DB. Distinguish by whether any
                    // relations have been registered in meta.
                    if meta.iter()?.next().is_some() {
                        bail!(
                            "database uses legacy JSON tuple format (no recorded format \
                             version but meta entries present); mangle-db no longer supports \
                             reading it. Recreate the database."
                        );
                    }
                    format.insert(FORMAT_VERSION_KEY, TUPLE_FORMAT_VERSION)?;
                }
            }
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

    /// Check whether a tuple already exists in a given tier. Uses the index
    /// on column 0 when possible to avoid a full scan. Falls back to scan for
    /// zero-arity tuples (rare / non-standard).
    fn tier_contains_tuple(&self, tier: &str, relation: &str, tuple: &[Value]) -> Result<bool> {
        if tuple.is_empty() {
            return self.tier_contains_tuple_by_scan(tier, relation, tuple);
        }
        let read_txn = self.db.begin_read()?;
        let candidates = self.index_lookup_row_ids(&read_txn, tier, relation, 0, &tuple[0])?;
        if candidates.is_empty() {
            return Ok(false);
        }
        let primary_name = primary_table_for(tier, relation);
        let primary_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&primary_name);
        let primary = match read_txn.open_table(primary_def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(e) => return Err(anyhow!("failed to open {primary_name}: {e}")),
        };
        let serialized = serialize_tuple(tuple);
        for row_id in candidates {
            if let Some(v) = primary.get(row_id)? {
                if v.value() == serialized.as_slice() {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn tier_contains_tuple_by_scan(
        &self,
        tier: &str,
        relation: &str,
        tuple: &[Value],
    ) -> Result<bool> {
        let tuples = self.read_table_tuples(&primary_table_for(tier, relation))?;
        let serialized = serialize_tuple(tuple);
        Ok(tuples.iter().any(|(_, t)| serialize_tuple(t) == serialized))
    }

    /// Look up row_ids whose column `col` holds `value`, via the index table
    /// for the given tier. Returns an empty vector if the index table does
    /// not exist yet (e.g. no inserts happened on that tier/column).
    fn index_lookup_row_ids(
        &self,
        read_txn: &redb::ReadTransaction,
        tier: &str,
        relation: &str,
        col: usize,
        value: &Value,
    ) -> Result<Vec<u64>> {
        let table_name = index_table(tier, relation, col);
        let index_def: TableDefinition<(&[u8], u64), ()> = TableDefinition::new(&table_name);
        let index = match read_txn.open_table(index_def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(vec![]),
            Err(e) => return Err(anyhow!("failed to open {table_name}: {e}")),
        };
        let key_bytes = encode_index_key(value);
        let range = (key_bytes.as_slice(), 0u64)..=(key_bytes.as_slice(), u64::MAX);
        let mut row_ids = Vec::new();
        for entry in index.range(range)? {
            let entry = entry?;
            let (_, row_id) = entry.0.value();
            row_ids.push(row_id);
        }
        Ok(row_ids)
    }

    /// Look up row_ids via the tier's index and append the full tuples to
    /// `out`. No-op if the index table or primary table does not exist.
    fn fetch_by_index(
        &self,
        tier: &str,
        relation: &str,
        col: usize,
        value: &Value,
        out: &mut Vec<Vec<Value>>,
    ) -> Result<()> {
        let read_txn = self.db.begin_read()?;
        let row_ids = self.index_lookup_row_ids(&read_txn, tier, relation, col, value)?;
        if row_ids.is_empty() {
            return Ok(());
        }
        let primary_name = primary_table_for(tier, relation);
        let primary_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&primary_name);
        let primary = match read_txn.open_table(primary_def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(e) => return Err(anyhow!("failed to open {primary_name}: {e}")),
        };
        for row_id in row_ids {
            if let Some(v) = primary.get(row_id)? {
                out.push(deserialize_tuple(v.value())?);
            }
        }
        Ok(())
    }

    /// Move all rows from `src_tier` to `dst_tier` for a given relation,
    /// rebuilding index entries under the destination tier and dropping the
    /// source tier's primary + index tables. Row ids are reallocated (the
    /// destination uses fresh ids from `self.next_ids`).
    fn promote_tier(&mut self, src_tier: &str, dst_tier: &str, relation: &str) -> Result<()> {
        let src_primary_name = primary_table_for(src_tier, relation);
        let src_tuples = self.read_table_tuples(&src_primary_name)?;
        if src_tuples.is_empty() {
            // Even with no tuples, stale index tables may exist — drop them.
            self.drop_tier_tables(src_tier, relation)?;
            return Ok(());
        }

        // Preallocate destination row_ids up front so we don't mutate
        // `self.next_ids` inside the transaction-building loop.
        let new_ids: Vec<u64> = (0..src_tuples.len())
            .map(|_| self.alloc_id(relation))
            .collect();
        let dst_primary_name = primary_table_for(dst_tier, relation);

        let write_txn = self.db.begin_write()?;
        {
            let dst_primary_def: TableDefinition<u64, &[u8]> =
                TableDefinition::new(&dst_primary_name);
            let mut dst_primary = write_txn.open_table(dst_primary_def)?;
            for ((_, tuple), new_id) in src_tuples.iter().zip(new_ids.iter()) {
                let data = serialize_tuple(tuple);
                dst_primary.insert(*new_id, data.as_slice())?;
            }
            // Group index updates by column so each dst index table is
            // opened only once in this transaction.
            let arity = src_tuples.iter().map(|(_, t)| t.len()).max().unwrap_or(0);
            for col in 0..arity {
                let dst_idx_name = index_table(dst_tier, relation, col);
                let dst_idx_def: TableDefinition<(&[u8], u64), ()> =
                    TableDefinition::new(&dst_idx_name);
                let mut dst_idx = write_txn.open_table(dst_idx_def)?;
                for ((_, tuple), new_id) in src_tuples.iter().zip(new_ids.iter()) {
                    if let Some(val) = tuple.get(col) {
                        let key_bytes = encode_index_key(val);
                        dst_idx.insert((key_bytes.as_slice(), *new_id), ())?;
                    }
                }
            }
        }
        write_txn.commit()?;

        // Drop source tier tables (primary + all index tables).
        self.drop_tier_tables(src_tier, relation)
    }

    /// Find and remove a tuple from the given tier (primary + all its index
    /// entries). Returns true if the tuple existed. Uses the column-0 index
    /// to locate candidate row_ids.
    fn retract_from_tier(&self, tier: &str, relation: &str, tuple: &[Value]) -> Result<bool> {
        if tuple.is_empty() {
            return self.retract_from_tier_by_scan(tier, relation, tuple);
        }
        let read_txn = self.db.begin_read()?;
        let candidates = self.index_lookup_row_ids(&read_txn, tier, relation, 0, &tuple[0])?;
        if candidates.is_empty() {
            return Ok(false);
        }
        let primary_name = primary_table_for(tier, relation);
        let primary_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&primary_name);
        let primary = match read_txn.open_table(primary_def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(e) => return Err(anyhow!("failed to open {primary_name}: {e}")),
        };
        let serialized = serialize_tuple(tuple);
        let mut target: Option<u64> = None;
        for row_id in candidates {
            if let Some(v) = primary.get(row_id)? {
                if v.value() == serialized.as_slice() {
                    target = Some(row_id);
                    break;
                }
            }
        }
        drop(primary);
        drop(read_txn);
        let Some(row_id) = target else {
            return Ok(false);
        };

        // Now remove in a write txn: primary row + every column's index entry.
        let write_txn = self.db.begin_write()?;
        {
            let primary_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&primary_name);
            let mut primary = write_txn.open_table(primary_def)?;
            primary.remove(row_id)?;
            for (col, val) in tuple.iter().enumerate() {
                let idx_name = index_table(tier, relation, col);
                let idx_def: TableDefinition<(&[u8], u64), ()> = TableDefinition::new(&idx_name);
                let mut idx = match write_txn.open_table(idx_def) {
                    Ok(t) => t,
                    Err(redb::TableError::TableDoesNotExist(_)) => continue,
                    Err(e) => return Err(anyhow!("failed to open {idx_name}: {e}")),
                };
                let key_bytes = encode_index_key(val);
                idx.remove((key_bytes.as_slice(), row_id))?;
            }
        }
        write_txn.commit()?;
        Ok(true)
    }

    fn retract_from_tier_by_scan(
        &self,
        tier: &str,
        relation: &str,
        tuple: &[Value],
    ) -> Result<bool> {
        let primary_name = primary_table_for(tier, relation);
        let tuples = self.read_table_tuples(&primary_name)?;
        let serialized = serialize_tuple(tuple);
        for (row_id, t) in &tuples {
            if serialize_tuple(t) == serialized {
                let write_txn = self.db.begin_write()?;
                {
                    let primary_def: TableDefinition<u64, &[u8]> =
                        TableDefinition::new(&primary_name);
                    let mut primary = write_txn.open_table(primary_def)?;
                    primary.remove(*row_id)?;
                }
                write_txn.commit()?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Drop the primary and all index tables for a given `(tier, relation)`.
    /// Uses `list_tables` to enumerate index tables by name prefix so arity
    /// need not be tracked separately.
    fn drop_tier_tables(&self, tier: &str, relation: &str) -> Result<()> {
        let primary_name = primary_table_for(tier, relation);
        let write_txn = self.db.begin_write()?;
        {
            // Primary.
            let primary_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&primary_name);
            let _ = write_txn.delete_table(primary_def);

            // Index tables with matching prefix.
            let prefix = format!("idx_{tier}:{relation}:");
            let handles: Vec<_> = write_txn
                .list_tables()?
                .filter(|h| h.name().starts_with(&prefix))
                .collect();
            for h in handles {
                let _ = write_txn.delete_table(h);
            }
        }
        write_txn.commit()?;
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
        for (_, tuple) in self.read_table_tuples(&primary_table_for(TIER_STABLE, relation))? {
            all.push(tuple);
        }
        for (_, tuple) in self.read_table_tuples(&primary_table_for(TIER_DELTA, relation))? {
            all.push(tuple);
        }
        Ok(Box::new(all.into_iter()))
    }

    fn scan_delta(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        let tuples: Vec<Vec<Value>> = self
            .read_table_tuples(&primary_table_for(TIER_DELTA, relation))?
            .into_iter()
            .map(|(_, t)| t)
            .collect();
        Ok(Box::new(tuples.into_iter()))
    }

    fn scan_next_delta(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        let tuples: Vec<Vec<Value>> = self
            .read_table_tuples(&primary_table_for(TIER_NEXT_DELTA, relation))?
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
        let mut results = Vec::new();
        self.fetch_by_index(TIER_STABLE, relation, col_idx, key, &mut results)?;
        self.fetch_by_index(TIER_DELTA, relation, col_idx, key, &mut results)?;
        Ok(Box::new(results.into_iter()))
    }

    fn scan_delta_index(
        &self,
        relation: &str,
        col_idx: usize,
        key: &Value,
    ) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
        let mut results = Vec::new();
        self.fetch_by_index(TIER_DELTA, relation, col_idx, key, &mut results)?;
        Ok(Box::new(results.into_iter()))
    }

    fn insert(&mut self, relation: &str, tuple: Vec<Value>) -> Result<bool> {
        // Dedup across all three tiers using the column-0 index.
        if self.tier_contains_tuple(TIER_STABLE, relation, &tuple)?
            || self.tier_contains_tuple(TIER_DELTA, relation, &tuple)?
            || self.tier_contains_tuple(TIER_NEXT_DELTA, relation, &tuple)?
        {
            return Ok(false);
        }

        let id = self.alloc_id(relation);
        let serialized = serialize_tuple(&tuple);
        let primary_name = primary_table_for(TIER_NEXT_DELTA, relation);

        let write_txn = self.db.begin_write()?;
        {
            let primary_def: TableDefinition<u64, &[u8]> = TableDefinition::new(&primary_name);
            let mut primary = write_txn.open_table(primary_def)?;
            primary.insert(id, serialized.as_slice())?;
            // Maintain the secondary index for every column in the same txn.
            for (col, val) in tuple.iter().enumerate() {
                let idx_name = index_table(TIER_NEXT_DELTA, relation, col);
                let idx_def: TableDefinition<(&[u8], u64), ()> = TableDefinition::new(&idx_name);
                let mut idx = write_txn.open_table(idx_def)?;
                let key_bytes = encode_index_key(val);
                idx.insert((key_bytes.as_slice(), id), ())?;
            }
        }
        write_txn.commit()?;

        Ok(true)
    }

    fn merge_deltas(&mut self) {
        for relation in self.relations.clone() {
            let _ = self.promote_tier(TIER_DELTA, TIER_STABLE, &relation);
            let _ = self.promote_tier(TIER_NEXT_DELTA, TIER_DELTA, &relation);
        }
        let _ = self.save_meta();
    }

    fn create_relation(&mut self, relation: &str) {
        if self.relations.insert(relation.to_string()) {
            self.next_ids.entry(relation.to_string()).or_insert(0);
            // Create the stable primary table so scans on an empty relation
            // don't hit TableDoesNotExist on the very first call.
            let table_name = primary_table_for(TIER_STABLE, relation);
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
        for tier in [TIER_STABLE, TIER_DELTA, TIER_NEXT_DELTA] {
            if self.retract_from_tier(tier, relation, tuple)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn clear(&mut self, relation: &str) {
        let _ = self.drop_tier_tables(TIER_STABLE, relation);
        let _ = self.drop_tier_tables(TIER_DELTA, relation);
        let _ = self.drop_tier_tables(TIER_NEXT_DELTA, relation);
    }

    fn relation_names(&self) -> Vec<String> {
        self.relations.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(tuple: Vec<Value>) -> Vec<Value> {
        let bytes = serialize_tuple(&tuple);
        deserialize_tuple(&bytes).expect("roundtrip should succeed")
    }

    #[test]
    fn test_tuple_roundtrip_number() {
        let t = vec![Value::Number(0), Value::Number(-1), Value::Number(i64::MAX)];
        assert_eq!(roundtrip(t.clone()), t);
    }

    #[test]
    fn test_tuple_roundtrip_float_incl_nan() {
        let nan_payload = f64::from_bits(0x7ff8_0000_1234_5678);
        let t = vec![
            Value::Float(1.5),
            Value::Float(-0.0),
            Value::Float(f64::INFINITY),
            Value::Float(f64::NEG_INFINITY),
            Value::Float(nan_payload),
        ];
        let out = roundtrip(t.clone());
        // Float PartialEq on Value compares by bits, so NaN payload must match.
        assert_eq!(out, t);
        if let (Value::Float(a), Value::Float(b)) = (&out[4], &t[4]) {
            assert_eq!(a.to_bits(), b.to_bits(), "NaN payload must roundtrip bit-exact");
        } else {
            panic!("expected Float at index 4");
        }
    }

    #[test]
    fn test_tuple_roundtrip_string_and_name() {
        let t = vec![
            Value::String("hello".into()),
            Value::Name("/foo/bar".into()),
            Value::String("".into()),
        ];
        assert_eq!(roundtrip(t.clone()), t);
    }

    #[test]
    fn test_tuple_roundtrip_time_duration() {
        let t = vec![Value::Time(1_700_000_000_123_456_789), Value::Duration(-42)];
        assert_eq!(roundtrip(t.clone()), t);
    }

    #[test]
    fn test_tuple_roundtrip_compound_all_kinds() {
        let t = vec![
            Value::Compound(CompoundKind::List, vec![Value::Number(1), Value::Number(2)]),
            Value::Compound(
                CompoundKind::Pair,
                vec![Value::String("k".into()), Value::Number(3)],
            ),
            Value::Compound(
                CompoundKind::Map,
                vec![
                    Value::Name("/key1".into()),
                    Value::Number(1),
                    Value::Name("/key2".into()),
                    Value::Number(2),
                ],
            ),
            Value::Compound(
                CompoundKind::Struct,
                vec![Value::Name("/x".into()), Value::Number(10)],
            ),
        ];
        assert_eq!(roundtrip(t.clone()), t);
    }

    #[test]
    fn test_tuple_roundtrip_nested_compound() {
        let inner = Value::Compound(CompoundKind::List, vec![Value::Number(1)]);
        let t = vec![Value::Compound(CompoundKind::List, vec![inner.clone(), inner])];
        assert_eq!(roundtrip(t.clone()), t);
    }

    #[test]
    fn test_tuple_roundtrip_null_and_mixed() {
        let t = vec![
            Value::Null,
            Value::Number(1),
            Value::Null,
            Value::String("end".into()),
        ];
        assert_eq!(roundtrip(t.clone()), t);
    }

    #[test]
    fn test_tuple_roundtrip_empty() {
        assert_eq!(roundtrip(vec![]), Vec::<Value>::new());
    }

    /// Simulate a legacy DB: meta table has relation entries but no recorded
    /// format version. Reopening should bail.
    #[test]
    fn test_open_rejects_legacy_db() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("legacy.redb");

        // Hand-build a legacy-shaped DB without going through DiskStore::open.
        {
            let db = Database::create(&db_path)?;
            let write_txn = db.begin_write()?;
            {
                let mut meta = write_txn.open_table(META_TABLE)?;
                meta.insert("some_relation", 0u64)?;
                // Note: FORMAT_TABLE is intentionally not populated.
            }
            write_txn.commit()?;
        }

        let err = DiskStore::open(&db_path).err().expect("open should fail");
        assert!(
            err.to_string().contains("legacy JSON"),
            "expected legacy-format error, got: {err}"
        );
        Ok(())
    }

    /// A DB with a format version we don't recognize must be rejected on open.
    #[test]
    fn test_open_rejects_unknown_format_version() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("future.redb");

        {
            let db = Database::create(&db_path)?;
            let write_txn = db.begin_write()?;
            {
                let mut format = write_txn.open_table(FORMAT_TABLE)?;
                format.insert(FORMAT_VERSION_KEY, 9999u64)?;
            }
            write_txn.commit()?;
        }

        let err = DiskStore::open(&db_path).err().expect("open should fail");
        assert!(
            err.to_string()
                .contains("database tuple format version is 9999"),
            "expected unknown-version error, got: {err}"
        );
        Ok(())
    }

    /// A fresh DB created by DiskStore::open must reopen cleanly with the
    /// matching format version recorded.
    #[test]
    fn test_open_records_format_version_on_fresh_db() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("fresh.redb");
        {
            let mut store = DiskStore::open(&db_path)?;
            store.create_relation("r");
            store.insert("r", vec![Value::Number(1)])?;
            store.merge_deltas();
        }
        // Reopen — should succeed, no format errors.
        let store = DiskStore::open(&db_path)?;
        let facts: Vec<_> = store.scan("r")?.collect();
        assert_eq!(facts.len(), 1);
        Ok(())
    }

    #[test]
    fn test_deserialize_rejects_empty() {
        let err = deserialize_tuple(&[]).unwrap_err();
        assert!(
            err.to_string().contains("failed to decode tuple"),
            "got: {err}"
        );
    }

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

    /// A sorted multiset of tuples — scan order is not stable across tiers, so
    /// tests that care about content (not order) must sort.
    fn sorted(mut v: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
        v.sort();
        v
    }

    #[test]
    fn test_scan_index_returns_only_matching_rows() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("idx.redb");
        let mut store = DiskStore::open(&db_path)?;
        store.create_relation("edge");

        store.insert("edge", vec![Value::Number(1), Value::Number(2)])?;
        store.insert("edge", vec![Value::Number(1), Value::Number(3)])?;
        store.insert("edge", vec![Value::Number(2), Value::Number(3)])?;
        store.merge_deltas();
        store.merge_deltas();

        // Index on column 0, value 1 → two matching rows.
        let got: Vec<_> = store.scan_index("edge", 0, &Value::Number(1))?.collect();
        assert_eq!(
            sorted(got),
            sorted(vec![
                vec![Value::Number(1), Value::Number(2)],
                vec![Value::Number(1), Value::Number(3)],
            ])
        );

        // Index on column 1, value 3 → two matching rows.
        let got: Vec<_> = store.scan_index("edge", 1, &Value::Number(3))?.collect();
        assert_eq!(
            sorted(got),
            sorted(vec![
                vec![Value::Number(1), Value::Number(3)],
                vec![Value::Number(2), Value::Number(3)],
            ])
        );

        // Non-matching key → empty.
        let got: Vec<_> = store.scan_index("edge", 0, &Value::Number(999))?.collect();
        assert!(got.is_empty());

        Ok(())
    }

    #[test]
    fn test_scan_index_across_all_value_variants() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("variants.redb");
        let mut store = DiskStore::open(&db_path)?;
        store.create_relation("r");

        let compound = Value::Compound(
            CompoundKind::Struct,
            vec![Value::Name("/f".into()), Value::Number(7)],
        );
        let rows = vec![
            vec![Value::Number(42)],
            vec![Value::Float(1.25)],
            vec![Value::String("hello".into())],
            vec![Value::Name("/foo".into())],
            vec![Value::Time(1_700_000_000)],
            vec![Value::Duration(-1)],
            vec![compound.clone()],
            vec![Value::Null],
        ];
        for r in &rows {
            store.insert("r", r.clone())?;
        }
        store.merge_deltas();
        store.merge_deltas();

        for r in &rows {
            let got: Vec<_> = store.scan_index("r", 0, &r[0])?.collect();
            assert_eq!(got, vec![r.clone()], "variant lookup failed: {:?}", r[0]);
        }
        Ok(())
    }

    #[test]
    fn test_scan_delta_index_is_delta_only() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("delta.redb");
        let mut store = DiskStore::open(&db_path)?;
        store.create_relation("r");

        // Two merges → row is in stable.
        store.insert("r", vec![Value::Number(1), Value::String("s".into())])?;
        store.merge_deltas();
        store.merge_deltas();

        // One more insert + one merge → row is in delta.
        store.insert("r", vec![Value::Number(2), Value::String("d".into())])?;
        store.merge_deltas();

        let in_all: Vec<_> = store.scan_index("r", 0, &Value::Number(1))?.collect();
        assert_eq!(in_all.len(), 1, "stable row must be reachable via scan_index");

        let in_delta_only: Vec<_> = store
            .scan_delta_index("r", 0, &Value::Number(1))?
            .collect();
        assert!(
            in_delta_only.is_empty(),
            "stable row must not surface in scan_delta_index"
        );

        let delta_row: Vec<_> = store
            .scan_delta_index("r", 0, &Value::Number(2))?
            .collect();
        assert_eq!(delta_row.len(), 1);
        Ok(())
    }

    #[test]
    fn test_retract_removes_from_index() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("retract.redb");
        let mut store = DiskStore::open(&db_path)?;
        store.create_relation("r");

        store.insert("r", vec![Value::Number(1), Value::Number(10)])?;
        store.insert("r", vec![Value::Number(1), Value::Number(20)])?;
        store.merge_deltas();
        store.merge_deltas();

        assert!(store.retract("r", &[Value::Number(1), Value::Number(10)])?);

        let via_col0: Vec<_> = store.scan_index("r", 0, &Value::Number(1))?.collect();
        assert_eq!(via_col0, vec![vec![Value::Number(1), Value::Number(20)]]);

        let via_col1: Vec<_> = store.scan_index("r", 1, &Value::Number(10))?.collect();
        assert!(
            via_col1.is_empty(),
            "retracted row must be absent from every column's index"
        );

        let still_there: Vec<_> = store.scan_index("r", 1, &Value::Number(20))?.collect();
        assert_eq!(
            still_there,
            vec![vec![Value::Number(1), Value::Number(20)]]
        );
        Ok(())
    }

    #[test]
    fn test_index_matches_filtered_full_scan() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("consistency.redb");
        let mut store = DiskStore::open(&db_path)?;
        store.create_relation("t");

        // Three-column relation with deliberate value overlap across columns.
        let tuples = vec![
            vec![Value::Number(1), Value::String("a".into()), Value::Number(7)],
            vec![Value::Number(2), Value::String("a".into()), Value::Number(7)],
            vec![Value::Number(2), Value::String("b".into()), Value::Number(8)],
            vec![Value::Number(3), Value::String("b".into()), Value::Number(7)],
        ];
        for t in &tuples {
            store.insert("t", t.clone())?;
        }
        store.merge_deltas();
        store.insert(
            "t",
            vec![Value::Number(1), Value::String("b".into()), Value::Number(9)],
        )?;
        store.merge_deltas();

        let all: Vec<_> = store.scan("t")?.collect();
        assert_eq!(all.len(), 5);

        let queries: Vec<(usize, Value)> = vec![
            (0, Value::Number(1)),
            (0, Value::Number(2)),
            (1, Value::String("a".into())),
            (1, Value::String("b".into())),
            (2, Value::Number(7)),
            (2, Value::Number(9)),
        ];
        for (col, key) in queries {
            let via_index: Vec<_> = store.scan_index("t", col, &key)?.collect();
            let via_scan: Vec<_> = all
                .iter()
                .filter(|row| row.get(col) == Some(&key))
                .cloned()
                .collect();
            assert_eq!(
                sorted(via_index),
                sorted(via_scan),
                "index disagrees with filtered scan for col={col}, key={key:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_crash_safety_no_partial_index() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("crash.redb");
        {
            let mut store = DiskStore::open(&db_path)?;
            store.create_relation("r");
            store.insert("r", vec![Value::Number(1), Value::Number(2)])?;
            store.insert("r", vec![Value::Number(3), Value::Number(4)])?;
            store.merge_deltas();
            store.merge_deltas();
            // Simulate crash: drop the store without an explicit flush beyond
            // the per-op commits already performed. redb's mmap + WAL guarantees
            // each committed op is durable; in-progress ones are discarded.
        }

        let store = DiskStore::open(&db_path)?;
        // Every committed row must be fully reachable via every column's index.
        let got: Vec<_> = store.scan_index("r", 0, &Value::Number(1))?.collect();
        assert_eq!(got, vec![vec![Value::Number(1), Value::Number(2)]]);
        let got: Vec<_> = store.scan_index("r", 1, &Value::Number(4))?.collect();
        assert_eq!(got, vec![vec![Value::Number(3), Value::Number(4)]]);
        Ok(())
    }

    #[test]
    fn test_insert_dedup_uses_index_path() -> Result<()> {
        // Not a direct observation of the index being used, but a functional
        // check: dedup behavior is preserved after the index-driven rewrite.
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("dedup.redb");
        let mut store = DiskStore::open(&db_path)?;
        store.create_relation("r");

        assert!(store.insert("r", vec![Value::Number(1), Value::Number(2)])?);
        assert!(!store.insert("r", vec![Value::Number(1), Value::Number(2)])?);
        store.merge_deltas();
        assert!(!store.insert("r", vec![Value::Number(1), Value::Number(2)])?);
        store.merge_deltas();
        assert!(!store.insert("r", vec![Value::Number(1), Value::Number(2)])?);

        let all: Vec<_> = store.scan("r")?.collect();
        assert_eq!(all.len(), 1);
        Ok(())
    }

    #[test]
    fn test_clear_removes_index_tables() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("clear.redb");
        let mut store = DiskStore::open(&db_path)?;
        store.create_relation("r");
        store.insert("r", vec![Value::Number(1), Value::Number(2)])?;
        store.insert("r", vec![Value::Number(3), Value::Number(4)])?;
        store.merge_deltas();
        store.merge_deltas();

        store.clear("r");

        let got: Vec<_> = store.scan_index("r", 0, &Value::Number(1))?.collect();
        assert!(got.is_empty(), "after clear, index must return nothing");
        let got: Vec<_> = store.scan("r")?.collect();
        assert!(got.is_empty(), "after clear, primary must be empty");
        Ok(())
    }
}
