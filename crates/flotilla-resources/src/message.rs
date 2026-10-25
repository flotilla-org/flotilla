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
    const QUERYABLE_FIELDS: &'static [&'static str] =
        &["metadata.name", "spec.receiver", "spec.in_reply_to", "status.phase", "status.submission.batch_id"];
    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "messages", kind: "Message" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::HomeBoundRuntime;

    fn validate_spec(_meta: &InputMeta, spec: &Self::Spec) -> Result<(), ResourceError> {
        crate::validate_message_address(&spec.sender)?;
        crate::validate_message_address(&spec.receiver)?;
        if spec.interrupting && spec.relation != MessageRelation::Supervisor {
            return Err(ResourceError::invalid("only supervisor messages may interrupt an active turn"));
        }
        if let Some(condition) = &spec.delivery_condition {
            crate::admit_leaf(condition).map_err(ResourceError::invalid)?;
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
            if current.submission.is_some() && current.resolved_receiver.is_none() && requested.phase == MessagePhase::Expired {
                return Err(ResourceError::invalid("possible input must establish acceptance before automatic expiry"));
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_digest: Option<String>,
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
    /// Original firing condition, checked again before keystroke delivery.
    /// Decode pre-#2927 Messages without it; remove default after one fleet roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_condition: Option<Leaf>,
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

pub use flotilla_protocol::MessageReference;

/// The sender's awaiting-reply record kind of a stall: a Reply-expectation
/// message about the stalled seat (#3574 ruling 2, #3587 layer 7).
pub const STALL_KIND: &str = "stall";

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
    /// Every persisted phase, in lifecycle order, for derived backend predicates.
    pub const ALL: [Self; 10] = [
        Self::Accepted,
        Self::WaitingOnReferences,
        Self::Deliverable,
        Self::Delivered,
        Self::Satisfied,
        Self::Answered,
        Self::OutcomeMet,
        Self::Expired,
        Self::Superseded,
        Self::DeadLettered,
    ];

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
    /// The agent transport that delivered (ADR 0059 §4), such as `keystroke`.
    /// Absent on receipts written before the transport router (#3545).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct MessageStatus {
    /// Explicit operator authorization, pinned to the receiver that was inspected.
    // Compatibility default for pre-#3799 records; removable one roll after #3799.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_delivery: Option<OperatorMessageDelivery>,
    // Compatibility default for pre-#3799 records; removable one roll after #3799.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fence_recheck_at: Option<DateTime<Utc>>,
    /// Preserve possibly-written submissions when an operator authorizes another attempt.
    // Compatibility default for pre-#3799 records; removable one roll after #3799.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub delivery_history: Vec<MessageSubmission>,
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
            operator_delivery: None,
            fence_recheck_at: None,
            delivery_history: Vec::new(),
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
    // Compatibility default for pre-#3799 records; removable one roll after #3799.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_reason: Option<String>,
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
    /// The agent transport the router chose for this batch; polls and
    /// observations go back to it. Absent on submissions recorded before the
    /// transport router (#3545), which poll the session's first offered transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorMessageDelivery {
    pub reason: String,
    pub session: String,
    pub crew_id: String,
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
            Self::Finish { phase, reason, at } => {
                if status.submission.is_some() && status.resolved_receiver.is_none() && *phase == MessagePhase::Expired {
                    return;
                }
                // Terminal disposition revokes any queued operator attempt; the
                // original submission remains available for audit.
                if phase.is_terminal() {
                    status.operator_delivery = None;
                }
                (*phase, Some(reason.clone()), *at)
            }
        };
        if status.phase != phase || status.reason != reason {
            status.phase = phase;
            status.reason = reason;
            status.since = at;
        }
    }
}

impl MessageSpec {
    /// Flotilla's own turn messages (turn rules, nudges, charter notices):
    /// obsolete once an operator or supervisor speaks to the same receiver.
    pub fn is_system_turn(&self) -> bool {
        self.sender.starts_with("system:")
    }

    /// A person or supervising role speaking, never flotilla's own turn rules.
    pub fn is_operator_or_supervisor(&self) -> bool {
        !self.is_system_turn() && (self.relation == MessageRelation::Supervisor || self.sender.starts_with("principal:"))
    }
}

