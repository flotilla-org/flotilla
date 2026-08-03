use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use flotilla_protocol::{Leaf, LeafKind, LeafOperator};

use crate::{Convoy, CrewWorkPhase, CrewWorkState, ResourceObject, Vessel, WorkState};

pub const ADMITTED_LEAF_VOCABULARY: &[(&str, &str)] = &[
    ("convoy", ".status.phase"),
    ("vessel", ".status.phase"),
    ("work", ".status.phase"),
    ("work", ".latest-claim.disposition"),
    ("work", ".latest-claim.claimed-at"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreeValue {
    True,
    False,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafValue {
    Text(String),
    Timestamp(DateTime<Utc>),
}

impl std::fmt::Display for LeafValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(value) => f.write_str(value),
            Self::Timestamp(value) => write!(f, "{}", value.to_rfc3339()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafEvaluation {
    pub result: ThreeValue,
    pub value: Option<LeafValue>,
}

/// Typed seam between the expression evaluator and a stored resource shape.
///
/// A descriptor-backed implementation can replace the handwritten subjects
/// without changing admission, comparison, or subscription machinery.
pub trait LeafSubject {
    fn kind(&self) -> LeafKind;
    fn value(&self, field_path: &str) -> Option<LeafValue>;
    fn observed_at(&self) -> Option<DateTime<Utc>> {
        None
    }
}

pub fn admit_leaf(leaf: &Leaf) -> Result<(), String> {
    let kind = leaf.address.kind();
    let admitted =
        ADMITTED_LEAF_VOCABULARY.iter().any(|(candidate_kind, path)| *candidate_kind == kind.to_string() && *path == leaf.field_path);
    if !admitted {
        let vocabulary = ADMITTED_LEAF_VOCABULARY.iter().map(|(kind, path)| format!("{kind}{path}")).collect::<Vec<_>>().join(", ");
        return Err(format!("leaf path `{kind}{}` is not admitted; admitted vocabulary: {vocabulary}", leaf.field_path));
    }
    if leaf.field_path != ".latest-claim.claimed-at" && !matches!(leaf.operator, LeafOperator::Equal | LeafOperator::NotEqual) {
        return Err(format!("operator `{}` is not admitted for text leaf `{kind}{}`; use `==` or `!=`", leaf.operator, leaf.field_path));
    }
    if leaf.field_path == ".latest-claim.claimed-at" {
        leaf.literal
            .parse::<DateTime<Utc>>()
            .map_err(|error| format!("invalid timestamp literal `{}` for work.latest-claim.claimed-at: {error}", leaf.literal))?;
    }
    Ok(())
}

pub fn evaluate_leaf(
    leaf: &Leaf,
    subject: Option<&dyn LeafSubject>,
    freshness_demand: Option<DateTime<Utc>>,
) -> Result<LeafEvaluation, String> {
    admit_leaf(leaf)?;
    let Some(subject) = subject else {
        return Ok(LeafEvaluation { result: ThreeValue::Unknown, value: None });
    };
    if subject.kind() != leaf.address.kind() {
        return Err(format!("leaf addresses {} but subject is {}", leaf.address.kind(), subject.kind()));
    }
    if freshness_demand.is_some_and(|demand| subject.observed_at().is_none_or(|observed_at| observed_at < demand)) {
        return Ok(LeafEvaluation { result: ThreeValue::Unknown, value: None });
    }
    let Some(value) = subject.value(&leaf.field_path) else {
        return Ok(LeafEvaluation { result: ThreeValue::Unknown, value: None });
    };
    let literal = bind_literal(&leaf.field_path, &leaf.literal)?;
    let ordering = compare_values(&value, &literal)?;
    let fired = match leaf.operator {
        LeafOperator::Equal => ordering == Ordering::Equal,
        LeafOperator::NotEqual => ordering != Ordering::Equal,
        LeafOperator::LessThan => ordering == Ordering::Less,
        LeafOperator::LessThanOrEqual => ordering != Ordering::Greater,
        LeafOperator::GreaterThan => ordering == Ordering::Greater,
        LeafOperator::GreaterThanOrEqual => ordering != Ordering::Less,
    };
    Ok(LeafEvaluation { result: if fired { ThreeValue::True } else { ThreeValue::False }, value: Some(value) })
}

fn bind_literal(path: &str, literal: &str) -> Result<LeafValue, String> {
    if path == ".latest-claim.claimed-at" {
        return literal
            .parse::<DateTime<Utc>>()
            .map(LeafValue::Timestamp)
            .map_err(|error| format!("invalid timestamp literal `{literal}`: {error}"));
    }
    Ok(LeafValue::Text(literal.to_string()))
}

fn compare_values(left: &LeafValue, right: &LeafValue) -> Result<Ordering, String> {
    match (left, right) {
        (LeafValue::Text(left), LeafValue::Text(right)) => Ok(left.cmp(right)),
        (LeafValue::Timestamp(left), LeafValue::Timestamp(right)) => Ok(left.cmp(right)),
        _ => Err("leaf value and bound literal have different types".to_string()),
    }
}

pub struct ConvoyLeafSubject<'a>(pub &'a ResourceObject<Convoy>);

impl LeafSubject for ConvoyLeafSubject<'_> {
    fn kind(&self) -> LeafKind {
        LeafKind::Convoy
    }

    fn value(&self, field_path: &str) -> Option<LeafValue> {
        match field_path {
            ".status.phase" => self.0.status.as_ref().map(|status| LeafValue::Text(format!("{:?}", status.phase))),
            _ => None,
        }
    }
}

pub struct VesselLeafSubject<'a>(pub &'a ResourceObject<Vessel>);

impl LeafSubject for VesselLeafSubject<'_> {
    fn kind(&self) -> LeafKind {
        LeafKind::Vessel
    }

    fn value(&self, field_path: &str) -> Option<LeafValue> {
        match field_path {
            ".status.phase" => self.0.status.as_ref().map(|status| LeafValue::Text(format!("{:?}", status.phase))),
            _ => None,
        }
    }
}

