use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use mangle_common::Value;

/// Append-only mutation log for durable EDB writes.
///
/// Programs opted in via `persist_edb("name")` get their API mutations
/// written to `{edb_dir}/{program}/mutations.mg`. The file format uses
/// valid Mangle syntax:
///
/// - Insertions: `relation(arg1, arg2).`
/// - Retractions: `__retract__relation(arg1, arg2).`
///
/// On restart, `FileEdbSource` loads these files and applies `__retract__`
/// post-processing to reconstruct the correct state.
pub struct MutationLog {
    edb_dir: PathBuf,
    persist_programs: HashSet<String>,
}

impl MutationLog {
    pub fn new(edb_dir: PathBuf, persist_programs: HashSet<String>) -> Self {
        Self {
            edb_dir,
            persist_programs,
        }
    }

    pub fn is_persistent(&self, program: &str) -> bool {
        self.persist_programs.contains(program)
    }

    /// Append an insert fact to the mutations file.
    pub fn append_insert(&self, program: &str, relation: &str, tuple: &[Value]) -> Result<()> {
        if !self.is_persistent(program) {
            return Ok(());
        }
        let line = format_fact(relation, tuple);
        self.append_line(program, &line)
    }

    /// Append a retract fact to the mutations file.
    pub fn append_retract(&self, program: &str, relation: &str, tuple: &[Value]) -> Result<()> {
        if !self.is_persistent(program) {
            return Ok(());
        }
        let line = format_fact(&format!("__retract__{}", relation), tuple);
        self.append_line(program, &line)
    }

    fn append_line(&self, program: &str, line: &str) -> Result<()> {
        let dir = self.edb_dir.join(program);
        std::fs::create_dir_all(&dir)
            .map_err(|e| anyhow!("cannot create EDB dir {}: {}", dir.display(), e))?;

        let path = dir.join("mutations.mg");

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| anyhow!("cannot open {}: {}", path.display(), e))?;

        writeln!(file, "{}", line)
            .map_err(|e| anyhow!("cannot write to {}: {}", path.display(), e))?;

        Ok(())
    }
}

/// Format a fact as valid Mangle syntax: `relation(v1, v2).`
pub fn format_fact(relation: &str, tuple: &[Value]) -> String {
    let args: Vec<String> = tuple.iter().map(|v| v.to_string()).collect();
    format!("{}({}).", relation, args.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_fact_numbers() {
        let fact = format_fact("edge", &[Value::Number(1), Value::Number(2)]);
        assert_eq!(fact, "edge(1, 2).");
    }

    #[test]
    fn test_format_fact_strings() {
        let fact = format_fact(
            "container",
            &[
                Value::String("web".to_string()),
                Value::String("running".to_string()),
            ],
        );
        assert_eq!(fact, r#"container("web", "running")."#);
    }

    #[test]
    fn test_format_fact_retract() {
        let fact = format_fact(
            "__retract__container",
            &[
                Value::String("web".to_string()),
                Value::String("running".to_string()),
            ],
        );
        assert_eq!(fact, r#"__retract__container("web", "running")."#);
    }

    #[test]
    fn test_mutation_log_persistent() {
        let dir = tempfile::tempdir().unwrap();
        let mut programs = HashSet::new();
        programs.insert("runtime".to_string());

        let log = MutationLog::new(dir.path().to_path_buf(), programs);

        assert!(log.is_persistent("runtime"));
        assert!(!log.is_persistent("other"));
    }

    #[test]
    fn test_mutation_log_append_insert() {
        let dir = tempfile::tempdir().unwrap();
        let mut programs = HashSet::new();
        programs.insert("runtime".to_string());

        let log = MutationLog::new(dir.path().to_path_buf(), programs);

        log.append_insert(
            "runtime",
            "container",
            &[
                Value::String("web".to_string()),
                Value::String("running".to_string()),
            ],
        )
        .unwrap();

        let content = std::fs::read_to_string(dir.path().join("runtime/mutations.mg")).unwrap();
        assert_eq!(content, "container(\"web\", \"running\").\n");
    }

    #[test]
    fn test_mutation_log_append_retract() {
        let dir = tempfile::tempdir().unwrap();
        let mut programs = HashSet::new();
        programs.insert("runtime".to_string());

        let log = MutationLog::new(dir.path().to_path_buf(), programs);

        log.append_retract(
            "runtime",
            "container",
            &[
                Value::String("web".to_string()),
                Value::String("running".to_string()),
            ],
        )
        .unwrap();

        let content = std::fs::read_to_string(dir.path().join("runtime/mutations.mg")).unwrap();
        assert_eq!(
            content,
            "__retract__container(\"web\", \"running\").\n"
        );
    }

    #[test]
    fn test_mutation_log_skips_non_persistent() {
        let dir = tempfile::tempdir().unwrap();
        let programs = HashSet::new(); // no persistent programs

        let log = MutationLog::new(dir.path().to_path_buf(), programs);

        log.append_insert("runtime", "container", &[Value::Number(1)])
            .unwrap();

        // No file should be created
        assert!(!dir.path().join("runtime/mutations.mg").exists());
    }

    #[test]
    fn test_mutation_log_multiple_appends() {
        let dir = tempfile::tempdir().unwrap();
        let mut programs = HashSet::new();
        programs.insert("runtime".to_string());

        let log = MutationLog::new(dir.path().to_path_buf(), programs);

        log.append_insert(
            "runtime",
            "container",
            &[
                Value::String("web".to_string()),
                Value::String("running".to_string()),
            ],
        )
        .unwrap();

        log.append_insert(
            "runtime",
            "container",
            &[
                Value::String("postgres".to_string()),
                Value::String("running".to_string()),
            ],
        )
        .unwrap();

        log.append_retract(
            "runtime",
            "container",
            &[
                Value::String("web".to_string()),
                Value::String("running".to_string()),
            ],
        )
        .unwrap();

        let content = std::fs::read_to_string(dir.path().join("runtime/mutations.mg")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], r#"container("web", "running")."#);
        assert_eq!(lines[1], r#"container("postgres", "running")."#);
        assert_eq!(lines[2], r#"__retract__container("web", "running")."#);
    }
}
