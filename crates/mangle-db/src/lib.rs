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

//! # Mangle DB
//!
//! Persistent fact stores for Mangle, providing EDB sources, IDB caching,
//! and a `Database` abstraction that manages compilation, execution, and
//! query serving.

pub mod backend;
pub mod database;
pub mod file_backend;
pub mod file_source;
pub mod predicate;
pub mod provenance;
pub mod simplerow;
pub mod source;

#[cfg(feature = "disk")]
pub mod disk_store;

pub use backend::{CacheMeta, IdbBackend, IdbSnapshot};
pub use database::{Database, DatabaseConfig, IdbMode, RecomputeStrategy, StoreBackend};
pub use file_backend::FileIdbBackend;
pub use file_source::FileEdbSource;
pub use predicate::extract_predicates;
pub use provenance::{Derivation, FactKey, ProvenanceIndex};
pub use source::{ColumnPredicate, EdbSource, Fingerprint, PredicateOp, RelationInfo};
