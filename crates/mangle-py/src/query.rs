use anyhow::{Result, anyhow};
use mangle_ast::{Arena, BaseTerm, Const};
use mangle_common::Value;
use mangle_parse::Parser;

pub struct ParsedQuery {
    pub predicate: String,
    pub args: Vec<QueryArg>,
}

pub enum QueryArg {
    Variable,
    StringConst(String),
    NameConst(String),
    NumberConst(i64),
}

/// Parse a query string. If parsing fails, fall back to extracting just the
/// predicate name (no argument filtering).
pub fn parse_query_lenient(query: &str) -> Result<ParsedQuery> {
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
