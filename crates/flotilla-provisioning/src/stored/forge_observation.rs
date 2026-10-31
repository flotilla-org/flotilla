//! Forge observations on the engine: ChangeRequest, Issue and ForgeRead, plain records (a spec and
//! no machine) written by the host that observes their source, at its own home, only when an
//! observed value changes.
//!
//! A record holds the observed values alone. When each was last confirmed is a judgement: the
//! observing home's change-stream watermark while its reads succeed, and `failing_since` once they
//! fail. So the old per-field `observed_at`, the observing authority and the demand times are not
//! stored. ChangeRequest's convoy history is not stored either: the convoys hold those links.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use flotilla_protocol::IssueSource;
use flotilla_resources::{
    ChangeRequestReviewObservation, ChangeRequestSpec, ChangeRequestStatus, ForgeReadRequest, ForgeReadStatus, IssueSpec, IssueStatus,
    Observation, ObservedChangeRequestState, ObservedChecks, ObservedIssueState, ObservedMergeability, ObservedReviewDecision,
};
use flotilla_schema::{Kind, KindRegistry, RegistryError, Stored, TableSet, Timestamp};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum ChangeRequestState {
    #[store(code = "op")]
    Open,
    #[store(code = "dr")]
    Draft,
    #[store(code = "mg")]
    Merged,
    #[store(code = "cl")]
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum ReviewDecision {
    #[store(code = "ap")]
    Approved,
    #[store(code = "cr")]
    ChangesRequested,
    #[store(code = "rq")]
    Required,
    /// The forge reports no review decision; an unobserved one is a null column.
    #[store(code = "no")]
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum Checks {
    #[store(code = "pa")]
    Pass,
    #[store(code = "fa")]
    Fail,
    #[store(code = "pe")]
    Pending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum Mergeability {
    #[store(code = "me")]
    Mergeable,
    #[store(code = "co")]
    Conflicting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum IssueState {
    #[store(code = "op")]
    Open,
    #[store(code = "cl")]
    Closed,
}

impl From<ObservedChangeRequestState> for ChangeRequestState {
    fn from(state: ObservedChangeRequestState) -> Self {
        match state {
            ObservedChangeRequestState::Open => Self::Open,
            ObservedChangeRequestState::Draft => Self::Draft,
            ObservedChangeRequestState::Merged => Self::Merged,
            ObservedChangeRequestState::Closed => Self::Closed,
        }
    }
}

impl From<ChangeRequestState> for ObservedChangeRequestState {
    fn from(state: ChangeRequestState) -> Self {
        match state {
            ChangeRequestState::Open => Self::Open,
            ChangeRequestState::Draft => Self::Draft,
            ChangeRequestState::Merged => Self::Merged,
            ChangeRequestState::Closed => Self::Closed,
        }
    }
}

impl From<ObservedReviewDecision> for ReviewDecision {
    fn from(decision: ObservedReviewDecision) -> Self {
        match decision {
            ObservedReviewDecision::Approved => Self::Approved,
            ObservedReviewDecision::ChangesRequested => Self::ChangesRequested,
            ObservedReviewDecision::Required => Self::Required,
            ObservedReviewDecision::None => Self::None,
        }
    }
}

impl From<ReviewDecision> for ObservedReviewDecision {
    fn from(decision: ReviewDecision) -> Self {
        match decision {
            ReviewDecision::Approved => Self::Approved,
            ReviewDecision::ChangesRequested => Self::ChangesRequested,
            ReviewDecision::Required => Self::Required,
            ReviewDecision::None => Self::None,
        }
    }
}

impl From<ObservedChecks> for Checks {
    fn from(checks: ObservedChecks) -> Self {
        match checks {
            ObservedChecks::Pass => Self::Pass,
            ObservedChecks::Fail => Self::Fail,
            ObservedChecks::Pending => Self::Pending,
        }
    }
}

impl From<Checks> for ObservedChecks {
    fn from(checks: Checks) -> Self {
        match checks {
            Checks::Pass => Self::Pass,
            Checks::Fail => Self::Fail,
            Checks::Pending => Self::Pending,
        }
    }
}

impl From<ObservedMergeability> for Mergeability {
    fn from(mergeable: ObservedMergeability) -> Self {
        match mergeable {
            ObservedMergeability::Mergeable => Self::Mergeable,
            ObservedMergeability::Conflicting => Self::Conflicting,
        }
    }
}

impl From<Mergeability> for ObservedMergeability {
    fn from(mergeable: Mergeability) -> Self {
        match mergeable {
            Mergeability::Mergeable => Self::Mergeable,
            Mergeability::Conflicting => Self::Conflicting,
        }
    }
}

impl From<ObservedIssueState> for IssueState {
    fn from(state: ObservedIssueState) -> Self {
        match state {
            ObservedIssueState::Open => Self::Open,
            ObservedIssueState::Closed => Self::Closed,
        }
    }
}

impl From<IssueState> for ObservedIssueState {
    fn from(state: IssueState) -> Self {
        match state {
            IssueState::Open => Self::Open,
            IssueState::Closed => Self::Closed,
        }
    }
}

/// An issue another record refers to, in the stored spelling of its source: one row of a list
/// of links.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Stored)]
pub struct IssueSubject {
    pub service: String,
    pub scope: String,
    pub number: u64,
}

fn at<T>(value: Option<T>, observed_at: DateTime<Utc>) -> Observation<T> {
    Observation { value, observed_at }
}

/// The observing home's view of one change request: kind code `cq`, named by the subject's record
/// name. A null value is one the forge has not reported; a list is unreported until its
/// `_observed` flag is set.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
// clippy::duplicated_attributes reads the key columns the two `index(...)` declarations share as
// repeated attributes.
#[allow(clippy::duplicated_attributes)]
#[store(
    kind = "ChangeRequest",
    code = "cq",
    index(name = "by_subject", eq(service, scope, number)),
    index(name = "by_branch", eq(service, scope, head_branch)),
    index(name = "by_closed_issue", eq(closes))
)]
pub struct ChangeRequestRecord {
    pub service: String,
    pub scope: String,
    pub number: u64,
    pub title: Option<String>,
    pub author: Option<String>,
    pub state: Option<ChangeRequestState>,
    pub review_decision: Option<ReviewDecision>,
    pub review_requested_from_owner: Option<bool>,
    pub head_sha: Option<String>,
    pub checks: Option<Checks>,
    pub actionable_at_head: Option<bool>,
    pub mergeable: Option<Mergeability>,
    /// Since when the observer's reads of this subject have failed; the values were last confirmed then.
    pub failing_since: Option<Timestamp>,
    pub head_branch: Option<String>,
    pub base_branch: Option<String>,
    /// The issues the forge says this change request closes.
    #[store(child)]
    pub closes: Vec<IssueSubject>,
    pub closes_observed: bool,
}

impl ChangeRequestRecord {
    /// The record of an old copy's subject and observed values.
    pub fn from_old(spec: &ChangeRequestSpec, status: &ChangeRequestStatus) -> Self {
        Self {
            service: spec.service.clone(),
            scope: spec.scope.clone(),
            number: spec.number,
            title: status.title.value.clone(),
            author: status.author.value.clone(),
            state: status.state.value.map(Into::into),
            review_decision: status.review_decision.value.map(Into::into),
            review_requested_from_owner: status.review_requested_from_owner.value,
            head_sha: status.head_sha.value.clone(),
            checks: status.checks.value.map(Into::into),
            actionable_at_head: status.review.actionable_at_head.value,
            mergeable: status.mergeable.value.map(Into::into),
            failing_since: None,
            head_branch: None,
            base_branch: None,
            closes: Vec::new(),
            closes_observed: false,
        }
    }

    /// A record of `spec`'s change request with nothing observed yet.
    pub fn unobserved(spec: &ChangeRequestSpec) -> Self {
        Self {
            service: spec.service.clone(),
            scope: spec.scope.clone(),
            number: spec.number,
            title: None,
            author: None,
            state: None,
            review_decision: None,
            review_requested_from_owner: None,
            head_sha: None,
            checks: None,
            actionable_at_head: None,
            mergeable: None,
            failing_since: None,
            head_branch: None,
            base_branch: None,
            closes: Vec::new(),
            closes_observed: false,
        }
    }

    /// The old spec, observed by `home`; convoy history is the convoys' to tell.
    pub fn old_spec(&self, home: &str) -> ChangeRequestSpec {
        ChangeRequestSpec {
            service: self.service.clone(),
            scope: self.scope.clone(),
            number: self.number,
            observing_authority: home.to_owned(),
            subject_of: Vec::new(),
        }
    }

    /// The old status, every value last confirmed at `observed_at`.
    pub fn old_status(&self, observed_at: DateTime<Utc>) -> ChangeRequestStatus {
        ChangeRequestStatus {
            title: at(self.title.clone(), observed_at),
            author: at(self.author.clone(), observed_at),
            review_decision: at(self.review_decision.map(Into::into), observed_at),
            review_requested_from_owner: at(self.review_requested_from_owner, observed_at),
            state: at(self.state.map(Into::into), observed_at),
            head_sha: at(self.head_sha.clone(), observed_at),
            checks: at(self.checks.map(Into::into), observed_at),
            review: ChangeRequestReviewObservation { actionable_at_head: at(self.actionable_at_head, observed_at) },
            mergeable: at(self.mergeable.map(Into::into), observed_at),
        }
    }
}

/// The observing home's view of one issue: kind code `iu`, named by the subject's record name. The
/// parent and a list are unreported until their `_observed` flag is set.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "Issue",
    code = "iu",
    index(name = "by_subject", eq(service, scope, number)),
    index(name = "by_parent", eq(parent_service, parent_scope, parent_number)),
    index(name = "by_blocker", eq(blocked_by))
)]
pub struct IssueRecord {
    pub service: String,
    pub scope: String,
    pub number: u64,
    pub title: Option<String>,
    #[store(cbor)]
    pub assignees: Option<Vec<String>>,
    pub state: Option<IssueState>,
    #[store(cbor)]
    pub labels: Option<Vec<String>>,
    pub updated_at: Option<Timestamp>,
    /// Since when the observer's reads of this issue have failed; the values were last confirmed then.
    pub failing_since: Option<Timestamp>,
    pub body: Option<String>,
    /// The forge's issue type name, uninterpreted.
    pub issue_type: Option<String>,
    /// The parent issue: all three set, or none; unreported until `parent_observed` is set.
    pub parent_service: Option<String>,
    pub parent_scope: Option<String>,
    pub parent_number: Option<u64>,
    pub parent_observed: bool,
    #[store(child)]
    pub blocked_by: Vec<IssueSubject>,
    pub blocked_by_observed: bool,
    /// Each issue field's value by field name, as the forge spells it: a single-select field's
    /// option name, a number's decimal digits.
    #[store(cbor)]
    pub fields: Option<BTreeMap<String, String>>,
}

