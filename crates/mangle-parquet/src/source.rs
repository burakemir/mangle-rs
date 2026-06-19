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

//! [`ParquetEdbSource`] — plain Parquet file EDB source with row-group pruning.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Result, anyhow};
use arrow_array::RecordBatch;
use arrow_schema::{DataType, Schema, SchemaRef, TimeUnit};
use log::{debug, warn};
use mangle_common::Value;
use mangle_db::{ColumnPredicate, EdbSource, Fingerprint, PredicateOp, RelationInfo};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::metadata::ParquetMetaData;

use crate::convert::record_batch_to_values;

/// An EDB source backed by one or more plain Parquet files.
///
/// `open()` accepts either a single `.parquet` file or a directory containing
/// `.parquet` files (read as the union of all files, in sorted filename order).
/// All files are assumed to share the schema of the first file.
///
/// # Predicate pushdown
///
/// `scan_with_predicates()` exploits Parquet's per-row-group min/max column
/// statistics to skip entire row groups that cannot satisfy a predicate
/// (row-group pruning). Pruning is **best-effort and always safe**: when
/// statistics are missing, the column type is not supported for pruning, or the
/// predicate/value types are not order-comparable, the row group is read and the
/// predicate is re-checked in memory. The Mangle runtime re-checks all
/// predicates in memory regardless, so correctness is never compromised.
///
/// Only *flat* (all-primitive-column) schemas are eligible for pruning; for
/// schemas containing nested columns (struct/list/map) the source falls back to
/// reading every row group. Nested columns are still converted correctly — they
/// simply don't participate in pruning.
///
/// # Type mapping
///
/// Uses the shared [`convert`](crate::convert) module, so Parquet data is mapped
/// to Mangle [`Value`]s exactly as Delta Lake data is (see the crate-level docs).
pub struct ParquetEdbSource {
    /// Parquet files to read, in the order they are scanned.
    paths: Vec<PathBuf>,
    /// The Mangle relation name for this source.
    relation_name: String,
    /// Column names in order (from the first file's schema).
    column_names: Vec<String>,
    /// Cached Arrow schema (from the first file).
    schema: SchemaRef,
}

impl ParquetEdbSource {
    /// Open a Parquet file or a directory of `.parquet` files as an EDB source.
    ///
    /// If `path` is a directory, every `*.parquet` file directly inside it is
    /// included (non-recursive), scanned in sorted filename order. If `path` is
    /// a single file, only that file is read.
    ///
    /// The `relation_name` is the name under which the file's rows appear as a
    /// Mangle relation (e.g. `"orders"`).
    pub fn open(path: impl AsRef<Path>, relation_name: &str) -> Result<Self> {
        let path = path.as_ref();
        let paths = if path.is_dir() {
            let mut ps: Vec<PathBuf> = std::fs::read_dir(path)?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "parquet"))
                .collect();
            ps.sort();
            if ps.is_empty() {
                return Err(anyhow!(
                    "no .parquet files found in directory {}",
                    path.display()
                ));
            }
            ps
        } else if path.is_file() {
            vec![path.to_path_buf()]
        } else {
            return Err(anyhow!(
                "parquet path does not exist: {}",
                path.display()
            ));
        };

        Self::from_files(paths, relation_name)
    }

    /// Create an EDB source from an explicit list of Parquet files.
    ///
    /// The schema is read from the first file; all files are assumed to share
    /// it. Files are scanned in the given order.
    pub fn from_files(paths: Vec<PathBuf>, relation_name: &str) -> Result<Self> {
        if paths.is_empty() {
            return Err(anyhow!("no parquet files provided"));
        }
        let schema = read_schema(&paths[0])?;
        let column_names: Vec<String> = schema
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect();

        debug!(
            "Opened {} parquet file(s) as Mangle relation '{}': {} columns {:?}",
            paths.len(),
            relation_name,
            column_names.len(),
            column_names,
        );

        Ok(Self {
            paths,
            relation_name: relation_name.to_string(),
            column_names,
            schema,
        })
    }

    /// Returns the column names in order.
    pub fn column_names(&self) -> &[String] {
        &self.column_names
    }

    /// Returns the Arrow schema used for this source.
    pub fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    /// Returns the underlying file paths.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Read every row group of a file into RecordBatches.
    fn read_file(path: &Path) -> Result<Vec<RecordBatch>> {
        read_file_row_groups(path, None)
    }
}

impl EdbSource for ParquetEdbSource {
    fn name(&self) -> &str {
        &self.relation_name
    }

    fn relations(&self) -> Result<Vec<RelationInfo>> {
        let mut total: usize = 0;
        for path in &self.paths {
            match file_num_rows(path) {
                Ok(n) => total += n,
                Err(e) => {
                    warn!("failed to read row count from {}: {}", path.display(), e);
                }
            }
        }
        Ok(vec![RelationInfo {
            name: self.relation_name.clone(),
            estimated_rows: total,
        }])
    }

