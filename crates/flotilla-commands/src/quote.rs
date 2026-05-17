/// Quote a value if it contains whitespace, quotes, or backslashes.
///
/// Used by the `Display` impls of noun-verb commands so values containing spaces
/// (e.g. `--input topic="my work"`) survive a Display → re-parse round-trip.
pub fn quote_value(s: &str) -> String {
    let needs_quoting = s.is_empty() || s.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'' || c == '\\');
    if !needs_quoting {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::quote_value;

    #[test]
    fn plain_identifiers_pass_through() {
        assert_eq!(quote_value("scratch"), "scratch");
        assert_eq!(quote_value("topic=demo"), "topic=demo");
        assert_eq!(quote_value("https://example.com/repo.git"), "https://example.com/repo.git");
    }

    #[test]
    fn whitespace_triggers_quoting() {
        assert_eq!(quote_value("hello world"), "\"hello world\"");
        assert_eq!(quote_value("topic=my work"), "\"topic=my work\"");
    }

    #[test]
    fn embedded_quotes_and_backslashes_are_escaped() {
        assert_eq!(quote_value("with\"quote"), "\"with\\\"quote\"");
        assert_eq!(quote_value("back\\slash"), "\"back\\\\slash\"");
    }

    #[test]
    fn empty_string_is_quoted() {
        assert_eq!(quote_value(""), "\"\"");
    }
}
