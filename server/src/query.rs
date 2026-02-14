/// Extracts the predicate name from a query string like `greeting(X)` or `route(Method, Path)`.
/// Returns the predicate name (everything before the first `(`).
pub fn extract_predicate(query: &str) -> Option<&str> {
    let trimmed = query.trim();
    let paren = trimmed.find('(')?;
    let name = trimmed[..paren].trim();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_query() {
        assert_eq!(extract_predicate("greeting(X)"), Some("greeting"));
    }

    #[test]
    fn test_multi_arg() {
        assert_eq!(
            extract_predicate("route(Method, Path, Handler)"),
            Some("route")
        );
    }

    #[test]
    fn test_no_parens() {
        assert_eq!(extract_predicate("greeting"), None);
    }

    #[test]
    fn test_empty() {
        assert_eq!(extract_predicate(""), None);
    }
}
