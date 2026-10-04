/// A token with its byte offset in the original input.
pub struct CommandToken {
    pub value: String,
    /// Byte offset of the token's start in the original input (including any leading quote).
    pub offset: usize,
}

/// Tokenize palette input. Like shell splitting with quote support, but without
/// treating `#` as a comment character (users type `cr #42 open`, not shell scripts).
///
/// Returns tokens with their byte offsets in the original input, enabling
/// prefix slicing for Tab completion.
pub fn tokenize_command(input: &str) -> Result<Vec<CommandToken>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut token_start: Option<usize> = None;
    let mut byte_offset = 0;
    let mut chars = input.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double_quote => {
                if token_start.is_none() {
                    token_start = Some(byte_offset);
                }
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                if token_start.is_none() {
                    token_start = Some(byte_offset);
                }
                in_double_quote = !in_double_quote;
            }
            '\\' if !in_single_quote => {
                if token_start.is_none() {
                    token_start = Some(byte_offset);
                }
                byte_offset += ch.len_utf8();
                if let Some(next) = chars.next() {
                    current.push(next);
                    byte_offset += next.len_utf8();
                }
                continue;
            }
            ' ' | '\t' if !in_single_quote && !in_double_quote => {
                if token_start.is_some() {
                    tokens.push(CommandToken { value: std::mem::take(&mut current), offset: token_start.unwrap_or(byte_offset) });
                    token_start = None;
                }
            }
            _ => {
                if token_start.is_none() {
                    token_start = Some(byte_offset);
                }
                current.push(ch);
            }
        }
        byte_offset += ch.len_utf8();
    }

    if in_single_quote || in_double_quote {
        return Err("unclosed quote".to_string());
    }
    if token_start.is_some() {
        tokens.push(CommandToken { value: current, offset: token_start.unwrap_or(byte_offset) });
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::tokenize_command;
    use crate::quote_value;

    // Quoting must preserve arbitrary identifier values, including empty tokens,
    // whitespace, quotes, escapes, Unicode and issue references; offsets locate
    // the quoted token in the original command for palette completion.
    #[test]
    fn quoted_values_round_trip_with_offsets() {
        let values = ["", "plain", "my work", "\t", "line\nbreak", "'", "\"", "\\", "café", "#42"];
        for first in values {
            for second in values {
                let prefix = format!("{} ", quote_value(first));
                let input = format!("{prefix}{}", quote_value(second));
                let tokens = tokenize_command(&input).expect("quoted values tokenize");
                assert_eq!(tokens.iter().map(|t| t.value.as_str()).collect::<Vec<_>>(), [first, second]);
                assert_eq!(tokens.iter().map(|t| t.offset).collect::<Vec<_>>(), [0, prefix.len()]);
            }
        }
    }

    // Generated identifiers combine shell grammar characters and Unicode;
    // lengths 0..24 include empty and repeated delimiters. Appending a sentinel
    // verifies one quoted token cannot consume the following argument.
    #[hegel::test]
    fn generated_quoted_values_round_trip(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let alphabet = ['a', ' ', '\t', '\n', '\'', '"', '\\', '#', 'é'];
        let len = tc.draw(gs::integers::<usize>().min_value(0).max_value(24));
        let value: String =
            (0..len).map(|_| alphabet[tc.draw(gs::integers::<usize>().min_value(0).max_value(alphabet.len() - 1))]).collect();
        let input = format!("{} sentinel", quote_value(&value));
        let tokens = tokenize_command(&input).expect("quoted identifier");
        assert_eq!(tokens.iter().map(|t| t.value.as_str()).collect::<Vec<_>>(), [value.as_str(), "sentinel"]);
    }

    // An unfinished quote is an incomplete command, rather than a valid token.
    #[test]
    fn unclosed_quotes_are_rejected() {
        for input in ["'unfinished", "\"unfinished"] {
            assert!(tokenize_command(input).is_err());
        }
    }
}