/// A Message batch whose delivery is held for an operator (#3455): its retry
/// disposition is terminal, either because acceptance evidence is ambiguous
/// or because a definitely-unsent batch exhausted its attempts. Later
/// Messages to the same receiver queue behind it until it is cleared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldMessageBatch {
    /// The recorded submission's batch id, or the first member's name when
    /// the batch was definitely unsent and kept no submission.
    pub batch: String,
    pub receiver: String,
    pub members: Vec<String>,
    pub reason: String,
    /// Every member is a flotilla system turn, so an operator or supervisor
    /// message to the same receiver supersedes the whole batch.
    pub system_only: bool,
    /// A previous attempt may already appear in the conversation.
    pub prior_submission: bool,
}

impl HeldMessageBatch {
    /// Whether this Message is a member of a held batch.
    pub fn is_held(message: &crate::ResourceObject<Message>) -> bool {
        message.status.as_ref().is_some_and(|status| {
            !status.phase.is_terminal()
                && status.resolved_receiver.is_none()
                && status
                    .retry
                    .as_ref()
                    .is_some_and(|retry| matches!(retry.disposition, crate::ControllerRetryDisposition::Terminal { .. }))
        })
    }

    /// The held batches among `messages`, one per batch, in input order.
    pub fn collect<'a>(messages: impl IntoIterator<Item = &'a crate::ResourceObject<Message>>) -> Vec<Self> {
        let messages: Vec<_> = messages.into_iter().collect();
        let mut batches: Vec<Self> = Vec::new();
        for message in messages.iter().copied().filter(|message| Self::is_held(message)) {
            if batches.iter().any(|batch| batch.members.contains(&message.metadata.name)) {
                continue;
            }
            let status = message.status.as_ref().expect("held message has status");
            let mut members = status
                .submission
                .as_ref()
                .map(|submission| submission.members.clone())
                .filter(|members| !members.is_empty())
                .unwrap_or_else(|| status.failed_batch_members.clone());
            if !members.contains(&message.metadata.name) {
                members = vec![message.metadata.name.clone()];
            }
            let system_only = members.iter().all(|member| {
                messages.iter().find(|message| message.metadata.name == *member).is_some_and(|message| message.spec.is_system_turn())
            });
            batches.push(Self {
                batch: status.submission.as_ref().map_or_else(|| members[0].clone(), |submission| submission.batch_id.clone()),
                receiver: message.spec.receiver.clone(),
                reason: status.reason.clone().unwrap_or_else(|| "delivery held".into()),
                members,
                system_only,
                prior_submission: status.submission.is_some() || !status.delivery_history.is_empty(),
            });
        }
        batches
    }

    /// The operator command that closes this batch, keeping its audit.
    pub fn clear_command(&self, namespace: &str) -> String {
        format!("flotilla resource fail-message-batch {} --namespace {namespace} --reason '<why>'", self.members[0])
    }

    /// One plain sentence naming the batch, why it is held and how to clear it.
    pub fn describe(&self, namespace: &str) -> String {
        format!(
            "message batch `{}` is held for {} ({} message{}: {}): {}; {}, then authorize same-session delivery with `flotilla resource deliver-message-batch {} --namespace {} --reason '<why>'`; dispose of it with `{}`",
            self.batch,
            self.receiver,
            self.members.len(),
            if self.members.len() == 1 { "" } else { "s" },
            self.members.join(", "),
            self.reason,
            if self.prior_submission { "confirm the message is not already in the conversation and verify the pane shows the agent composer" } else { "verify the pane shows the agent composer first" },
            self.members[0],
            namespace,
            self.clear_command(namespace)
        )
    }
}

/// Receiver-scoped producer identity prevents unrelated convoy queues from
/// colliding when their local producer keys happen to be equal.
pub use flotilla_address::message_record_name;

impl MessageSpec {
    pub fn same_intent(&self, other: &Self) -> bool {
        if self == other {
            return true;
        }
        let mut left = self.clone();
        let mut right = other.clone();
        // Retry an admission stored before #2927 without changing its immutable
        // spec. Delivery recovers the old guard from the workflow receipt.
        // Remove this compatibility comparison after one fleet roll.
        if left.delivery_condition.is_none() || right.delivery_condition.is_none() {
            left.delivery_condition = None;
            right.delivery_condition = None;
        }
        let digest = |spec: &Self| spec.body_digest.clone().unwrap_or_else(|| message_body_digest(&spec.body));
        let same_body = digest(&left) == digest(&right);
        left.body.clear();
        left.body_digest = None;
        right.body.clear();
        right.body_digest = None;
        same_body && left == right
    }
}

pub fn message_body_digest(body: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(body.as_bytes()))
}
