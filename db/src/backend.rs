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

//! IDB backend trait and supporting types for persistent IDB caching.

use anyhow::Result;
use mangle_factstore::Value;
use serde::{Deserialize, Serialize};

/// Metadata about a cached IDB snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMeta {
    /// SHA-256 of the program source text.
    #[serde(with = "hex_array")]
    pub program_hash: [u8; 32],
    /// Combined fingerprint of all EDB sources.
    #[serde(with = "hex_vec")]
    pub edb_fingerprint: Vec<u8>,
    /// Unix timestamp when the cache was created.
    pub created_at: u64,
}

/// A snapshot of derived (IDB) facts.
pub struct IdbSnapshot {
    /// (relation_name, facts) pairs.
    pub relations: Vec<(String, Vec<Vec<Value>>)>,
}

/// Backend for persistent IDB caching.
///
/// Implementations store and retrieve IDB snapshots keyed by database name.
/// The `CacheMeta` is used to determine if a cached snapshot is still valid.
pub trait IdbBackend: Send + Sync {
    /// Load a cached IDB snapshot for the given database.
    /// Returns `None` if no cache exists.
    fn load(&self, db_name: &str) -> Result<Option<(CacheMeta, IdbSnapshot)>>;

    /// Save an IDB snapshot for the given database.
    fn save(&self, db_name: &str, meta: &CacheMeta, snapshot: &IdbSnapshot) -> Result<()>;

    /// Invalidate (delete) the cached IDB for the given database.
    fn invalidate(&self, db_name: &str) -> Result<()>;
}

mod hex_array {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        s.serialize_str(&hex)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let hex = String::deserialize(d)?;
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(serde::de::Error::custom))
            .collect::<Result<_, _>>()?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| serde::de::Error::custom("expected 32 bytes"))?;
        Ok(arr)
    }
}

mod hex_vec {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        s.serialize_str(&hex)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let hex = String::deserialize(d)?;
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(serde::de::Error::custom))
            .collect::<Result<_, _>>()?;
        Ok(bytes)
    }
}