    fn scan(&self, relation: &str) -> Result<Vec<Vec<Value>>> {
        if relation != self.relation_name {
            return Ok(vec![]);
        }
        debug!(
            "ParquetEdbSource::scan('{}') — full scan of {} file(s)",
            relation,
            self.paths.len(),
        );
        warn!(
            "Performing a full Parquet scan on '{}'. For large files, prefer \
             scan_with_predicates() via Mangle rules that filter EDB columns so \
             row-group pruning can apply.",
            relation,
        );
        let mut out = Vec::new();
        for path in &self.paths {
            for batch in Self::read_file(path)? {
                out.extend(record_batch_to_values(&batch));
            }
        }
        Ok(out)
    }

    fn scan_with_predicates(
        &self,
        relation: &str,
        predicates: &[ColumnPredicate],
    ) -> Result<Vec<Vec<Value>>> {
        if relation != self.relation_name {
            return Ok(vec![]);
        }
        if predicates.is_empty() {
            return self.scan(relation);
        }

        let flat = self.schema.fields().iter().all(|f| !is_nested(f.data_type()));

        let mut out: Vec<Vec<Value>> = Vec::new();
        for path in &self.paths {
            let file = std::fs::File::open(path)?;
            let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;

            let selected = if flat {
                select_row_groups(builder.metadata(), &self.schema, predicates)
            } else {
                debug!(
                    "schema contains nested columns; skipping row-group pruning for '{}'",
                    path.display()
                );
                (0..builder.metadata().num_row_groups()).collect()
            };

            let pruned = builder.metadata().num_row_groups() - selected.len();
            debug!(
                "ParquetEdbSource: '{}' — {} of {} row groups selected ({} pruned)",
                path.display(),
                selected.len(),
                selected.len() + pruned,
                pruned,
            );

            let reader = builder.with_row_groups(selected).build()?;
            for batch in reader {
                let batch = batch?;
                for row in record_batch_to_values(&batch) {
                    if predicates.iter().all(|p| p.eval(&row)) {
                        out.push(row);
                    }
                }
            }
        }

        debug!(
            "ParquetEdbSource: {} rows after row-group pruning + in-memory re-check",
            out.len(),
        );
        Ok(out)
    }