pub struct WorkLeafSubject<'a> {
    pub work: &'a WorkState,
    pub crew: Option<&'a std::collections::BTreeMap<String, CrewWorkState>>,
}

impl WorkLeafSubject<'_> {
    fn latest_claim(&self) -> Option<&CrewWorkState> {
        self.crew?
            .values()
            .filter(|state| state.phase == CrewWorkPhase::Done)
            .filter(|state| state.finished_at.is_some())
            .max_by_key(|state| state.finished_at)
    }
}

impl LeafSubject for WorkLeafSubject<'_> {
    fn kind(&self) -> LeafKind {
        LeafKind::Work
    }

    fn value(&self, field_path: &str) -> Option<LeafValue> {
        match field_path {
            ".status.phase" => Some(LeafValue::Text(format!("{:?}", self.work.phase))),
            ".latest-claim.disposition" => self.latest_claim()?.disposition.clone().map(LeafValue::Text),
            ".latest-claim.claimed-at" => self.latest_claim()?.finished_at.map(LeafValue::Timestamp),
            _ => None,
        }
    }

    fn observed_at(&self) -> Option<DateTime<Utc>> {
        self.latest_claim()?.finished_at
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::LeafAddress;

    use super::*;

    struct FakeSubject {
        observed_at: Option<DateTime<Utc>>,
    }

    impl LeafSubject for FakeSubject {
        fn kind(&self) -> LeafKind {
            LeafKind::Convoy
        }

        fn value(&self, _field_path: &str) -> Option<LeafValue> {
            Some(LeafValue::Text("Landed".to_string()))
        }

        fn observed_at(&self) -> Option<DateTime<Utc>> {
            self.observed_at
        }
    }

    fn leaf(path: &str) -> Leaf {
        Leaf {
            address: LeafAddress::Convoy { name: "demo".to_string() },
            field_path: path.to_string(),
            operator: LeafOperator::Equal,
            literal: "Landed".to_string(),
        }
    }

    #[test]
    fn absent_and_stale_subjects_are_unknown() {
        assert_eq!(evaluate_leaf(&leaf(".status.phase"), None, None).expect("evaluate").result, ThreeValue::Unknown);
        let demand = "2026-08-03T20:00:00Z".parse().expect("timestamp");
        let stale = FakeSubject { observed_at: Some("2026-08-03T19:00:00Z".parse().expect("timestamp")) };
        assert_eq!(evaluate_leaf(&leaf(".status.phase"), Some(&stale), Some(demand)).expect("evaluate").result, ThreeValue::Unknown);
    }

    #[test]
    fn unknown_path_names_the_closed_vocabulary() {
        let error = admit_leaf(&leaf(".status.nope")).expect_err("path should be rejected");
        assert!(error.contains("admitted vocabulary"));
        assert!(error.contains("convoy.status.phase"));
        assert!(error.contains("work.latest-claim.disposition"));
    }

    #[test]
    fn text_leaf_rejects_order_operators_at_admission() {
        let mut leaf = leaf(".status.phase");
        leaf.operator = LeafOperator::GreaterThan;
        let error = admit_leaf(&leaf).expect_err("text ordering should be rejected");
        assert!(error.contains("operator `>` is not admitted"));
        assert!(error.contains("use `==` or `!=`"));
    }
}
