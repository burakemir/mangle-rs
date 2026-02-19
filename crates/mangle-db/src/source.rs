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

//! EDB source trait and supporting types.

use anyhow::Result;
use mangle_common::Value;

/// Metadata about a relation provided by an EDB source.
#[derive(Debug, Clone)]
pub struct RelationInfo {
    pub name: String,
    pub estimated_rows: usize,
}

/// A fingerprint for staleness detection.
/// Typically a SHA-256 hash of source metadata (file mtimes, sizes, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint(pub Vec<u8>);

/// Readonly provider of extensional (base) facts.
///
/// Implementations load facts from external sources (files, databases, etc.)
/// into the working store during `Database::open()`.
pub trait EdbSource: Send + Sync {
    /// A human-readable name for this source.
    fn name(&self) -> &str;

    /// Returns metadata about the relations this source provides.
    fn relations(&self) -> Result<Vec<RelationInfo>>;

    /// Returns all tuples for the given relation.
    fn scan(&self, relation: &str) -> Result<Vec<Vec<Value>>>;

    /// Returns a fingerprint for staleness detection.
    /// `None` means "always recompute" (no caching possible).
    fn fingerprint(&self) -> Result<Option<Fingerprint>>;
}
