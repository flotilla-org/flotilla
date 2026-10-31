//! A verb's `when` (#4182 ruling 3): conditions over a row's columns that must all hold, in the
//! grammar of a table read's `where` (`.status.phase != Landed`). The client evaluates them from
//! the row's cells; the route re-checks them against the subject's current row.
use std::cmp::Ordering;

use flotilla_address::{Operator, Template};
use flotilla_protocol::table::Row;
use serde_json::Value;

/// One parsed condition: the column it reads, compared with a literal.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Condition {
    /// As written, for the `when` column and a `not_applicable` answer.
    pub text: String,
    pub column: String,
    pub operator: Operator,
    pub literal: String,
}

impl Condition {
    /// Parses one condition, as a table read parses `where`.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let template: Template = format!("$row {text}").parse().map_err(|error| format!("condition `{text}`: {error}"))?;
        if template.path.segments().is_empty() {
            return Err(format!("condition `{text}` names no column"));
        }
        Ok(Self {
            text: text.to_owned(),
            column: template.path.segments().join("."),
            operator: template.operator,
            literal: template.literal,
        })
    }

    /// Whether the condition holds of `row`. A missing or null cell equals nothing, so only `!=`
    /// holds of it.
    pub(crate) fn holds(&self, row: &Row) -> bool {
        holds(row.get(&self.column).unwrap_or(&Value::Null), self.operator, &self.literal)
    }
}

/// Whether `cell` compares as `operator` with `literal`: numerically when both are numbers, by
/// text otherwise.
pub(crate) fn holds(cell: &Value, operator: Operator, literal: &str) -> bool {
    let order = match cell {
        Value::Null => return operator == Operator::NotEqual,
        Value::Number(number) => match (number.as_f64(), literal.parse::<f64>()) {
            (Some(number), Ok(literal)) => number.partial_cmp(&literal),
            _ => Some(number.to_string().as_str().cmp(literal)),
        },
        // Booleans have no order.
        Value::Bool(flag) => matches!(operator, Operator::Equal | Operator::NotEqual).then(|| flag.to_string().as_str().cmp(literal)),
        Value::String(text) => Some(text.as_str().cmp(literal)),
        Value::Array(_) | Value::Object(_) => None,
    };
    let Some(order) = order else { return operator == Operator::NotEqual };
    match operator {
        Operator::Equal => order == Ordering::Equal,
        Operator::NotEqual => order != Ordering::Equal,
        Operator::LessThan => order == Ordering::Less,
        Operator::LessThanOrEqual => order != Ordering::Greater,
        Operator::GreaterThan => order == Ordering::Greater,
        Operator::GreaterThanOrEqual => order != Ordering::Less,
    }
}

/// The columns a `confirm` text substitutes: each `{column}`.
pub(crate) fn confirm_columns(confirm: &str) -> Vec<String> {
    confirm.split('{').skip(1).filter_map(|part| part.split_once('}').map(|(column, _)| column.to_owned())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(phase: Value) -> Row {
        Row::from([("status.phase".into(), phase)])
    }

    // Behaviour (#4182 ruling 3): `when` uses the `where` grammar over the row's columns; text
    // compares as text, numbers numerically, and a missing cell satisfies only `!=`.
    #[test]
    fn conditions_compare_cells_with_literals() {
        let not_landed = Condition::parse(".status.phase != Landed").unwrap();
        assert_eq!(not_landed.column, "status.phase");
        assert!(not_landed.holds(&row(json!("Running"))));
        assert!(!not_landed.holds(&row(json!("Landed"))));
        assert!(not_landed.holds(&Row::new()));
        let landed = Condition::parse(".status.phase == Landed").unwrap();
        assert!(!landed.holds(&Row::new()));
        assert!(holds(&json!(9), Operator::LessThan, "10"));
        assert!(!holds(&json!("9"), Operator::LessThan, "10"), "text compares as text");
        assert!(holds(&json!(true), Operator::Equal, "true"));
        assert!(!holds(&json!(true), Operator::LessThan, "true"));
        assert!(Condition::parse("status.phase != Landed").is_err(), "a column path starts with `.`");
        assert!(Condition::parse(". == x").is_err());
    }

    #[test]
    fn confirm_substitutions_name_their_columns() {
        assert_eq!(confirm_columns("Abandon {metadata.name} in {status.phase}?"), ["metadata.name", "status.phase"]);
        assert!(confirm_columns("Abandon it?").is_empty());
    }
}
