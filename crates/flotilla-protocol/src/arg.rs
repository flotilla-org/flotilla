use std::fmt;

use serde::{Deserialize, Serialize};

/// Structured shell command fragments for shell-backed consumers.
/// This is intentionally not a universal argv representation.
///
/// **Safety invariant:** `Literal` is raw shell at the current depth.
/// Only resolvers (trusted code) construct `Arg` values. When serialized
/// across the wire, this extends to a protocol-level trust assumption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum Arg {
    /// Emitted verbatim at the current shell depth (flags, syntax tokens, expansion vars).
    Literal(String),
    /// Shell-quoted at flatten time (single-quoted, no expansion).
    Quoted(String),
    /// Subtree rendered as a single shell-quoted argument at the next depth.
    NestedCommand(Vec<Arg>),
}

impl Arg {
    fn fmt_indented(&self, f: &mut fmt::Formatter<'_>, depth: usize) -> fmt::Result {
        let indent = "  ".repeat(depth);
        match self {
            Arg::Literal(s) => write!(f, "{indent}{s}"),
            Arg::Quoted(s) => write!(f, "{indent}\"{s}\""),
            Arg::NestedCommand(inner) => {
                writeln!(f, "{indent}NestedCommand(")?;
                for arg in inner {
                    arg.fmt_indented(f, depth + 1)?;
                    writeln!(f)?;
                }
                write!(f, "{indent})")
            }
        }
    }
}

impl fmt::Display for Arg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_indented(f, 0)
    }
}

