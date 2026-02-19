use anyhow::{Result, anyhow};
use mangle_ast::{Arena, BaseTerm, Const};
use mangle_common::Value;
use mangle_parse::Parser;

/// A parsed query with predicate name and argument patterns.
pub struct ParsedQuery {
    pub predicate: String,
    pub args: Vec<QueryArg>,
}

/// A single argument position in a query.
pub enum QueryArg {
    /// A variable (uppercase or `_`) — matches anything.
    Variable,
    /// A quoted string constant like `"GET"`.
    StringConst(String),
    /// A name constant like `/role/admin`.
    NameConst(String),
    /// A numeric constant like `42`.
    NumberConst(i64),
}

/// Parse a query string like `route("GET", Path, Handler)` into a `ParsedQuery`.
pub fn parse_query(query: &str) -> Result<ParsedQuery> {
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
            _ => QueryArg::Variable, // treat unknown forms as wildcards
        })
        .collect();

    Ok(ParsedQuery { predicate, args })
}

/// Filter tuples so that constant args must match the corresponding position.
pub fn filter_tuples(tuples: Vec<Vec<Value>>, query: &ParsedQuery) -> Vec<Vec<Value>> {
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
                        if val != &Value::String(s.clone()) {
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

/// Extracts the predicate name from a query string (legacy helper).
pub fn extract_predicate(query: &str) -> Option<&str> {
    let trimmed = query.trim();
    let paren = trimmed.find('(')?;
    let name = trimmed[..paren].trim();
    if name.is_empty() { None } else { Some(name) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_all_variables() {
        let pq = parse_query("route(Method, Path, Handler)").unwrap();
        assert_eq!(pq.predicate, "route");
        assert_eq!(pq.args.len(), 3);
        assert!(matches!(pq.args[0], QueryArg::Variable));
        assert!(matches!(pq.args[1], QueryArg::Variable));
        assert!(matches!(pq.args[2], QueryArg::Variable));
    }

    #[test]
    fn test_parse_string_constant() {
        let pq = parse_query(r#"route("GET", Path, Handler)"#).unwrap();
        assert_eq!(pq.predicate, "route");
        assert_eq!(pq.args.len(), 3);
        match &pq.args[0] {
            QueryArg::StringConst(s) => assert_eq!(s, "GET"),
            _ => panic!("expected StringConst"),
        }
        assert!(matches!(pq.args[1], QueryArg::Variable));
        assert!(matches!(pq.args[2], QueryArg::Variable));
    }

    #[test]
    fn test_parse_name_constant() {
        let pq = parse_query("role(/role/admin)").unwrap();
        assert_eq!(pq.predicate, "role");
        assert_eq!(pq.args.len(), 1);
        match &pq.args[0] {
            QueryArg::NameConst(s) => assert_eq!(s, "/role/admin"),
            _ => panic!("expected NameConst"),
        }
    }

    #[test]
    fn test_parse_number_constant() {
        let pq = parse_query("fact(42, X)").unwrap();
        assert_eq!(pq.predicate, "fact");
        assert_eq!(pq.args.len(), 2);
        match &pq.args[0] {
            QueryArg::NumberConst(n) => assert_eq!(*n, 42),
            _ => panic!("expected NumberConst"),
        }
        assert!(matches!(pq.args[1], QueryArg::Variable));
    }

    #[test]
    fn test_parse_no_args() {
        let pq = parse_query("empty()").unwrap();
        assert_eq!(pq.predicate, "empty");
        assert!(pq.args.is_empty());
    }

    #[test]
    fn test_filter_all_variables() {
        let pq = parse_query("pred(X, Y)").unwrap();
        let tuples = vec![
            vec![Value::Number(1), Value::Number(2)],
            vec![Value::Number(3), Value::Number(4)],
        ];
        let result = filter_tuples(tuples.clone(), &pq);
        assert_eq!(result, tuples);
    }

    #[test]
    fn test_filter_string_constant() {
        let pq = parse_query(r#"route("GET", Path)"#).unwrap();
        let tuples = vec![
            vec![Value::String("GET".into()), Value::String("/api".into())],
            vec![Value::String("POST".into()), Value::String("/api".into())],
            vec![Value::String("GET".into()), Value::String("/home".into())],
        ];
        let result = filter_tuples(tuples, &pq);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0][1], Value::String("/api".into()));
        assert_eq!(result[1][1], Value::String("/home".into()));
    }

    #[test]
    fn test_filter_number_constant() {
        let pq = parse_query("fact(1, Y)").unwrap();
        let tuples = vec![
            vec![Value::Number(1), Value::String("a".into())],
            vec![Value::Number(2), Value::String("b".into())],
            vec![Value::Number(1), Value::String("c".into())],
        ];
        let result = filter_tuples(tuples, &pq);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_filter_all_constants() {
        let pq = parse_query(r#"fact("x", 1)"#).unwrap();
        let tuples = vec![
            vec![Value::String("x".into()), Value::Number(1)],
            vec![Value::String("x".into()), Value::Number(2)],
            vec![Value::String("y".into()), Value::Number(1)],
        ];
        let result = filter_tuples(tuples, &pq);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], vec![Value::String("x".into()), Value::Number(1)]);
    }

    #[test]
    fn test_extract_predicate_legacy() {
        assert_eq!(extract_predicate("greeting(X)"), Some("greeting"));
        assert_eq!(
            extract_predicate("route(Method, Path, Handler)"),
            Some("route")
        );
        assert_eq!(extract_predicate("greeting"), None);
        assert_eq!(extract_predicate(""), None);
    }
}
