// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS\" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! [`DeltaEdbSource`] — Delta Lake EDB source with predicate pushdown.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use arrow_array::RecordBatch;
use deltalake_core::{DeltaTable, PartitionFilter, PartitionValue};
use log::{debug, warn};
use mangle_common::Value;
use mangle_db::{ColumnPredicate, EdbSource, Fingerprint, PredicateOp, RelationInfo};
use url::Url;

use mangle_parquet::convert::record_batch_to_values;

/// An EDB source backed by a Delta Lake table.
///
/// **Read the [crate-level documentation](crate) before using this.**
/// Realistic Delta tables are too large for in-memory processing; this
/// source only works effectively with predicate pushdown.
///
/// # Predicate Pushdown
///
/// When `scan_with_predicates()` is called, this source translates
/// `ColumnPredicate`s into Delta Lake's native pushdown mechanisms:
///
/// 1. **Equality predicates on partition columns** → `PartitionFilter`
///    (eliminates entire directories of Parquet files).
/// 2. **All other predicates** → SQL `WHERE` clauses via DataFusion
///    (enables file-level data skipping and Parquet-level pushdown).
///
/// Predicates are **best-effort**: the source may return more rows than
/// strictly match. The Mangle runtime always re-checks predicates in-memory.
pub struct DeltaEdbSource {
    /// The loaded Delta table.
    table: DeltaTable,
    /// The Mangle relation name for this table.
    relation_name: String,
    /// Names of partition columns (for mapping predicates to PartitionFilters).
    partition_columns: Vec<String>,
    /// Cached schema: column names in order.
    column_names: Vec<String>,
    /// Mapping from column name → column index.
    #[allow(dead_code)]
    column_index: HashMap<String, usize>,
}

impl DeltaEdbSource {
    /// Open a Delta Lake table and create an EDB source for it.
    ///
    /// The `table_url` can be a local path (`file:///...`) or a cloud URL
    /// (`s3://...`, `gs://...`, `az://...`). Storage options are passed
    /// directly to `deltalake_core::open_table_with_storage_options()`.
    ///
    /// The `relation_name` is the name under which this table's rows will
    /// appear as a Mangle relation (e.g. `"orders"`).
    pub async fn open(
        table_url: &str,
        relation_name: &str,
        storage_options: HashMap<String, String>,
    ) -> Result<Self> {
        let url = Url::parse(table_url)
            .map_err(|e| anyhow!("invalid table URL '{}': {}", table_url, e))?;

        let table = if storage_options.is_empty() {
            deltalake_core::open_table(url).await?
        } else {
            deltalake_core::open_table_with_storage_options(url, storage_options).await?
        };

        Self::from_table(table, relation_name)
    }

    /// Create an EDB source from an already-loaded `DeltaTable`.
    ///
    /// The table must already be loaded (call `table.load().await?` before
    /// passing it). This constructor does not create its own Tokio runtime,
    /// so it can be called from within an async context.
    pub fn from_table(table: DeltaTable, relation_name: &str) -> Result<Self> {
        let snapshot = table.snapshot().map_err(|e| anyhow!("table not initialized: {}", e))?;
        let schema = snapshot.schema();

        // Get partition columns from the snapshot metadata
        let partition_columns: Vec<String> = snapshot
            .metadata()
            .partition_columns()
            .iter()
            .map(|s| s.clone())
            .collect();

        let column_names: Vec<String> = schema.fields().map(|f| f.name().clone()).collect();
        let column_index: HashMap<String, usize> = column_names
            .iter()
            .enumerate()
            .map(|(i, name)| (name.clone(), i))
            .collect();

        debug!(
            "Opened Delta table '{}' as Mangle relation '{}': {} columns, {} partitions {:?}",
            table.table_url(),
            relation_name,
            column_names.len(),
            partition_columns.len(),
            partition_columns,
        );

        Ok(Self {
            table,
            relation_name: relation_name.to_string(),
            partition_columns,
            column_names,
            column_index,
        })
    }

    /// Returns the column names in order.
    pub fn column_names(&self) -> &[String] {
        &self.column_names
    }

    /// Returns the partition column names.
    pub fn partition_columns(&self) -> &[String] {
        &self.partition_columns
    }

    /// Translate `ColumnPredicate`s into Delta Lake `PartitionFilter`s.
    ///
    /// Only equality predicates on partition columns are converted.
    /// Other predicates are handled via the DataFusion SQL WHERE clause.
    fn extract_partition_filters(&self, predicates: &[ColumnPredicate]) -> Vec<PartitionFilter> {
        let mut filters = Vec::new();
        for pred in predicates {
            if pred.op != PredicateOp::Eq {
                continue;
            }
            let col_name = match self.column_names.get(pred.col_idx) {
                Some(name) => name,
                None => continue,
            };
            if !self.partition_columns.contains(col_name) {
                continue;
            }
            let value_str = value_to_partition_string(&pred.value);
            filters.push(PartitionFilter {
                key: col_name.clone(),
                value: PartitionValue::Equal(value_str),
            });
        }
        filters
    }

