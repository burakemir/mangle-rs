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

//! File-based IDB backend: stores cached IDB snapshots as simplerow files.
//!
//! Layout in the cache directory:
//! - `{db_name}.meta.json` — CacheMeta (program hash, edb fingerprint, timestamp)
//! - `{db_name}.idb.mgr`  — IDB facts in simplerow format

use std::path::PathBuf;

use anyhow::Result;

use crate::backend::{CacheMeta, IdbBackend, IdbSnapshot};
use crate::simplerow;

/// File-based IDB cache backend.
pub struct FileIdbBackend {
    dir: PathBuf,
}

impl FileIdbBackend {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn meta_path(&self, db_name: &str) -> PathBuf {
        self.dir.join(format!("{db_name}.meta.json"))
    }

    fn idb_path(&self, db_name: &str) -> PathBuf {
        self.dir.join(format!("{db_name}.idb.mgr"))
    }
}

impl IdbBackend for FileIdbBackend {
    fn load(&self, db_name: &str) -> Result<Option<(CacheMeta, IdbSnapshot)>> {
        let meta_path = self.meta_path(db_name);
        let idb_path = self.idb_path(db_name);

        if !meta_path.exists() || !idb_path.exists() {
            return Ok(None);
        }

        let meta_json = std::fs::read_to_string(&meta_path)?;
        let meta: CacheMeta = serde_json::from_str(&meta_json)?;

        let idb_data = std::fs::read(&idb_path)?;
        let sr_data = simplerow::read_from_bytes(&idb_data)?;

        let relations: Vec<_> = sr_data.tables.into_iter().collect();
        let snapshot = IdbSnapshot { relations };

        Ok(Some((meta, snapshot)))
    }

    fn save(&self, db_name: &str, meta: &CacheMeta, snapshot: &IdbSnapshot) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;

        let meta_json = serde_json::to_string_pretty(meta)?;
        std::fs::write(self.meta_path(db_name), meta_json)?;

        let mut file = std::fs::File::create(self.idb_path(db_name))?;
        let tables: Vec<_> = snapshot
            .relations
            .iter()
            .map(|(name, facts)| (name.clone(), facts.clone()))
            .collect();
        simplerow::write_simple_row(&mut file, &tables)?;

        Ok(())
    }

    fn invalidate(&self, db_name: &str) -> Result<()> {
        let meta_path = self.meta_path(db_name);
        let idb_path = self.idb_path(db_name);

        if meta_path.exists() {
            std::fs::remove_file(&meta_path)?;
        }
        if idb_path.exists() {
            std::fs::remove_file(&idb_path)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangle_common::Value;

    #[test]
    fn test_file_idb_backend_round_trip() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let backend = FileIdbBackend::new(dir.path());

        // Initially empty
        assert!(backend.load("test")?.is_none());

        let meta = CacheMeta {
            program_hash: [0xAB; 32],
            edb_fingerprint: vec![0xCD; 32],
            created_at: 1234567890,
        };

        let snapshot = IdbSnapshot {
            relations: vec![(
                "derived".to_string(),
                vec![
                    vec![Value::Number(1), Value::Number(2)],
                    vec![Value::Number(3), Value::Number(4)],
                ],
            )],
        };

        backend.save("test", &meta, &snapshot)?;

        let (loaded_meta, loaded_snapshot) = backend.load("test")?.expect("should exist");
        assert_eq!(loaded_meta.program_hash, meta.program_hash);
        assert_eq!(loaded_meta.edb_fingerprint, meta.edb_fingerprint);
        assert_eq!(loaded_meta.created_at, meta.created_at);
        assert_eq!(loaded_snapshot.relations.len(), 1);
        assert_eq!(loaded_snapshot.relations[0].0, "derived");
        assert_eq!(loaded_snapshot.relations[0].1.len(), 2);

        // Invalidate
        backend.invalidate("test")?;
        assert!(backend.load("test")?.is_none());

        Ok(())
    }
}