impl IssueRecord {
    /// The record of an old copy's subject and observed values.
    pub fn from_old(spec: &IssueSpec, status: &IssueStatus) -> Self {
        Self {
            service: spec.service.clone(),
            scope: spec.scope.clone(),
            number: spec.number,
            title: status.title.value.clone(),
            assignees: status.assignees.value.clone(),
            state: status.state.value.map(Into::into),
            labels: status.labels.value.clone(),
            updated_at: status.updated_at.value.as_ref().map(Timestamp::from_datetime),
            failing_since: None,
            body: None,
            issue_type: None,
            parent_service: None,
            parent_scope: None,
            parent_number: None,
            parent_observed: false,
            blocked_by: Vec::new(),
            blocked_by_observed: false,
            fields: None,
        }
    }

    /// A record of `spec`'s issue with nothing observed yet.
    pub fn unobserved(spec: &IssueSpec) -> Self {
        Self {
            service: spec.service.clone(),
            scope: spec.scope.clone(),
            number: spec.number,
            title: None,
            assignees: None,
            state: None,
            labels: None,
            updated_at: None,
            failing_since: None,
            body: None,
            issue_type: None,
            parent_service: None,
            parent_scope: None,
            parent_number: None,
            parent_observed: false,
            blocked_by: Vec::new(),
            blocked_by_observed: false,
            fields: None,
        }
    }

