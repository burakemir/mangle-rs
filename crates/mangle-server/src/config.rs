use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use mangle_common::Value;

use crate::store::eval_source_multi;

const CONFIG_SCHEMA: &str = include_str!("config_schema.mg");

pub struct ServerConfig {
    pub port: u16,
    pub programs_dir: Option<PathBuf>,
    pub edb_dir: Option<PathBuf>,
    pub idb_cache_dir: Option<PathBuf>,
    pub persist_edb: HashSet<String>,
    pub config_path: PathBuf,
}

impl ServerConfig {
    /// Parse CLI args to find --config path, then load and evaluate the config file.
    pub fn from_args() -> Result<Self> {
        let args: Vec<String> = std::env::args().collect();
        let mut config_path = PathBuf::from("./config.mg");

        let mut i = 1;
        while i < args.len() {
            if args[i] == "--config" {
                i += 1;
                if i < args.len() {
                    config_path = PathBuf::from(&args[i]);
                }
            }
            i += 1;
        }

        let source = std::fs::read_to_string(&config_path)
            .map_err(|e| anyhow!("cannot read config file {}: {}", config_path.display(), e))?;

        let mut config = Self::from_source(&source)?;
        config.config_path = config_path;
        Ok(config)
    }

    /// Parse a config source string against the embedded schema.
    pub fn from_source(config_source: &str) -> Result<Self> {
        let port = query_single_number(config_source, "config_schema.server_port(X)")?
            .unwrap_or(8090) as u16;

        let programs_dir =
            query_single_string(config_source, "config_schema.programs_dir(X)")?.map(PathBuf::from);

        let edb_dir =
            query_single_string(config_source, "config_schema.edb_dir(X)")?.map(PathBuf::from);

        let idb_cache_dir =
            query_single_string(config_source, "config_schema.idb_cache_dir(X)")?
                .map(PathBuf::from);

        let persist_edb = query_all_strings(config_source, "config_schema.persist_edb(X)")?;

        Ok(ServerConfig {
            port,
            programs_dir,
            edb_dir,
            idb_cache_dir,
            persist_edb,
            config_path: PathBuf::new(),
        })
    }
}

fn query_single_string(config_source: &str, query: &str) -> Result<Option<String>> {
    let results = eval_source_multi(&[CONFIG_SCHEMA, config_source], Some(query))?;
    match results.first() {
        Some(row) => match row.first() {
            Some(Value::String(s) | Value::Name(s)) => Ok(Some(s.clone())),
            Some(other) => Err(anyhow!("expected string for {}, got {:?}", query, other)),
            None => Ok(None),
        },
        None => Ok(None),
    }
}

fn query_single_number(config_source: &str, query: &str) -> Result<Option<i64>> {
    let results = eval_source_multi(&[CONFIG_SCHEMA, config_source], Some(query))?;
    match results.first() {
        Some(row) => match row.first() {
            Some(Value::Number(n)) => Ok(Some(*n)),
            Some(other) => Err(anyhow!("expected number for {}, got {:?}", query, other)),
            None => Ok(None),
        },
        None => Ok(None),
    }
}

fn query_all_strings(config_source: &str, query: &str) -> Result<HashSet<String>> {
    let results = eval_source_multi(&[CONFIG_SCHEMA, config_source], Some(query))?;
    let mut set = HashSet::new();
    for row in results {
        if let Some(Value::String(s) | Value::Name(s)) = row.first() {
            set.insert(s.clone());
        }
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_from_source_full() {
        let source = r#"
            Use config_schema !
            config_schema.server_port(9090).
            config_schema.programs_dir("/my/programs").
            config_schema.edb_dir("/my/edb").
            config_schema.idb_cache_dir("/my/idb").
            config_schema.persist_edb("runtime").
            config_schema.persist_edb("notes_knowledge").
        "#;

        let config = ServerConfig::from_source(source).unwrap();
        assert_eq!(config.port, 9090);
        assert_eq!(
            config.programs_dir,
            Some(PathBuf::from("/my/programs"))
        );
        assert_eq!(config.edb_dir, Some(PathBuf::from("/my/edb")));
        assert_eq!(config.idb_cache_dir, Some(PathBuf::from("/my/idb")));
        assert!(config.persist_edb.contains("runtime"));
        assert!(config.persist_edb.contains("notes_knowledge"));
        assert_eq!(config.persist_edb.len(), 2);
    }

    #[test]
    fn test_config_from_source_minimal() {
        let source = r#"
            Use config_schema !
            config_schema.server_port(8080).
        "#;

        let config = ServerConfig::from_source(source).unwrap();
        assert_eq!(config.port, 8080);
        assert_eq!(config.programs_dir, None);
        assert_eq!(config.edb_dir, None);
        assert_eq!(config.idb_cache_dir, None);
        assert!(config.persist_edb.is_empty());
    }

    #[test]
    fn test_config_defaults() {
        let source = r#"
            Use config_schema !
        "#;

        let config = ServerConfig::from_source(source).unwrap();
        assert_eq!(config.port, 8090); // default
    }
}
