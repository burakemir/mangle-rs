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

//! SimpleRow format: a row-oriented fact file format using Mangle syntax.
//!
//! Each fact is written as a valid Mangle atom (e.g. `edge(1, 2).`).
//! The file has the same header as SimpleColumn (predicate count, then
//! predicate name/arity/count lines), followed by facts grouped by predicate.

use anyhow::{Context, Result, anyhow};
use mangle_ast as ast;
use mangle_factstore::Value;
use mangle_parse::Parser;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};

/// Parsed simplerow data: predicate name → list of fact tuples.
pub struct SimpleRowData {
    pub tables: HashMap<String, Vec<Vec<Value>>>,
}

/// Read a simplerow file from bytes.
pub fn read_from_bytes(data: &[u8]) -> Result<SimpleRowData> {
    let reader = BufReader::new(data);
    read_simple_row(reader)
}

/// Read a simplerow file from a reader.
pub fn read_from_reader<R: Read>(reader: R) -> Result<SimpleRowData> {
    let reader = BufReader::new(reader);
    read_simple_row(reader)
}

struct PredInfo {
    name: String,
    arity: usize,
    num_facts: usize,
}

fn read_simple_row<R: BufRead>(mut reader: R) -> Result<SimpleRowData> {
    let mut line = String::new();

    // 1. Num Predicates
    reader.read_line(&mut line)?;
    let num_preds: usize = line.trim().parse().context("parsing num_preds")?;
    line.clear();

    // 2. Predicate Headers
    let mut preds = Vec::with_capacity(num_preds);
    for _ in 0..num_preds {
        reader.read_line(&mut line)?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 3 {
            return Err(anyhow!("Invalid predicate header: {line}"));
        }
        let name = parts[0].to_string();
        let arity: usize = parts[1].parse().context("parsing arity")?;
        let num_facts: usize = parts[2].parse().context("parsing num_facts")?;
        preds.push(PredInfo {
            name,
            arity,
            num_facts,
        });
        line.clear();
    }

    let mut tables = HashMap::new();

    // 3. Facts (one per line, as Mangle atoms)
    let arena = ast::Arena::new_with_global_interner();
    for pred in &preds {
        let mut facts = Vec::with_capacity(pred.num_facts);

        if pred.arity == 0 {
            // Flag facts: just the predicate name followed by a dot
            for _ in 0..pred.num_facts {
                line.clear();
                reader.read_line(&mut line)?;
                // e.g. "flag_pred."
                facts.push(vec![]);
            }
            tables.insert(pred.name.clone(), facts);
            continue;
        }

        for _ in 0..pred.num_facts {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Err(anyhow!(
                    "Unexpected EOF reading facts for {}",
                    pred.name
                ));
            }
            let text = line.trim();
            if text.is_empty() {
                continue;
            }

            // Parse the atom using mangle-parse
            let mut parser = Parser::new(&arena, text.as_bytes(), "simplerow");
            parser.next_token().map_err(|e| anyhow!(e))?;
            let clause = parser.parse_clause()?;
            let atom = &clause.head;

            let mut tuple = Vec::with_capacity(pred.arity);
            for arg in atom.args {
                tuple.push(term_to_value(arg));
            }
            facts.push(tuple);
        }
        tables.insert(pred.name.clone(), facts);
    }

    Ok(SimpleRowData { tables })
}

fn term_to_value(term: &ast::BaseTerm) -> Value {
    match term {
        ast::BaseTerm::Const(ast::Const::Number(n)) => Value::Number(*n),
        ast::BaseTerm::Const(ast::Const::String(s)) => Value::String(s.to_string()),
        ast::BaseTerm::Const(ast::Const::Name(n)) => {
            // Name constants become strings
            Value::String(format!("{n:?}"))
        }
        _ => Value::String(format!("{term:?}")),
    }
}

/// Write facts in simplerow format.
pub fn write_simple_row<W: Write>(
    writer: &mut W,
    tables: &[(String, Vec<Vec<Value>>)],
) -> Result<()> {
    // Header: number of predicates
    writeln!(writer, "{}", tables.len())?;

    // Predicate info lines
    for (name, facts) in tables {
        let arity = facts.first().map_or(0, |f| f.len());
        writeln!(writer, "{} {} {}", name, arity, facts.len())?;
    }

    // Facts as Mangle atoms
    for (name, facts) in tables {
        for tuple in facts {
            write!(writer, "{name}(")?;
            for (i, val) in tuple.iter().enumerate() {
                if i > 0 {
                    write!(writer, ", ")?;
                }
                write!(writer, "{val}")?;
            }
            writeln!(writer, ").")?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trip() -> Result<()> {
        let tables = vec![
            (
                "edge".to_string(),
                vec![
                    vec![Value::Number(1), Value::Number(2)],
                    vec![Value::Number(2), Value::Number(3)],
                ],
            ),
            (
                "user".to_string(),
                vec![vec![Value::String("Alice".to_string()), Value::Number(30)]],
            ),
        ];

        let mut buf = Vec::new();
        write_simple_row(&mut buf, &tables)?;

        let data = read_from_bytes(&buf)?;

        assert_eq!(data.tables["edge"].len(), 2);
        assert_eq!(
            data.tables["edge"][0],
            vec![Value::Number(1), Value::Number(2)]
        );
        assert_eq!(
            data.tables["edge"][1],
            vec![Value::Number(2), Value::Number(3)]
        );

        assert_eq!(data.tables["user"].len(), 1);
        assert_eq!(
            data.tables["user"][0],
            vec![Value::String("Alice".to_string()), Value::Number(30)]
        );

        Ok(())
    }

    #[test]
    fn test_write_format() -> Result<()> {
        let tables = vec![(
            "p".to_string(),
            vec![vec![Value::Number(42), Value::String("hello".to_string())]],
        )];

        let mut buf = Vec::new();
        write_simple_row(&mut buf, &tables)?;
        let output = String::from_utf8(buf)?;

        assert!(output.contains("1\n")); // 1 predicate
        assert!(output.contains("p 2 1\n")); // name arity count
        assert!(output.contains("p(42, \"hello\").\n"));

        Ok(())
    }
}
