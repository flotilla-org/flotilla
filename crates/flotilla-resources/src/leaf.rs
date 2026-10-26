use std::{cmp::Ordering, time::Duration};

use chrono::{DateTime, Utc};
use flotilla_protocol::{Leaf, LeafAddress, LeafKind, LeafOperator};

use crate::{Artifact, ChangeRequest, Convoy, CrewWorkPhase, CrewWorkState, Issue, ResourceObject, Vessel, WorkState};

pub const ADMITTED_LEAF_VOCABULARY: &[(&str, &str)] = &[
    ("convoy", ".status.phase"),
    ("convoy", ".status.stalled"),
    ("vessel", ".status.phase"),
    ("work", ".status.phase"),
    ("work", ".latest-claim.disposition"),
    ("work", ".latest-claim.claimed-at"),
    ("cr", ".state"),
    ("cr", ".head-sha"),
    ("cr", ".checks"),
    ("cr", ".review.actionable-at-head"),
    ("cr", ".mergeable"),
    ("cr", ".ready"),
    ("issue", ".state"),
    ("issue", ".updated-at"),
    ("artifact", ".exists"),
];

/// Phrasing is part of the closed actor-leaf vocabulary. An actor row without
/// one cannot be armed, since it could never receive a meaningful nudge.
pub fn actor_obligation(leaf: &Leaf) -> Result<String, String> {
    match (&leaf.address, crew_role_path(&leaf.field_path), leaf.operator, leaf.literal.as_str()) {
        (LeafAddress::Work { work, .. }, Some(role), LeafOperator::Equal, "Done") => Ok(format!(
            "You owe deliverables for {work}/{role}: submit your PR and decision-ledger artifact; your part ends when your promises are kept. Run `crew stall --reason <infra|scope|decision|access|other> --message …` if blocked."
        )),
        _ => Err(format!("actor leaf `{leaf:?}` has no obligation phrasing")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreeValue {
    True,
    False,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LeafValue {
    Text(String),
    Number(f64),
    Timestamp(DateTime<Utc>),
}

impl std::fmt::Display for LeafValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(value) => f.write_str(value),
            Self::Number(value) => write!(f, "{value}"),
            Self::Timestamp(value) => write!(f, "{}", value.to_rfc3339()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
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
    /// Immutable observations satisfy subsequent freshness demands too.
    fn is_final(&self) -> bool {
        false
    }
}

pub fn admit_leaf(leaf: &Leaf) -> Result<(), String> {
    let kind = leaf.address.kind();
    let crew_field = kind == LeafKind::Work && crew_role_path(&leaf.field_path).is_some();
    let issue_label = kind == LeafKind::Issue && issue_label_path(&leaf.field_path).is_some();
    let artifact_summary = kind == LeafKind::Artifact
        && leaf.field_path.strip_prefix(".summary.").is_some_and(|field| !field.is_empty() && !field.contains('.'));
    let admitted = crew_field
        || issue_label
        || artifact_summary
        || ADMITTED_LEAF_VOCABULARY.iter().any(|(candidate_kind, path)| *candidate_kind == kind.to_string() && *path == leaf.field_path);
    if !admitted {
        let mut vocabulary = ADMITTED_LEAF_VOCABULARY.iter().map(|(kind, path)| format!("{kind}{path}")).collect::<Vec<_>>();
        vocabulary.extend([
            "work.crew.<role>.phase".to_string(),
            "issue.labels.<label>".to_string(),
            "artifact.summary.<field>".to_string(),
        ]);
        let vocabulary = vocabulary.join(", ");
        return Err(format!("leaf path `{kind}{}` is not admitted; admitted vocabulary: {vocabulary}", leaf.field_path));
    }
    if kind == LeafKind::Artifact && leaf.field_path == ".exists" && (leaf.operator != LeafOperator::Equal || leaf.literal != "true") {
        return Err("artifact existence leaf must compare `.exists == true`".to_string());
    }
    if leaf.field_path != ".latest-claim.claimed-at"
        && leaf.field_path != ".updated-at"
        && !matches!(leaf.operator, LeafOperator::Equal | LeafOperator::NotEqual)
    {
        return Err(format!("operator `{}` is not admitted for text leaf `{kind}{}`; use `==` or `!=`", leaf.operator, leaf.field_path));
    }
    validate_leaf_literal(kind, &leaf.field_path, &leaf.literal)
}

/// Validate the typed value independently of an address, so template admission
/// and concrete leaf admission use the same literal rules.
pub fn validate_leaf_literal(kind: LeafKind, field_path: &str, literal: &str) -> Result<(), String> {
    if field_path == ".latest-claim.claimed-at" || field_path == ".updated-at" {
        literal
            .parse::<DateTime<Utc>>()
            .map_err(|error| format!("invalid timestamp literal `{literal}` for {kind}{field_path}: {error}"))?;
    }
    if kind == LeafKind::Issue && issue_label_path(field_path).is_some() && !matches!(literal, "true" | "false") {
        return Err(format!("issue label leaf literal must be `true` or `false`, got `{literal}`"));
    }
    Ok(())
}

fn crew_role_path(path: &str) -> Option<&str> {
    let role = path.strip_prefix(".crew.")?.strip_suffix(".phase")?;
    (!role.is_empty() && role.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))).then_some(role)
}

fn issue_label_path(path: &str) -> Option<&str> {
    path.strip_prefix(".labels.").filter(|label| !label.is_empty())
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
    if !subject.is_final()
        && freshness_demand.is_some_and(|demand| subject.observed_at(&leaf.field_path).is_none_or(|observed_at| observed_at < demand))
    {
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
    if matches!(path, ".latest-claim.claimed-at" | ".updated-at") {
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
        (LeafValue::Number(left), LeafValue::Number(right)) => {
            left.partial_cmp(right).ok_or_else(|| "leaf number cannot be compared".to_string())
        }
        (LeafValue::Timestamp(left), LeafValue::Timestamp(right)) => Ok(left.cmp(right)),
        _ => Err("leaf value and bound literal have different types".to_string()),
    }
}

pub struct ArtifactLeafSubject<'a>(pub &'a ResourceObject<Artifact>);

impl LeafSubject for ArtifactLeafSubject<'_> {
    fn kind(&self) -> LeafKind {
        LeafKind::Artifact
    }

    fn value(&self, field_path: &str) -> Option<LeafValue> {
        if field_path == ".exists" {
            return Some(LeafValue::Text("true".to_string()));
        }
        let field = field_path.strip_prefix(".summary.")?;
        let value = self.0.spec.summary.get(field)?;
        match value {
            serde_json::Value::String(value) => Some(LeafValue::Text(value.clone())),
            serde_json::Value::Bool(value) => Some(LeafValue::Text(value.to_string())),
            serde_json::Value::Number(value) => Some(LeafValue::Text(value.to_string())),
            _ => None,
        }
    }

    fn observed_at(&self, _field_path: &str) -> Option<DateTime<Utc>> {
        Some(self.0.spec.recorded_at.unwrap_or(self.0.metadata.creation_timestamp))
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
            ".status.stalled" => self.0.status.as_ref().map(|status| LeafValue::Text(status.stalled.is_some().to_string())),
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
            path if crew_role_path(path).is_some() => {
                let role = crew_role_path(path).expect("validated crew path");
                self.crew?.get(role).map(|state| LeafValue::Text(format!("{:?}", state.phase)))
            }
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

pub struct IssueLeafSubject<'a> {
    pub issue: &'a ResourceObject<Issue>,
    pub now: DateTime<Utc>,
    pub stale_after: Duration,
}

impl IssueLeafSubject<'_> {
    fn field_observed_at(&self, field_path: &str) -> Option<DateTime<Utc>> {
        let status = self.issue.status.as_ref()?;
        match field_path {
            ".state" => Some(status.state.observed_at),
            ".updated-at" => Some(status.updated_at.observed_at),
            path if issue_label_path(path).is_some() => Some(status.labels.observed_at),
            _ => None,
        }
    }
}

impl LeafSubject for IssueLeafSubject<'_> {
    fn kind(&self) -> LeafKind {
        LeafKind::Issue
    }

    fn value(&self, field_path: &str) -> Option<LeafValue> {
        let observed_at = self.field_observed_at(field_path)?;
        if self.now.signed_duration_since(observed_at).to_std().ok().is_none_or(|age| age > self.stale_after) {
            return None;
        }
        let status = self.issue.status.as_ref()?;
        match field_path {
            ".state" => Some(LeafValue::Text(
                match status.state.value? {
                    crate::ObservedIssueState::Open => "open",
                    crate::ObservedIssueState::Closed => "closed",
                }
                .to_string(),
            )),
            ".updated-at" => status.updated_at.value.map(LeafValue::Timestamp),
            path => {
                let label = issue_label_path(path)?;
                Some(LeafValue::Text(status.labels.value.as_ref()?.iter().any(|value| value == label).to_string()))
            }
        }
    }

    fn observed_at(&self, field_path: &str) -> Option<DateTime<Utc>> {
        self.field_observed_at(field_path)
    }
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
            ".ready" => {
                if status.state.value == Some(crate::ObservedChangeRequestState::Merged) {
                    status.state.observed_at
                } else {
                    status.state.observed_at.min(status.checks.observed_at)
                }
            }
            _ => return None,
        })
    }

    pub(crate) fn is_fresh(&self, field_path: &str) -> bool {
        // Every field of a merged request is final and cannot age out.
        if self.change_request.status.as_ref().is_some_and(|status| status.state.value == Some(crate::ObservedChangeRequestState::Merged)) {
            return true;
        }
        self.field_observed_at(field_path)
            .and_then(|observed_at| self.now.signed_duration_since(observed_at).to_std().ok())
            .is_some_and(|age| age <= self.stale_after)
    }
}