    /// Read data via DataFusion's table provider, which handles Parquet
    /// reading, partition column injection, and schema reconciliation.
    ///
    /// If partition filters are provided, they are translated to SQL WHERE
    /// clauses so DataFusion can exploit Delta's file pruning.
    ///
    /// This method is synchronous (matching the `EdbSource` trait) but
    /// internally bridges to async code. It spawns a new thread to avoid
    /// the "cannot start a runtime from within a runtime" panic when
    /// called from an existing Tokio context.
    fn read_via_datafusion(
        &self,
        partition_filters: &[PartitionFilter],
    ) -> Result<Vec<Vec<Value>>> {
        use deltalake_core::datafusion::prelude::SessionContext;

        // Clone what we need and run the async code on a dedicated thread
        // to avoid tokio runtime nesting issues.
        let relation_name = self.relation_name.clone();
        let table = self.table.clone();
        let partition_filters = partition_filters.to_vec();

        let handle = std::thread::spawn(move || -> Result<Vec<Vec<Value>>> {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let ctx = SessionContext::new();

                // Register the Delta table
                let provider = table.table_provider().build().await?;
                ctx.register_table(&relation_name, Arc::new(provider))?;

                // Build SQL query
                let sql = if partition_filters.is_empty() {
                    format!("SELECT * FROM {}", relation_name)
                } else {
                    let where_clauses: Vec<String> = partition_filters
                        .iter()
                        .map(|f| match &f.value {
                            PartitionValue::Equal(v) => format!("{} = '{}'", f.key, v),
                            PartitionValue::NotEqual(v) => format!("{} != '{}'", f.key, v),
                            PartitionValue::In(vals) => {
                                let quoted: Vec<String> = vals.iter().map(|v| format!("'{}'", v)).collect();
                                format!("{} IN ({})", f.key, quoted.join(", "))
                            }
                            PartitionValue::NotIn(vals) => {
                                let quoted: Vec<String> = vals.iter().map(|v| format!("'{}'", v)).collect();
                                format!("{} NOT IN ({})", f.key, quoted.join(", "))
                            }
                            PartitionValue::LessThanOrEqual(v) => format!("{} <= '{}'", f.key, v),
                            PartitionValue::LessThan(v) => format!("{} < '{}'", f.key, v),
                            PartitionValue::GreaterThanOrEqual(v) => format!("{} >= '{}'", f.key, v),
                            PartitionValue::GreaterThan(v) => format!("{} > '{}'", f.key, v),
                        })
                        .collect();
                    format!(
                        "SELECT * FROM {} WHERE {}",
                        relation_name,
                        where_clauses.join(" AND ")
                    )
                };

                debug!("Delta scan SQL: {}", sql);

                let batches: Vec<RecordBatch> = ctx.sql(&sql).await?.collect().await?;

                let mut all_rows = Vec::new();
                for batch in batches {
                    all_rows.extend(record_batch_to_values(&batch));
                }

                Ok(all_rows)
            })
        });

        handle.join().map_err(|e| anyhow!("read thread panicked: {:?}", e))?
    }
}

impl EdbSource for DeltaEdbSource {
    fn name(&self) -> &str {
        &self.relation_name
    }

    fn relations(&self) -> Result<Vec<RelationInfo>> {
        let snapshot = self.table.snapshot().map_err(|e| anyhow!("table not loaded: {}", e))?;
        let estimated_rows = snapshot.log_data().num_files() * 100_000; // rough estimate
        Ok(vec![RelationInfo {
            name: self.relation_name.clone(),
            estimated_rows,
        }])
    }

    fn scan(&self, relation: &str) -> Result<Vec<Vec<Value>>> {
        if relation != self.relation_name {
            return Ok(vec![]);
        }
        debug!(
            "DeltaEdbSource::scan('{}') — full table scan (no predicates)",
            relation
        );
        warn!(
            "Performing full Delta table scan on '{}'. \
             For large tables, use scan_with_predicates() with Mangle rules \
             that include filters on EDB columns.",
            relation
        );
        self.read_via_datafusion(&[])
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

        // Step 1: Extract partition filters from equality predicates
        // on partition columns
        let partition_filters = self.extract_partition_filters(predicates);

        debug!(
            "DeltaEdbSource::scan_with_predicates('{}') — {} predicates, \
             {} partition filters: {:?}",
            relation,
            predicates.len(),
            partition_filters.len(),
            partition_filters,
        );

        // Step 2: Read data with partition pruning
        let rows = self.read_via_datafusion(&partition_filters)?;

        // Step 3: Re-check all predicates in memory (the pushdown may have
        // been approximate — partition pruning is exact for equality, but
        // data skipping can produce false positives).
        let filtered: Vec<Vec<Value>> = rows
            .into_iter()
            .filter(|row| predicates.iter().all(|p| p.eval(row)))
            .collect();

        debug!(
            "DeltaEdbSource: {} rows after pushdown + re-check",
            filtered.len(),
        );

        Ok(filtered)
    }

    fn fingerprint(&self) -> Result<Option<Fingerprint>> {
        // Use the Delta table version as a fingerprint
        let version = self.table.version().unwrap_or(0);
        Ok(Some(Fingerprint(version.to_le_bytes().to_vec())))
    }
}

/// Convert a Mangle `Value` to a partition filter string.
///
/// Delta Lake stores partition values as strings in the transaction log,
/// so we need to convert Mangle values to their string representation.
fn value_to_partition_string(value: &Value) -> String {
    match value {
        Value::Number(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::String(s) => s.clone(),
        Value::Name(s) => s.clone(),
        Value::Time(nanos) => format!("{}", Value::Time(*nanos)),
        Value::Duration(nanos) => format!("{}", Value::Duration(*nanos)),
        Value::Null => String::new(),
        Value::Compound(_, _) => String::new(),
    }
}
