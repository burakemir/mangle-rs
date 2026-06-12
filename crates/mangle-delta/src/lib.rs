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

//! # Mangle Delta — Delta Lake adapter with predicate pushdown
//!
//! This crate provides a [`DeltaEdbSource`] that implements Mangle's
//! [`EdbSource`](mangle_db::EdbSource) trait, enabling Mangle programs to read
//! from [Delta Lake](https://delta.io/) tables as extensional (EDB) facts.
//!
//! ## Why predicate pushdown is mandatory
//!
//! **A realistic Delta Lake table is far too large for Mangle's edge-mode
//! in-memory processing.** Delta tables routinely contain millions to billions
//! of rows across hundreds or thousands of Parquet files. Mangle's Edge Mode
//! stores all facts in a `MemStore` (a `HashMap` in RAM). Loading an entire
//! Delta table into memory is neither feasible nor desirable.
//!
//! The only practical way to use Delta Lake with Mangle is through **predicate
//! pushdown**: the Mangle compiler analyzes the physical plan and extracts
//! column-level predicates (equality filters, range comparisons) that are
//! *always* applied to every row from an EDB relation. These predicates are
//! passed to `DeltaEdbSource::scan_with_predicates()`, which exploits Delta
//! Lake's multi-level pushdown capabilities:
//!
//! | Pushdown level | Mechanism | What it skips |
//! |---|---|---|
//! | **Partition pruning** | `PartitionFilter` on partition columns | Entire directories (Parquet files in non-matching partitions) |
//! | **File-level data skipping** | Min/max/null statistics in Delta log | Entire Parquet files whose stats don't match the predicate |
//! | **Row-group pruning** | Parquet row-group min/max stats | Row groups within a Parquet file |
//! | **Parquet predicate pushdown** | Page-level min/max in Parquet | Individual data pages |
//!
//! Even basic partition pruning can eliminate 99%+ of I/O on partitioned
//! tables. Without predicates, `scan_with_predicates()` falls back to a full
//! table scan — which for a realistic Delta table means reading gigabytes of
//! Parquet and converting it all to `Value` objects. **This will exhaust
//! memory and fail.**
//!
//! ## How it works
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//!  Mangle source program                                           │
//!  result(X) :- orders(X, Y, /region, "US"), Y > 1000.            │
//! └───────────────────────┬─────────────────────────────────────────┘
//!                          │ compile
//!                          ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//!  Physical plan (Op tree)                                         │
//!  Iterate { source: Scan("orders", [X,Y,R]),                     │
//!    body: Filter { cond: R = "US" ∧ Y > 1000, ... } }           │
//! └───────────────────────┬─────────────────────────────────────────┘
//!                          │ predicate extraction
//!                          ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//!  Extracted ColumnPredicates for "orders":                        │
//!    [Eq(2, "US"), Gt(1, 1000)]                                   │
//! └───────────────────────┬─────────────────────────────────────────┘
//!                          │ scan_with_predicates()
//!                          ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//!  DeltaEdbSource translates to:                                   │
//!    PartitionFilter: region = "US"     → partition pruning        │
//!    ScanBuilder::with_predicate(Y>1000) → data skipping + Parquet│
//! └───────────────────────┬─────────────────────────────────────────┘
//!                          │ read only matching Parquet files
//!                          ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//!  Arrow RecordBatches → Vec<Vec<Value>>                          │
//!  (only rows matching the predicates)                             │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Usage
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use mangle_db::{Database, DatabaseConfig, IdbMode, RecomputeStrategy, StoreBackend};
//! use mangle_delta::DeltaEdbSource;
//!
//! async fn example() -> anyhow::Result<()> {
//!     // Create a Delta EDB source pointing at a Delta table
//!     let orders_source = DeltaEdbSource::open(
//!         "s3://my-bucket/delta/orders",   // Delta table location
//!         "orders",                         // Mangle relation name
//!         std::collections::HashMap::new(), // storage options
//!     ).await?;
//!
//!     let config = DatabaseConfig {
//!         name: "analytics".to_string(),
//!         source: r#"
//!             # The "orders" relation comes from Delta Lake.
//!             # The compiler will extract predicates from this rule
//!             # and pass them to DeltaEdbSource for pushdown.
//!             large_us_orders(Id, Amount) :- orders(Id, Amount, /region, "US"),
//!                                                Amount > 1000.
//!         "#.to_string(),
//!         edb_sources: vec![Arc::new(orders_source)],
//!         idb_mode: IdbMode::InMemory,
//!         recompute: RecomputeStrategy::Full,
//!         store_backend: StoreBackend::InMemory,
//!     };
//!
//!     let db = Database::open(config)?;
//!     let results = db.query("large_us_orders")?;
//!     Ok(())
//! }
//! ```
//!
//! ## Type mapping (Arrow → Mangle)
//!
//! | Arrow type | Mangle `Value` |
//! |---|---|
//! | `Int8/16/32/64` | `Value::Number(i64)` |
//! | `UInt8/16/32` | `Value::Number(i64)` |
//! | `UInt64` | `Value::Number(i64)` (truncated) |
//! | `Float32/64` | `Value::Float(f64)` |
//! | `Utf8/LargeUtf8` | `Value::String(String)` |
//! | `Timestamp(_, _)` | `Value::Time(i64)` (nanoseconds) |
//! | `Date32/64` | `Value::Time(i64)` (nanoseconds since epoch) |
//! | `Duration(_)` | `Value::Duration(i64)` (nanoseconds) |
//! | `Boolean` | `Value::Number(0 or 1)` |
//! | `Null` | `Value::Null` |
//!
//! ## Limitations
//!
//! - **Write path**: This adapter is read-only. Writing Mangle-derived facts
//!   back to Delta Lake requires a separate `Store` implementation (not yet
//!   implemented).
//! - **Async/sync bridge**: Delta Lake's API is async. This adapter uses
//!   `tokio::runtime::Handle::block_on()` to bridge to Mangle's synchronous
//!   `EdbSource` trait. You must have a Tokio runtime available.
//! - **Nested types**: Struct and list columns are converted to
//!   `Value::Compound(...)`. Map columns are not yet supported.
//! - **Schema evolution**: The adapter reads the schema at `open()` time.
//!   Schema changes after opening require re-creating the source.

mod convert;
mod source;

pub use source::DeltaEdbSource;
