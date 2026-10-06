//! Durable receiver-homed agent inputs. Producers describe intent; delivery
//! adapters record evidence on the receiver's authority.
use chrono::{DateTime, Utc};
use flotilla_protocol::{Leaf, ResourceRef};
use serde::{Deserialize, Serialize};

use crate::{ApiPaths, ControllerRetry, InputMeta, ReplicationClass, Resource, ResourceError, StatusPatch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message;

impl Resource for Message {
    type Spec = MessageSpec;
    type Status = MessageStatus;
    type StatusPatch = MessageStatusPatch;
    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "messages", kind: "Message" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::HomeBoundRuntime;

    fn validate_spec(_meta: &InputMeta, spec: &Self::Spec) -> Result<(), ResourceError> {
        crate::validate_message_address(&spec.sender)?;
        crate::validate_message_address(&spec.receiver)?;
        if spec.interrupting && spec.relation != MessageRelation::Supervisor {
            return Err(ResourceError::invalid("only supervisor messages may interrupt an active turn"));
        }
        if let MessageExpectation::Outcome { condition } = &spec.expectation {
            crate::admit_leaf(condition).map_err(ResourceError::invalid)?;
        }
        if spec.subject.as_ref().is_some_and(|subject| !spec.references.contains(subject)) {
            return Err(ResourceError::invalid("message subject must be one of its references"));
        }
        Ok(())
    }

    fn validate_spec_update(current: &Self::Spec, requested: &Self::Spec) -> Result<(), ResourceError> {
        if current == requested {
            Ok(())
        } else {
            Err(ResourceError::invalid("Message spec is immutable after creation"))
        }
    }

    fn validate_status_update(current: Option<&Self::Status>, requested: &Self::Status) -> Result<(), ResourceError> {
        if requested.phase.is_waiting() && requested.reason.as_deref().is_none_or(str::is_empty) {
            return Err(ResourceError::invalid("a waiting message must explain its reason"));
        }
        if requested.phase.has_delivery_evidence()
            && requested
                .resolved_receiver
                .as_ref()
                .is_none_or(|receiver| receiver.crew_id.is_empty() || receiver.session.is_empty() || receiver.evidence.is_empty())
        {
            return Err(ResourceError::invalid("delivered message requires a resolved receiver and transport evidence"));
        }
        if requested.canonical_predecessor.as_ref().is_some_and(|reference| {
            requested.phase != MessagePhase::Superseded
                || reference.kind != "Message"
                || reference.namespace.is_empty()
                || reference.name.is_empty()
        }) {
            return Err(ResourceError::invalid("canonical suppression requires a superseded Message reference"));
        }
        if let Some(current) = current {
            if current.accepted_sequence.is_some() && current.accepted_sequence != requested.accepted_sequence {
                return Err(ResourceError::invalid("message acceptance order cannot change"));
            }
            if current.phase.is_terminal() && current != requested {
                return Err(ResourceError::invalid("terminal message status is immutable"));
            }
            if current.resolved_receiver.is_some() && current.resolved_receiver != requested.resolved_receiver {
                return Err(ResourceError::invalid("message delivery evidence cannot be changed or discarded"));
            }
            if current.phase.has_delivery_evidence() && !requested.phase.has_delivery_evidence() && !requested.phase.is_terminal() {
                return Err(ResourceError::invalid("delivered message cannot return to the pending queue"));
            }
            if current.phase == requested.phase && current.reason == requested.reason && current.since != requested.since {
                return Err(ResourceError::invalid("continuing message state must preserve its since timestamp"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct MessageSpec {
    pub sender: String,
    pub receiver: String,
    pub relation: MessageRelation,
    pub body: String,
    #[serde(default)]
    #[builder(default)]
    pub references: Vec<MessageReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<MessageReference>,
    #[serde(default)]
    #[builder(default)]
    pub expectation: MessageExpectation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(default)]
    #[builder(default)]
    pub interrupting: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRelation {
    Supervisor,
    Peer,
    Dependency,
    Dependee,
    System,
}

/// References retain source identity and revision, rather than opaque keys or
/// hashes of message content. Visibility gates are implemented separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageReference {
    ChangeRequest { service: String, scope: String, number: u64, revision: String },
    Commit { repository: ResourceRef, revision: String },
    Ref { repository: ResourceRef, name: String, revision: String },
    Artifact { resource: ResourceRef, revision: String },
    Issue { service: String, scope: String, number: u64, revision: String },
    Comment { service: String, scope: String, id: String, revision: String },
    ControlRecord { resource: ResourceRef, revision: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageExpectation {
    #[default]
    None,
    Reply,
    Outcome {
        condition: Leaf,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagePhase {
    #[default]
    Accepted,
    WaitingOnReferences,
    Deliverable,
    Delivered,
    Satisfied,
    Answered,
    OutcomeMet,
    Expired,
    Superseded,
    DeadLettered,
}

impl MessagePhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Satisfied | Self::Answered | Self::OutcomeMet | Self::Expired | Self::Superseded | Self::DeadLettered)
    }
    pub fn is_waiting(self) -> bool {
        matches!(self, Self::Accepted | Self::WaitingOnReferences | Self::Deliverable)
    }
    pub fn has_delivery_evidence(self) -> bool {
        matches!(self, Self::Delivered | Self::Satisfied | Self::Answered | Self::OutcomeMet)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ResolvedMessageReceiver {
    /// Qualified address of the delivered holder; absent on the previous generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_address: Option<String>,
    pub crew_id: String,
    pub session: String,
    pub delivered_at: DateTime<Utc>,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct MessageStatus {
    pub phase: MessagePhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_sequence: Option<u64>,
    pub since: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// A recovered partial creation remains an audit record but replays its canonical admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_predecessor: Option<ResourceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_receiver: Option<ResolvedMessageReceiver>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<ControllerRetry>,
    /// Persisted before transport I/O; recovery must establish acceptance or
    /// definite non-submission before another attempt can type this batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission: Option<MessageSubmission>,
    /// Members of the last definitely-unsent failure episode, including observation failures before submission.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub failed_batch_members: Vec<String>,
}

impl Default for MessageStatus {
    fn default() -> Self {
        Self {
            phase: MessagePhase::Accepted,
            accepted_sequence: None,
            // An absent status has no evidence timestamp. Writers provide their clock.
            since: DateTime::<Utc>::UNIX_EPOCH,
            reason: Some("waiting for receiver resolution".into()),
            canonical_predecessor: None,
            resolved_receiver: None,
            retry: None,
            submission: None,
            failed_batch_members: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct MessageSubmission {
    pub batch_id: String,
    pub crew_id: String,
    pub session: String,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    #[builder(default)]
    pub members: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageStatusPatch {
    Wait { phase: MessagePhase, reason: String, at: DateTime<Utc> },
    Delivered { receiver: ResolvedMessageReceiver, at: DateTime<Utc> },
    Finish { phase: MessagePhase, reason: String, at: DateTime<Utc> },
    Suppressed { predecessor: ResourceRef, at: DateTime<Utc> },
}

impl StatusPatch<MessageStatus> for MessageStatusPatch {
    fn apply(&self, status: &mut MessageStatus) {
        if status.phase.is_terminal() {
            return;
        }
        let (phase, reason, at) = match self {
            Self::Wait { phase, reason, at } => {
                if status.phase.has_delivery_evidence() {
                    return;
                }
                (*phase, Some(reason.clone()), *at)
            }
            Self::Delivered { receiver, at } => {
                if status.resolved_receiver.is_some() {
                    return;
                }
                status.resolved_receiver = Some(receiver.clone());
                status.retry = None;
                (MessagePhase::Delivered, None, *at)
            }
            Self::Suppressed { predecessor, at } => {
                if status.resolved_receiver.is_some() {
                    return;
                }
                status.canonical_predecessor = Some(predecessor.clone());
                (MessagePhase::Superseded, Some(format!("suppressed by {}", predecessor.name)), *at)
            }
            Self::Finish { phase, reason, at } => (*phase, Some(reason.clone()), *at),
        };
        if status.phase != phase || status.reason != reason {
            status.phase = phase;
            status.reason = reason;
            status.since = at;
        }
    }
}
