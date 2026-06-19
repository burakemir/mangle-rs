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

//! # Mangle Parquet — plain Parquet EDB source with row-group pruning
//!
//! This crate provides [`ParquetEdbSource`], which implements Mangle's
//! [`EdbSource`](mangle_db::EdbSource) trait to read plain Parquet files as
//! extensional (EDB) facts. It shares its Arrow → Mangle [`Value`] schema
//! mapping with the [`mangle-delta`](https://crates.io/crates/mangle-delta)
//! adapter via the [`convert`] module, so Parquet and Delta Lake data are
//! represented identically.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use mangle_db::{Database, DatabaseConfig, IdbMode, RecomputeStrategy, StoreBackend};
//! use mangle_parquet::ParquetEdbSource;
//!
//! fn example() -> anyhow::Result<()> {
//!     let source = ParquetEdbSource::open(
//!         "/data/orders.parquet", // or a directory of .parquet files
//!         "orders",
//!     )?;
//!
//!     let config = DatabaseConfig {
//!         name: "analytics".to_string(),
//!         source: r#"
//!             # The "orders" relation comes from Parquet. Filters on EDB
//!             # columns are pushed down as row-group pruning.
//!             big_orders(Id, Amount) :- orders(Id, Amount, /region, _),
//!                                        Amount > 1000.
//!         "#.to_string(),
//!         edb_sources: vec![Arc::new(source)],
//!         idb_mode: IdbMode::InMemory,
//!         recompute: RecomputeStrategy::Full,
//!         store_backend: StoreBackend::InMemory,
//!     };
//!
//!     let db = Database::open(config)?;
//!     let _results = db.query("big_orders")?;
//!     Ok(())
//! }
//! ```
//!
//! ## Predicate pushdown
//!
//! `scan_with_predicates()` uses Parquet's per-row-group min/max column
//! statistics to skip row groups that cannot satisfy a predicate. Pushdown is
//! best-effort and always correct: the Mangle runtime re-checks every predicate
//! in memory, so approximate pruning never affects results.
//!
//! Only flat (all-primitive) schemas are eligible for pruning; nested columns
//! are still converted correctly but do not participate in pruning.
//!
//! ## Type mapping (Arrow → Mangle)
//!
//! See the [`convert`] module for the full table. Integers → `Value::Number`,
//! floats → `Value::Float`, strings → `Value::String`, timestamps/dates →
//! `Value::Time` (ns), durations → `Value::Duration` (ns), booleans →
//! `Value::Number(0|1)`.

pub mod convert;

mod source;

pub use source::ParquetEdbSource;