/// Render a `Vec<Arg>` to a shell command string.
///
/// `Literal` values pass through verbatim. `Quoted` values are single-quoted.
/// `NestedCommand` subtrees are recursively flattened and the result is
/// single-quoted as a single argument.
///
/// The `depth` parameter tracks nesting level (pass 0 at the top level).
/// Currently it is threaded through for future use but does not affect quoting
/// strategy — single-quoting is used at all depths.
#[allow(clippy::only_used_in_recursion)]
pub fn flatten(args: &[Arg], depth: usize) -> String {
    args.iter()
        .map(|arg| match arg {
            Arg::Literal(s) => s.clone(),
            Arg::Quoted(s) => shell_quote(s),
            Arg::NestedCommand(inner) => {
                let rendered = flatten(inner, depth + 1);
                shell_quote(&rendered)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── flatten tests ───────────────────────────────────────────────

    #[test]
    fn flatten_single_literal() {
        let args = [Arg::Literal("ls".into())];
        assert_eq!(flatten(&args, 0), "ls");
    }

    #[test]
    fn flatten_single_quoted() {
        let args = [Arg::Quoted("hello world".into())];
        assert_eq!(flatten(&args, 0), "'hello world'");
    }

    #[test]
    fn flatten_quoted_with_embedded_single_quote() {
        let args = [Arg::Quoted("it's".into())];
        assert_eq!(flatten(&args, 0), "'it'\\''s'");
    }

    #[test]
    fn flatten_quoted_with_spaces_and_special_chars() {
        let args = [Arg::Quoted("path with spaces/$VAR/`cmd`".into())];
        assert_eq!(flatten(&args, 0), "'path with spaces/$VAR/`cmd`'");
    }

    #[test]
    fn flatten_nested_command_depth_zero() {
        // NestedCommand at depth 0: subtree rendered and single-quoted as one arg
        let args = [Arg::NestedCommand(vec![Arg::Literal("cleat".into()), Arg::Quoted("sess".into())])];
        assert_eq!(flatten(&args, 0), "'cleat '\\''sess'\\'''");
    }

    #[test]
    fn flatten_nested_two_levels_deep() {
        // depth 0 single-quotes outer, depth 1 single-quotes inner
        let args = [Arg::NestedCommand(vec![
            Arg::Literal("sh".into()),
            Arg::Literal("-c".into()),
            Arg::NestedCommand(vec![Arg::Literal("echo".into()), Arg::Quoted("hi".into())]),
        ])];
        // Verify depth 1 result independently before checking the full nested output
        let inner = [
            Arg::Literal("sh".into()),
            Arg::Literal("-c".into()),
            Arg::NestedCommand(vec![Arg::Literal("echo".into()), Arg::Quoted("hi".into())]),
        ];
        let depth_1 = flatten(&inner, 1);
        assert_eq!(depth_1, "sh -c 'echo '\\''hi'\\'''");

        // Full nested result at depth 0
        let full = flatten(&args, 0);
        assert_eq!(full, shell_quote(&depth_1));
    }

    #[test]
    fn flatten_mixed_args() {
        let args = [
            Arg::Literal("ssh".into()),
            Arg::Literal("-t".into()),
            Arg::Quoted("user@feta".into()),
            Arg::NestedCommand(vec![Arg::Literal("cleat".into()), Arg::Quoted("sess".into())]),
        ];
        // Quoted "user@feta" -> 'user@feta'
        // NestedCommand: flatten at depth 1 = "cleat 'sess'"
        //   shell_quote("cleat 'sess'") = 'cleat '\''sess'\'''
        assert_eq!(flatten(&args, 0), "ssh -t 'user@feta' 'cleat '\\''sess'\\'''");
    }

    #[test]
    fn flatten_empty_nested_command() {
        let args = [Arg::NestedCommand(vec![])];
        assert_eq!(flatten(&args, 0), "''");
    }

    #[test]
    fn flatten_empty_args() {
        let args: [Arg; 0] = [];
        assert_eq!(flatten(&args, 0), "");
    }

    #[test]
    fn flatten_regression_remote_attach() {
        // Regression test: equivalent to what wrap_remote_attach_commands() builds
        // for: target="user@feta", multiplex=false, checkout="/home/user/dev/my-repo",
        // command="cleat attach sess-1"
        //
        // Current code produces (with double-quote wrapping at depth 1):
        //   ssh -t 'user@feta' '$SHELL -l -c "cd '\''/home/user/dev/my-repo'\'' && cleat attach sess-1"'
        //
        // New Arg model uses single-quotes at all depths. Because $SHELL is Literal,
        // it passes through verbatim and gets expanded by the shell at the right level.
        // The shell semantics are equivalent: the remote receives the same effective
        // command after shell parsing, just via different quoting mechanics.
        let args = [
            Arg::Literal("ssh".into()),
            Arg::Literal("-t".into()),
            Arg::Quoted("user@feta".into()),
            Arg::NestedCommand(vec![
                Arg::Literal("$SHELL".into()),
                Arg::Literal("-l".into()),
                Arg::Literal("-c".into()),
                Arg::NestedCommand(vec![
                    Arg::Literal("cd".into()),
                    Arg::Quoted("/home/user/dev/my-repo".into()),
                    Arg::Literal("&&".into()),
                    Arg::Literal("cleat".into()),
                    Arg::Literal("attach".into()),
                    Arg::Literal("sess-1".into()),
                ]),
            ]),
        ];

        // Trace through flatten:
        // depth 2: "cd '/home/user/dev/my-repo' && cleat attach sess-1"
        let depth_2 = flatten(
            &[
                Arg::Literal("cd".into()),
                Arg::Quoted("/home/user/dev/my-repo".into()),
                Arg::Literal("&&".into()),
                Arg::Literal("cleat".into()),
                Arg::Literal("attach".into()),
                Arg::Literal("sess-1".into()),
            ],
            2,
        );
        assert_eq!(depth_2, "cd '/home/user/dev/my-repo' && cleat attach sess-1");

        // depth 1: "$SHELL -l -c '<depth_2 shell_quoted>'"
        // shell_quote(depth_2) escapes the ' around the path:
        //   'cd '\''/home/user/dev/my-repo'\'' && cleat attach sess-1'
        let depth_2_quoted = shell_quote(&depth_2);
        assert_eq!(depth_2_quoted, "'cd '\\''/home/user/dev/my-repo'\\'' && cleat attach sess-1'");

        let depth_1_result = format!("$SHELL -l -c {depth_2_quoted}");
        assert_eq!(depth_1_result, "$SHELL -l -c 'cd '\\''/home/user/dev/my-repo'\\'' && cleat attach sess-1'");

        // depth 0: "ssh -t 'user@feta' '<depth_1 shell_quoted>'"
        let full = flatten(&args, 0);

        // The full command: ssh receives the single-quoted depth-1 string.
        // On the remote, $SHELL expands (it's a Literal, unquoted by surrounding
        // single quotes that have been stripped by the local shell), and -c receives
        // the depth-2 command with its own quoting intact.
        let expected_depth_0 = format!("ssh -t 'user@feta' {}", shell_quote(&depth_1_result));
        assert_eq!(full, expected_depth_0);
    }

    #[test]
    fn flatten_regression_remote_attach_empty_command() {
        // Empty command case: login shell at remote directory.
        // Current code: ssh -t 'user@feta' '$SHELL -l -c "cd '\''/path'\'' && exec \$SHELL -l"'
        // New model: $SHELL is Literal at both levels, single-quoted throughout.
        let args = [
            Arg::Literal("ssh".into()),
            Arg::Literal("-t".into()),
            Arg::Quoted("user@feta".into()),
            Arg::NestedCommand(vec![
                Arg::Literal("$SHELL".into()),
                Arg::Literal("-l".into()),
                Arg::Literal("-c".into()),
                Arg::NestedCommand(vec![
                    Arg::Literal("cd".into()),
                    Arg::Quoted("/home/user/dev/my-repo".into()),
                    Arg::Literal("&&".into()),
                    Arg::Literal("exec".into()),
                    Arg::Literal("$SHELL".into()),
                    Arg::Literal("-l".into()),
                ]),
            ]),
        ];

        let full = flatten(&args, 0);

        // Verify the inner layers independently
        let depth_2 = "cd '/home/user/dev/my-repo' && exec $SHELL -l";
        let depth_1 = format!("$SHELL -l -c {}", shell_quote(depth_2));
        let expected = format!("ssh -t 'user@feta' {}", shell_quote(&depth_1));
        assert_eq!(full, expected);
    }

    #[test]
    fn flatten_regression_remote_attach_with_multiplex() {
        // With SSH multiplexing args (all Literal since they're raw shell flags)
        let args = [
            Arg::Literal("ssh".into()),
            Arg::Literal("-t".into()),
            Arg::Literal("-o".into()),
            Arg::Literal("ControlMaster=auto".into()),
            Arg::Literal("-o".into()),
            Arg::Quoted("/home/user/.config/flotilla/ssh/ctrl-%r@%h-%p".into()),
            Arg::Literal("-o".into()),
            Arg::Literal("ControlPersist=60".into()),
            Arg::Quoted("user@feta".into()),
            Arg::NestedCommand(vec![Arg::Literal("tmux".into()), Arg::Literal("attach".into())]),
        ];

        let full = flatten(&args, 0);
        assert!(full.starts_with("ssh -t -o ControlMaster=auto -o "));
        assert!(full.contains("'user@feta'"));
        assert!(full.ends_with("'tmux attach'"));
    }

    // ── Display tests ────────────────────────────────────────────────

    #[test]
    fn display_literal() {
        let arg = Arg::Literal("--verbose".into());
        assert_eq!(format!("{arg}"), "--verbose");
    }

    #[test]
    fn display_quoted() {
        let arg = Arg::Quoted("hello world".into());
        assert_eq!(format!("{arg}"), "\"hello world\"");
    }

    #[test]
    fn display_nested_command() {
        let arg = Arg::NestedCommand(vec![Arg::Literal("tmux".into()), Arg::Literal("attach".into())]);
        let output = format!("{arg}");
        assert!(output.contains("NestedCommand("));
        assert!(output.contains("  tmux"));
        assert!(output.contains("  attach"));
        assert!(output.ends_with(')'));

        // Verify nested indentation increases per depth
        let nested = Arg::NestedCommand(vec![Arg::Literal("ssh".into()), Arg::NestedCommand(vec![Arg::Literal("inner".into())])]);
        let output = format!("{nested}");
        assert!(output.contains("  NestedCommand("), "inner NestedCommand should be indented: {output}");
        assert!(output.contains("    inner"), "inner args should be double-indented: {output}");
    }

    // ── shell_quote tests ────────────────────────────────────────────

    #[test]
    fn shell_quote_simple() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn shell_quote_with_spaces() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
    }

    #[test]
    fn shell_quote_with_single_quote() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_quote_with_multiple_single_quotes() {
        assert_eq!(shell_quote("it''s"), "'it'\\'''\\''s'");
    }

    #[test]
    fn shell_quote_empty() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_dollar_and_backtick_preserved() {
        // Single-quoting prevents expansion, which is the point
        assert_eq!(shell_quote("$HOME `whoami`"), "'$HOME `whoami`'");
    }

    // ── serde tests (pre-existing) ──────────────────────────────────

    #[test]
    fn arg_serde_roundtrip_literal() {
        let arg = Arg::Literal("--verbose".to_string());
        let json = serde_json::to_string(&arg).expect("serialize");
        let decoded: Arg = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, arg);
    }

    #[test]
    fn arg_serde_roundtrip_quoted() {
        let arg = Arg::Quoted("hello world".to_string());
        let json = serde_json::to_string(&arg).expect("serialize");
        let decoded: Arg = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, arg);
    }

    #[test]
    fn arg_serde_roundtrip_nested_command() {
        let arg = Arg::NestedCommand(vec![
            Arg::Literal("ssh".to_string()),
            Arg::Quoted("user@host".to_string()),
            Arg::NestedCommand(vec![Arg::Literal("tmux".to_string()), Arg::Literal("attach".to_string())]),
        ]);
        let json = serde_json::to_string(&arg).expect("serialize");
        let decoded: Arg = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, arg);
    }

    #[test]
    fn arg_serde_adjacently_tagged_format() {
        let arg = Arg::Literal("--flag".to_string());
        let json = serde_json::to_string(&arg).expect("serialize");
        // Verify adjacently-tagged format: {"type":"Literal","value":"--flag"}
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["type"], "Literal");
        assert_eq!(v["value"], "--flag");

        let arg = Arg::NestedCommand(vec![Arg::Quoted("x".to_string())]);
        let json = serde_json::to_string(&arg).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["type"], "NestedCommand");
        assert!(v["value"].is_array());
    }
}