impl LeafSubject for ChangeRequestLeafSubject<'_> {
    fn is_final(&self) -> bool {
        self.change_request.status.as_ref().is_some_and(|status| status.state.value == Some(crate::ObservedChangeRequestState::Merged))
    }

    fn kind(&self) -> LeafKind {
        LeafKind::ChangeRequest
    }

    fn value(&self, field_path: &str) -> Option<LeafValue> {
        if !self.is_fresh(field_path) {
            return None;
        }
        let status = self.change_request.status.as_ref()?;
        let value = match field_path {
            ".ready" => {
                let merged = status.state.value == Some(crate::ObservedChangeRequestState::Merged);
                let open = status.state.value == Some(crate::ObservedChangeRequestState::Open);
                let checks_pass = status.checks.value == Some(crate::ObservedChecks::Pass);
                let mergeable = status.mergeable.value != Some(crate::ObservedMergeability::Conflicting);
                (merged || (open && checks_pass && mergeable)).to_string()
            }
            ".state" => match status.state.value? {
                crate::ObservedChangeRequestState::Open => "open".to_string(),
                crate::ObservedChangeRequestState::Draft => "draft".to_string(),
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
    fn crew_leaf_path_rejects_overlapping_or_empty_roles() {
        for path in [".crew.phase", ".crew..phase", ".crew.a/b.phase"] {
            let leaf = Leaf {
                address: flotilla_protocol::LeafAddress::Work { convoy: "demo".into(), work: "work".into() },
                field_path: path.into(),
                operator: LeafOperator::Equal,
                literal: "Done".into(),
            };
            assert!(admit_leaf(&leaf).is_err(), "{path} must be rejected");
            assert_eq!(crew_role_path(path), None);
        }
        assert_eq!(crew_role_path(".crew.coder.phase"), Some("coder"));
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
    fn every_merged_field_is_fresh_forever() {
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
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
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
        // Issue #3710: every merged field is final; checks and reviews remain usable after the TTL.
        assert_eq!(evaluate_leaf(&leaf, Some(&subject), None).expect("evaluate").result, ThreeValue::True);
        assert_eq!(subject.value(".ready"), Some(LeafValue::Text("true".to_string())));
        assert_eq!(subject.value(".checks"), Some(LeafValue::Text("pass".to_string())));
        assert_eq!(subject.value(".review.actionable-at-head"), Some(LeafValue::Text("false".to_string())));
        for field in [".state", ".ready", ".head-sha", ".checks", ".review.actionable-at-head", ".mergeable"] {
            assert!(subject.is_fresh(field), "{field}");
            let value = subject.value(field).expect("known field").to_string();
            let demanded = Leaf { field_path: field.to_string(), literal: value, ..leaf.clone() };
            assert_eq!(evaluate_leaf(&demanded, Some(&subject), Some(subject.now)).expect("final freshness").result, ThreeValue::True);
        }
        // Reopenable states remain known at the TTL boundary and expire just after it.
        for state in
            [crate::ObservedChangeRequestState::Open, crate::ObservedChangeRequestState::Draft, crate::ObservedChangeRequestState::Closed]
        {
            let mut reopenable = object.clone();
            reopenable.status.as_mut().expect("status").state = crate::Observation::known(state, observed_at);
            for age in [180, 181] {
                let subject = ChangeRequestLeafSubject {
                    change_request: &reopenable,
                    now: observed_at + chrono::Duration::seconds(age),
                    stale_after: Duration::from_secs(180),
                };
                assert_eq!(subject.value(".state").is_some(), age == 180);
            }
        }
    }

    #[test]
    fn ready_pr_with_unknown_mergeability_remains_ready_but_conflict_does_not() {
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
                title: Default::default(),
                author: Default::default(),
                review_decision: Default::default(),
                review_requested_from_owner: Default::default(),
                state: crate::Observation::known(crate::ObservedChangeRequestState::Open, observed_at),
                head_sha: crate::Observation::known("abc".to_string(), observed_at),
                checks: crate::Observation::known(crate::ObservedChecks::Pass, observed_at),
                review: crate::ChangeRequestReviewObservation { actionable_at_head: crate::Observation::known(false, observed_at) },
                mergeable: crate::Observation::unknown(DateTime::<Utc>::MIN_UTC),
            }),
        };
        let leaf = Leaf {
            address: LeafAddress::ChangeRequest {
                service: "github.com".to_string(),
                scope: "flotilla-org/flotilla".to_string(),
                number: 1363,
            },
            field_path: ".ready".to_string(),
            operator: LeafOperator::Equal,
            literal: "true".to_string(),
        };
        let subject = ChangeRequestLeafSubject { change_request: &object, now: observed_at, stale_after: Duration::from_secs(180) };
        assert_eq!(evaluate_leaf(&leaf, Some(&subject), None).expect("evaluate").result, ThreeValue::True);
        let mut conflicting = object;
        conflicting.status.as_mut().expect("status").mergeable =
            crate::Observation::known(crate::ObservedMergeability::Conflicting, observed_at);
        let subject = ChangeRequestLeafSubject { change_request: &conflicting, now: observed_at, stale_after: Duration::from_secs(180) };
        assert_eq!(evaluate_leaf(&leaf, Some(&subject), None).expect("evaluate").result, ThreeValue::False);
    }
}
