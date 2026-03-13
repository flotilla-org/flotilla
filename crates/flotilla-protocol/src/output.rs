use std::fmt;

use serde::Serialize;

/// Selects between human-readable and machine-readable output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Human,
    Json,
}

impl OutputFormat {
    pub fn from_json_flag(json: bool) -> Self {
        if json {
            Self::Json
        } else {
            Self::Human
        }
    }
}

/// Serialize `data` as compact single-line JSON. Falls back to Debug on error.
pub fn json_line<T: Serialize + fmt::Debug>(data: &T) -> String {
    serde_json::to_string(data).unwrap_or_else(|_| format!("{data:?}"))
}

/// Serialize `data` as pretty-printed JSON. Falls back to Debug on error.
pub fn json_pretty<T: Serialize + fmt::Debug>(data: &T) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|_| format!("{data:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Serialize)]
    struct Sample {
        name: String,
        count: u32,
    }

    #[test]
    fn json_line_produces_compact_json() {
        let s = Sample { name: "test".into(), count: 42 };
        let result = json_line(&s);
        assert_eq!(result, r#"{"name":"test","count":42}"#);
        assert!(!result.contains('\n'));
    }

    #[test]
    fn json_pretty_produces_indented_json() {
        let s = Sample { name: "test".into(), count: 42 };
        let result = json_pretty(&s);
        assert!(result.contains('\n'), "pretty JSON should contain newlines");
        assert!(result.contains("  \"name\""), "pretty JSON should be indented");
    }

    #[test]
    fn json_line_fallback_on_serialize_error() {
        #[derive(Debug)]
        struct Bad;
        impl Serialize for Bad {
            fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("intentional"))
            }
        }
        let result = json_line(&Bad);
        assert_eq!(result, "Bad");
    }

    #[test]
    fn json_pretty_fallback_on_serialize_error() {
        #[derive(Debug)]
        struct Bad;
        impl Serialize for Bad {
            fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("intentional"))
            }
        }
        let result = json_pretty(&Bad);
        assert_eq!(result, "Bad");
    }

    #[test]
    fn from_json_flag_conversion() {
        assert_eq!(OutputFormat::from_json_flag(true), OutputFormat::Json);
        assert_eq!(OutputFormat::from_json_flag(false), OutputFormat::Human);
    }
}