    /// The old spec, observed by `home`.
    pub fn old_spec(&self, home: &str) -> IssueSpec {
        IssueSpec { service: self.service.clone(), scope: self.scope.clone(), number: self.number, observing_authority: home.to_owned() }
    }

    /// The old status, every value last confirmed at `observed_at`; refused when `updated_at` lies
    /// outside chrono's range.
    pub fn old_status(&self, observed_at: DateTime<Utc>) -> Result<IssueStatus, String> {
        let updated_at =
            self.updated_at.map(|at| at.to_datetime().ok_or_else(|| format!("time {} is out of range", at.micros()))).transpose()?;
        Ok(IssueStatus {
            title: at(self.title.clone(), observed_at),
            assignees: at(self.assignees.clone(), observed_at),
            state: at(self.state.map(Into::into), observed_at),
            labels: at(self.labels.clone(), observed_at),
            updated_at: at(updated_at, observed_at),
        })
    }
}

/// A forge read's successful result, read whole; `value` is null when the read found nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadValue {
    pub value: serde_json::Value,
}

/// The observing home's latest result of one forge read: kind code `fr`, named by the read's
/// `forge_read_name`.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "ForgeRead", code = "fr")]
pub struct ForgeReadRecord {
    /// The read's canonical source.
    pub service: String,
    pub scope: String,
    /// The operation and its parameters, read whole.
    #[store(cbor)]
    pub request: ForgeReadRequest,
    /// The last successful result; none until a read succeeds.
    #[store(cbor)]
    pub value: Option<ReadValue>,
    pub error: Option<String>,
    pub retry_at: Option<Timestamp>,
}

