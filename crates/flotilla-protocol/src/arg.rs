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

#[cfg(test)]
mod tests {
    use super::*;

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
