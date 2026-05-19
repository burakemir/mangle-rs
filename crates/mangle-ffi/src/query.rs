//! Query atom parsing — lifted from `crates/mangle-py/src/query.rs`.
//!
//! A query is a Mangle atom: either a bare predicate name (`reachable`)
//! or `pred(arg1, arg2, ...)` where each argument is a constant filter
//! or a variable (wildcard). On parse, this module produces a
//! [`ParsedQuery`] which the cursor uses to scan the matching relation
//! and to filter the resulting tuples.
//!
//! Kept close to the mangle-py original so the two FFIs treat query
//! syntax identically.

use anyhow::{Result, anyhow};
use mangle_ast::{Arena, BaseTerm, Const};
use mangle_common::Value;
use mangle_parse::Parser;

pub(crate) struct ParsedQuery {
    pub predicate: String,
    pub args: Vec<QueryArg>,
}

pub(crate) enum QueryArg {
    Variable,
    StringConst(String),
    NameConst(String),
    NumberConst(i64),
}

/// Parse a query string. If parsing fails, fall back to extracting just
/// the predicate name (no argument filtering). Matches mangle-py's
/// `parse_query_lenient` exactly so both bindings accept the same input.
pub(crate) fn parse_query_lenient(query: &str) -> Result<ParsedQuery> {
    parse_query(query).or_else(|_| {
        let trimmed = query.trim();
        let name = match trimmed.find('(') {
            Some(i) => trimmed[..i].trim(),
            None => trimmed,
        };
        if name.is_empty() {
            return Err(anyhow!("empty predicate name in query: {:?}", query));
        }
        Ok(ParsedQuery {
            predicate: name.to_string(),
            args: vec![],
        })
    })
}

fn parse_query(query: &str) -> Result<ParsedQuery> {
    let arena = Arena::new_with_global_interner();
    let mut parser = Parser::new(&arena, query.as_bytes(), "query");
    parser.next_token().map_err(|e| anyhow!(e))?;
    let atom = parser.parse_atom()?;

    let predicate = arena
        .predicate_name(atom.sym)
        .ok_or_else(|| anyhow!("cannot resolve predicate name"))?
        .to_string();

    let args = atom
        .args
        .iter()
        .map(|arg| match arg {
            BaseTerm::Variable(_) => QueryArg::Variable,
            BaseTerm::Const(Const::String(s)) => QueryArg::StringConst(s.to_string()),
            BaseTerm::Const(Const::Name(n)) => {
                let name = arena.lookup_name(*n).unwrap_or("").to_string();
                QueryArg::NameConst(name)
            }
            BaseTerm::Const(Const::Number(n)) => QueryArg::NumberConst(*n),
            _ => QueryArg::Variable,
        })
        .collect();

    Ok(ParsedQuery { predicate, args })
}

/// Filter materialized tuples by the constant arguments in `query`.
/// Variables match anything; constants must `==` the tuple value at the
/// same position.
pub(crate) fn filter_tuples(tuples: Vec<Vec<Value>>, query: &ParsedQuery) -> Vec<Vec<Value>> {
    tuples
        .into_iter()
        .filter(|tuple| {
            for (i, arg) in query.args.iter().enumerate() {
                let Some(val) = tuple.get(i) else {
                    return false;
                };
                match arg {
                    QueryArg::Variable => {}
                    QueryArg::StringConst(s) => {
                        if val != &Value::String(s.clone()) {
                            return false;
                        }
                    }
                    QueryArg::NameConst(s) => {
                        if val != &Value::Name(s.clone()) {
                            return false;
                        }
                    }
                    QueryArg::NumberConst(n) => {
                        if val != &Value::Number(*n) {
                            return false;
                        }
                    }
                }
            }
            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_predicate() {
        let q = parse_query_lenient("reachable").unwrap();
        assert_eq!(q.predicate, "reachable");
        assert!(q.args.is_empty());
    }

    #[test]
    fn predicate_with_constants() {
        let q = parse_query_lenient(r#"route("GET", "/")"#).unwrap();
        assert_eq!(q.predicate, "route");
        assert_eq!(q.args.len(), 2);
        assert!(matches!(q.args[0], QueryArg::StringConst(ref s) if s == "GET"));
        assert!(matches!(q.args[1], QueryArg::StringConst(ref s) if s == "/"));
    }

    #[test]
    fn predicate_with_variable() {
        let q = parse_query_lenient(r#"route("GET", Path)"#).unwrap();
        assert_eq!(q.predicate, "route");
        assert_eq!(q.args.len(), 2);
        assert!(matches!(q.args[0], QueryArg::StringConst(_)));
        assert!(matches!(q.args[1], QueryArg::Variable));
    }

    #[test]
    fn malformed_falls_back_to_predicate_only() {
        // No closing paren; lenient mode extracts the predicate name.
        let q = parse_query_lenient("reachable(broken").unwrap();
        assert_eq!(q.predicate, "reachable");
        assert!(q.args.is_empty());
    }

    #[test]
    fn empty_query_errors() {
        assert!(parse_query_lenient("").is_err());
        assert!(parse_query_lenient("   ").is_err());
    }

    #[test]
    fn filter_matches_string_const() {
        let tuples = vec![
            vec![Value::String("GET".into()), Value::String("/".into())],
            vec![Value::String("POST".into()), Value::String("/".into())],
        ];
        let q = parse_query_lenient(r#"route("GET", X)"#).unwrap();
        let kept = filter_tuples(tuples, &q);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn filter_matches_number_const() {
        let tuples = vec![
            vec![Value::Number(1), Value::Number(2)],
            vec![Value::Number(3), Value::Number(2)],
        ];
        let q = parse_query_lenient("edge(1, X)").unwrap();
        let kept = filter_tuples(tuples, &q);
        assert_eq!(kept.len(), 1);
    }
}