    fn fingerprint(&self) -> Result<Option<Fingerprint>> {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        let mut any = false;
        for path in &self.paths {
            let meta = match std::fs::metadata(path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            any = true;
            hasher.update(path.to_string_lossy().as_bytes());
            hasher.update(meta.len().to_le_bytes());
            if let Ok(mtime) = meta.modified() {
                let secs = mtime
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                hasher.update(secs.to_le_bytes());
            }
        }
        if !any {
            return Ok(None);
        }
        Ok(Some(Fingerprint(hasher.finalize().to_vec())))
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Read the Arrow schema from a Parquet file without reading row data.
fn read_schema(path: &Path) -> Result<SchemaRef> {
    let file = std::fs::File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    Ok(builder.schema().clone())
}

/// Total number of rows across all row groups of a Parquet file.
fn file_num_rows(path: &Path) -> Result<usize> {
    let file = std::fs::File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    Ok(builder.metadata().file_metadata().num_rows() as usize)
}

/// Read specific row groups of a file into RecordBatches.
///
/// `row_groups = None` reads all row groups.
fn read_file_row_groups(path: &Path, row_groups: Option<&[usize]>) -> Result<Vec<RecordBatch>> {
    let file = std::fs::File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let builder = match row_groups {
        Some(rgs) => builder.with_row_groups(rgs.to_vec()),
        None => builder,
    };
    let reader = builder.build()?;
    let mut batches = Vec::new();
    for batch in reader {
        batches.push(batch?);
    }
    Ok(batches)
}

/// Decide which row groups of a file may contain rows matching all predicates.
///
/// A row group is dropped only when a predicate's value provably lies outside
/// the row group's min/max statistics for that column. Any uncertainty (missing
/// stats, unsupported type, non-comparable value variants, nested schema) keeps
/// the row group — pruning never produces false negatives.
fn select_row_groups(
    metadata: &ParquetMetaData,
    schema: &Schema,
    predicates: &[ColumnPredicate],
) -> Vec<usize> {
    let row_groups = metadata.row_groups();
    (0..row_groups.len())
        .filter(|&rg_idx| {
            let rg = &row_groups[rg_idx];
            for p in predicates {
                if p.col_idx >= rg.num_columns() {
                    continue;
                }
                let field = match schema.fields().get(p.col_idx) {
                    Some(f) => f,
                    None => continue,
                };
                if is_nested(field.data_type()) {
                    continue;
                }
                let col = rg.column(p.col_idx);
                let stats = match col.statistics() {
                    Some(s) => s,
                    None => continue,
                };
                let (min_bytes, max_bytes) = match (stats.min_bytes_opt(), stats.max_bytes_opt()) {
                    (Some(a), Some(b)) => (a, b),
                    _ => continue,
                };
                let (min, max) = match (
                    stats_to_value(min_bytes, field.data_type()),
                    stats_to_value(max_bytes, field.data_type()),
                ) {
                    (Some(a), Some(b)) => (a, b),
                    _ => continue,
                };
                // Only prune when the predicate value is order-comparable with
                // the statistics values; otherwise keep the row group (safe).
                if !ord_compatible(&p.value, &min) || !ord_compatible(&p.value, &max) {
                    continue;
                }
                if !range_can_match(&min, &max, p) {
                    return false;
                }
            }
            true
        })
        .collect()
}

/// Whether a value in the inclusive range `[min, max]` could satisfy `pred`.
///
/// Sound: returns `false` only when no value in `[min, max]` can match.
fn range_can_match(min: &Value, max: &Value, pred: &ColumnPredicate) -> bool {
    let v = &pred.value;
    match pred.op {
        PredicateOp::Eq => min <= v && v <= max,
        // exists x in [min, max] with x  < v  ⟺  min < v
        PredicateOp::Lt => min < v,
        // exists x in [min, max] with x <= v  ⟺  min <= v
        PredicateOp::Le => min <= v,
        // exists x in [min, max] with x  > v  ⟺  max > v
        PredicateOp::Gt => max > v,
        // exists x in [min, max] with x >= v  ⟺  max >= v
        PredicateOp::Ge => max >= v,
        // Cannot prune on inequality in general.
        PredicateOp::Neq => true,
    }
}

/// Whether `a` and `b` are compared by *value* (not by the arbitrary
/// cross-variant discriminant ordering) under `Value`'s `Ord` impl.
///
/// This guards pruning against comparing incomparable variants (e.g. a
/// `Value::Name` predicate against `Value::String` statistics), which would use
/// the meaningless discriminant fallback and could wrongly drop a row group.
fn ord_compatible(a: &Value, b: &Value) -> bool {
    use Value::*;
    matches!(
        (a, b),
        (Number(_), Number(_) | Float(_) | Time(_) | Duration(_))
            | (Float(_), Number(_) | Float(_))
            | (String(_), String(_))
            | (Name(_), Name(_))
            | (Time(_), Number(_) | Time(_))
            | (Duration(_), Number(_) | Duration(_))
            | (Compound(_, _), Compound(_, _))
            | (Null, Null)
    )
}

/// Decode a Parquet min/max statistic (raw value bytes) into a Mangle `Value`,
/// keyed on the Arrow column type so the result matches `convert`'s mapping.
///
/// Returns `None` for types whose statistics cannot be safely decoded (e.g.
/// `Date64`, `UInt64`, decimals, binary) — callers treat `None` as "no pruning".
fn stats_to_value(bytes: &[u8], dt: &DataType) -> Option<Value> {
    match dt {
        DataType::Boolean => bytes
            .first()
            .map(|&b| Value::Number(if b != 0 { 1 } else { 0 })),
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::UInt8 | DataType::UInt16
        | DataType::UInt32 => read_i32(bytes).map(|v| Value::Number(v as i64)),
        DataType::Int64 => read_i64(bytes).map(Value::Number),
        DataType::Float32 => read_f32(bytes).map(|v| Value::Float(v as f64)),
        DataType::Float64 => read_f64(bytes).map(Value::Float),
        DataType::Utf8 | DataType::LargeUtf8 => {
            std::str::from_utf8(bytes).ok().map(|s| Value::String(s.to_string()))
        }
        DataType::Timestamp(unit, _) => read_i64(bytes).map(|v| Value::Time(scale_time(*unit, v))),
        DataType::Date32 => read_i32(bytes).map(|v| Value::Time(v as i64 * 86_400_000_000_000)),
        DataType::Duration(unit) => {
            read_i64(bytes).map(|v| Value::Duration(scale_time(*unit, v)))
        }
        // Date64 physical encoding is ambiguous across writers; UInt64 would
        // truncate (see convert's fallback); decimals/binary/nested are not
        // supported for pruning. Fall back to reading the row group.
        _ => None,
    }
}

fn scale_time(unit: TimeUnit, v: i64) -> i64 {
    match unit {
        TimeUnit::Second => v.saturating_mul(1_000_000_000),
        TimeUnit::Millisecond => v.saturating_mul(1_000_000),
        TimeUnit::Microsecond => v.saturating_mul(1_000),
        TimeUnit::Nanosecond => v,
    }
}

fn read_i32(b: &[u8]) -> Option<i32> {
    let a: [u8; 4] = b.get(..4)?.try_into().ok()?;
    Some(i32::from_le_bytes(a))
}

fn read_i64(b: &[u8]) -> Option<i64> {
    let a: [u8; 8] = b.get(..8)?.try_into().ok()?;
    Some(i64::from_le_bytes(a))
}

fn read_f32(b: &[u8]) -> Option<f32> {
    let a: [u8; 4] = b.get(..4)?.try_into().ok()?;
    Some(f32::from_le_bytes(a))
}

fn read_f64(b: &[u8]) -> Option<f64> {
    let a: [u8; 8] = b.get(..8)?.try_into().ok()?;
    Some(f64::from_le_bytes(a))
}

/// Whether a data type is a nested (non-leaf) Arrow type.
fn is_nested(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Struct(_)
            | DataType::List(_)
            | DataType::LargeList(_)
            | DataType::FixedSizeList(_, _)
            | DataType::Map(_, _)
            | DataType::Union(_, _)
            | DataType::Dictionary(_, _)
    )
}
