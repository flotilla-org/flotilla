//! Forge observations on the engine: ChangeRequest, Issue and ForgeRead, plain records (a spec and
//! no machine) written by the host that observes their source, at its own home, only when an
//! observed value changes.
//!
//! A record holds the observed values alone. When each was last confirmed is a judgement: the
//! observing home's change-stream watermark while its reads succeed, and `failing_since` once they
//! fail. So the old per-field `observed_at`, the observing authority and the demand times are not
//! stored. ChangeRequest's convoy history is not stored either: the convoys hold those links.

use flotilla_protocol::IssueSource;
use flotilla_resources::{
    ChangeRequestSpec, ChangeRequestStatus, ForgeReadRequest, ForgeReadStatus, IssueSpec, IssueStatus, ObservedChangeRequestState,
    ObservedChecks, ObservedIssueState, ObservedMergeability, ObservedReviewDecision,
};
use flotilla_schema::{KindRegistry, RegistryError, Stored, Timestamp};
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

/// The observing home's view of one change request: kind code `cq`, named by the subject's record
/// name. A null value is one the forge has not reported.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "ChangeRequest", code = "cq", index(name = "by_subject", eq(service, scope, number)))]
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
        }
    }
}

/// The observing home's view of one issue: kind code `iu`, named by the subject's record name.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "Issue", code = "iu", index(name = "by_subject", eq(service, scope, number)))]
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
        }
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
    registry.register::<ChangeRequestRecord>()?;
    registry.register::<IssueRecord>()?;
    registry.register::<ForgeReadRecord>()
}

/// The forge observation kinds alone, for their DDL snapshot.
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    register(&mut registry)?;
    Ok(registry)
}
