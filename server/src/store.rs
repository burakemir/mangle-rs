use anyhow::{Result, anyhow};
use mangle_ast::Arena;
use mangle_factstore::Value;
use mangle_interpreter::MemStore;
use std::collections::HashMap;

use crate::query::{ParsedQuery, filter_tuples, parse_query};

/// Metadata about a loaded program (stored without compiled form).
pub struct StoredProgram {
    pub source: String,
    pub predicates: Vec<String>,
}

/// In-memory registry of named Mangle programs.
pub struct ProgramStore {
    programs: HashMap<String, StoredProgram>,
}

/// Info returned when listing programs.
#[derive(Clone)]
pub struct ProgramInfo {
    pub name: String,
    pub predicates: Vec<String>,
}

/// Parse a query string, falling back to simple predicate extraction.
fn parse_query_lenient(query: &str) -> Result<ParsedQuery> {
    parse_query(query).or_else(|_| {
        let trimmed = query.trim();
        let paren = trimmed.find('(')
            .ok_or_else(|| anyhow!("invalid query: cannot extract predicate from '{}'", query))?;
        let name = trimmed[..paren].trim();
        if name.is_empty() {
            return Err(anyhow!("invalid query: empty predicate name"));
        }
        Ok(ParsedQuery {
            predicate: name.to_string(),
            args: vec![],
        })
    })
}

impl ProgramStore {
    pub fn new() -> Self {
        Self {
            programs: HashMap::new(),
        }
    }

    /// Compile source to extract predicate names, then store source + names.
    /// The compiled form is discarded (avoids lifetime issues with Arena-borrowing types).
    pub fn load(&mut self, name: &str, source: &str) -> Result<ProgramInfo> {
        let arena = Arena::new_with_global_interner();
        let (_ir, stratified) = mangle_driver::compile(source, &arena)?;

        // Collect IDB predicate names from strata
        let mut predicates = Vec::new();
        for stratum in stratified.strata() {
            for pred in &stratum {
                if let Some(pred_name) = arena.predicate_name(*pred) {
                    predicates.push(pred_name.to_string());
                }
            }
        }

        let info = ProgramInfo {
            name: name.to_string(),
            predicates: predicates.clone(),
        };

        self.programs.insert(
            name.to_string(),
            StoredProgram {
                source: source.to_string(),
                predicates,
            },
        );

        Ok(info)
    }

    pub fn get(&self, name: &str) -> Option<&StoredProgram> {
        self.programs.get(name)
    }

    pub fn list(&self) -> Vec<ProgramInfo> {
        self.programs
            .iter()
            .map(|(name, prog)| ProgramInfo {
                name: name.clone(),
                predicates: prog.predicates.clone(),
            })
            .collect()
    }

    /// Recompile from stored source, execute, and scan the queried relation.
    /// Parses query arguments and filters results to match constant positions.
    pub fn execute_query(
        &self,
        name: &str,
        query: &str,
    ) -> Result<Vec<Vec<Value>>> {
        let prog = self
            .programs
            .get(name)
            .ok_or_else(|| anyhow!("program '{}' not found", name))?;

        let parsed = parse_query_lenient(query)?;

        let arena = Arena::new_with_global_interner();
        let (mut ir, stratified) = mangle_driver::compile(&prog.source, &arena)?;
        let store = Box::new(MemStore::new());
        let interpreter = mangle_driver::execute(&mut ir, &stratified, store)?;

        let tuples: Vec<Vec<Value>> = interpreter.store().scan(&parsed.predicate)?.collect();
        Ok(filter_tuples(tuples, &parsed))
    }
}

/// Compile and execute ephemeral source, returning results for the queried relation.
pub fn eval_source(source: &str, query: Option<&str>) -> Result<Vec<Vec<Value>>> {
    let arena = Arena::new_with_global_interner();
    let (mut ir, stratified) = mangle_driver::compile(source, &arena)?;
    let store = Box::new(MemStore::new());
    let interpreter = mangle_driver::execute(&mut ir, &stratified, store)?;

    if let Some(q) = query {
        let parsed = parse_query_lenient(q)?;
        let tuples: Vec<Vec<Value>> = interpreter.store().scan(&parsed.predicate)?.collect();
        Ok(filter_tuples(tuples, &parsed))
    } else {
        let mut all = Vec::new();
        for name in interpreter.store().relation_names() {
            let tuples: Vec<Vec<Value>> = interpreter.store().scan(&name)?.collect();
            all.extend(tuples);
        }
        Ok(all)
    }
}
