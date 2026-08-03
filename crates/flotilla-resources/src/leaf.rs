use std::{cmp::Ordering, time::Duration};

use chrono::{DateTime, Utc};
use flotilla_protocol::{Leaf, LeafKind, LeafOperator};

use crate::{ChangeRequest, Convoy, CrewWorkPhase, CrewWorkState, ResourceObject, Vessel, WorkState};

pub const ADMITTED_LEAF_VOCABULARY: &[(&str, &str)] = &[
    ("convoy", ".status.phase"),
    ("vessel", ".status.phase"),
    ("work", ".status.phase"),
    ("work", ".latest-claim.disposition"),
    ("work", ".latest-claim.claimed-at"),
    ("cr", ".state"),
    ("cr", ".head-sha"),
    ("cr", ".checks"),
    ("cr", ".review.actionable-at-head"),
    ("cr", ".mergeable"),
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
    fn observed_at(&self, _field_path: &str) -> Option<DateTime<Utc>> {
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
    if freshness_demand.is_some_and(|demand| subject.observed_at(&leaf.field_path).is_none_or(|observed_at| observed_at < demand)) {
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

    fn observed_at(&self, _field_path: &str) -> Option<DateTime<Utc>> {
        self.latest_claim()?.finished_at
    }
}

pub struct ChangeRequestLeafSubject<'a> {
    pub change_request: &'a ResourceObject<ChangeRequest>,
    pub now: DateTime<Utc>,
    pub stale_after: Duration,
}

impl ChangeRequestLeafSubject<'_> {
    fn field_observed_at(&self, field_path: &str) -> Option<DateTime<Utc>> {
        let status = self.change_request.status.as_ref()?;
        Some(match field_path {
            ".state" => status.state.observed_at,
            ".head-sha" => status.head_sha.observed_at,
            ".checks" => status.checks.observed_at,
            ".review.actionable-at-head" => status.review.actionable_at_head.observed_at,
            ".mergeable" => status.mergeable.observed_at,
            _ => return None,
        })
    }

    fn is_fresh(&self, field_path: &str) -> bool {
        self.field_observed_at(field_path)
            .and_then(|observed_at| self.now.signed_duration_since(observed_at).to_std().ok())
            .is_some_and(|age| age <= self.stale_after)
    }
}

impl LeafSubject for ChangeRequestLeafSubject<'_> {
    fn kind(&self) -> LeafKind {
        LeafKind::ChangeRequest
    }

    fn value(&self, field_path: &str) -> Option<LeafValue> {
        if !self.is_fresh(field_path) {
            return None;
        }
        let status = self.change_request.status.as_ref()?;
        let value = match field_path {
            ".state" => match status.state.value? {
                crate::ObservedChangeRequestState::Open => "open".to_string(),
                crate::ObservedChangeRequestState::Merged => "merged".to_string(),
                crate::ObservedChangeRequestState::Closed => "closed".to_string(),
            },
            ".head-sha" => status.head_sha.value.clone()?,
            ".checks" => match status.checks.value? {
                crate::ObservedChecks::Pass => "pass".to_string(),
                crate::ObservedChecks::Fail => "fail".to_string(),
                crate::ObservedChecks::Pending => "pending".to_string(),
            },
            ".review.actionable-at-head" => status.review.actionable_at_head.value?.to_string(),
            ".mergeable" => match status.mergeable.value? {
                crate::ObservedMergeability::Mergeable => "mergeable".to_string(),
                crate::ObservedMergeability::Conflicting => "conflicting".to_string(),
            },
            _ => return None,
        };
        Some(LeafValue::Text(value))
    }

    fn observed_at(&self, field_path: &str) -> Option<DateTime<Utc>> {
        self.field_observed_at(field_path)
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

        fn observed_at(&self, _field_path: &str) -> Option<DateTime<Utc>> {
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

    #[test]
    fn stale_change_request_field_is_structurally_unknown() {
        let observed_at = "2026-08-03T20:00:00Z".parse().expect("time");
        let object = ResourceObject::<ChangeRequest> {
            metadata: crate::ObjectMeta {
                name: "cr".to_string(),
                namespace: "flotilla".to_string(),
                resource_version: "1".to_string(),
                labels: Default::default(),
                annotations: Default::default(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: observed_at,
                merge: None,
            },
            spec: crate::ChangeRequestSpec::builder()
                .service("github.com".to_string())
                .scope("flotilla-org/flotilla".to_string())
                .number(1363)
                .observing_authority("feta".to_string())
                .build(),
            status: Some(crate::ChangeRequestStatus {
                state: crate::Observation::known(crate::ObservedChangeRequestState::Merged, observed_at),
                head_sha: crate::Observation::known("abc".to_string(), observed_at),
                checks: crate::Observation::known(crate::ObservedChecks::Pass, observed_at),
                review: crate::ChangeRequestReviewObservation { actionable_at_head: crate::Observation::known(false, observed_at) },
                mergeable: crate::Observation::known(crate::ObservedMergeability::Mergeable, observed_at),
            }),
        };
        let subject = ChangeRequestLeafSubject {
            change_request: &object,
            now: "2026-08-03T20:03:01Z".parse().expect("time"),
            stale_after: Duration::from_secs(180),
        };
        let leaf = Leaf {
            address: LeafAddress::ChangeRequest {
                service: "github.com".to_string(),
                scope: "flotilla-org/flotilla".to_string(),
                number: 1363,
            },
            field_path: ".state".to_string(),
            operator: LeafOperator::Equal,
            literal: "merged".to_string(),
        };
        assert_eq!(evaluate_leaf(&leaf, Some(&subject), None).expect("evaluate").result, ThreeValue::Unknown);
    }
}