impl ForgeReadRecord {
    /// The record of an old read's source, request and latest status.
    pub fn from_old(source: &IssueSource, request: &ForgeReadRequest, status: &ForgeReadStatus) -> Self {
        Self {
            service: source.service.clone(),
            scope: source.scope.clone(),
            request: request.clone(),
            value: status.value.clone().map(|value| ReadValue { value }),
            error: status.error.clone(),
            retry_at: status.retry_at.as_ref().map(Timestamp::from_datetime),
        }
    }

    pub fn source(&self) -> IssueSource {
        IssueSource { service: self.service.clone(), scope: self.scope.clone() }
    }
}

/// Registers the three kinds into `registry` under their codes, `cq`, `iu` and `fr`: plain kinds.
pub fn register(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    registry.register_table_set(added_since_previous_generation::<ChangeRequestRecord>(
        &["head_branch", "base_branch"],
        &["closes_observed"],
    ))?;
    registry.register_table_set(added_since_previous_generation::<IssueRecord>(
        &["body", "issue_type", "parent_service", "parent_scope", "parent_number", "fields"],
        &["parent_observed", "blocked_by_observed"],
    ))?;
    registry.register::<ForgeReadRecord>()
}

/// `K`'s tables with root columns the previous generation's table lacks marked, so opening its
/// store adds them: `absent` as unobserved, `unset` flags as false. ADR 0047 N→N+1: remove the
/// marks, and the test helper `previous_generation_tables`, after the next fleet roll.
fn added_since_previous_generation<K: Kind>(absent: &[&str], unset: &[&str]) -> TableSet {
    let mut set = K::table_set();
    for (name, flag) in absent.iter().map(|name| (name, false)).chain(unset.iter().map(|name| (name, true))) {
        let column = set.tables[0].columns.iter_mut().find(|column| column.name == *name);
        let column = column.unwrap_or_else(|| panic!("{} has no root column {name}", K::KIND));
        if flag {
            column.mark_previous_generation_default(0);
        } else {
            column.mark_previous_generation_absent();
        }
        assert!(column.previous_generation_value().is_some(), "{} column {name} cannot be added to an older table", K::KIND);
    }
    set
}

/// The forge observation kinds alone, for their DDL snapshot.
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    register(&mut registry)?;
    Ok(registry)
}
