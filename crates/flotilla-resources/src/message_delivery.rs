//! Receiver-side batching and durable transport accounting.
use std::{collections::BTreeMap, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{PrincipalRef, ResourceRef};

use crate::{
    api_version, apply_status_patch, resolve_message_receiver, ControllerRetry, ControllerRetryDisposition, Demand, DemandKind, DemandSpec,
    DemandStatusPatch, InputMeta, Message, MessageExpectation, MessageInbox, MessagePhase, MessageStatus, MessageStatusPatch,
    MessageSubmission, OwnerReference, ResolvedMessageReceiver, Resource, ResourceError, ResourceObject, ResourceProvenance, RetryBackoff,
    StatusPatch, TerminalSession, TerminalSessionPhase,
};

const MAX_ATTEMPTS: u32 = 3;
const HOLD_BOUND: chrono::Duration = chrono::Duration::minutes(5);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageObservation {
    pub ready: bool,
    pub working: bool,
    pub evidence: Option<String>,
    pub output_digest: Option<String>,
    pub waiting_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageTransportOutcome {
    Pending,
    Accepted { evidence: String },
    NotSubmitted { reason: String },
    Unconfirmed { reason: String },
}

#[derive(Debug, Clone, bon::Builder)]
pub struct MessageBatch {
    pub id: String,
    pub holder: ResourceObject<TerminalSession>,
    pub submission: MessageSubmission,
    pub text: String,
}

/// The transport owns readiness and evidence. Polling an existing submission
/// must never type its text again, even after adapter or daemon restart.
#[async_trait]
pub trait MessageTransport: Send + Sync {
    async fn observe(
        &self,
        holder: &ResourceObject<TerminalSession>,
        submission: Option<&MessageSubmission>,
    ) -> Result<MessageObservation, String>;
    async fn submit(&self, batch: &MessageBatch) -> MessageTransportOutcome;
    async fn poll(&self, batch: &MessageBatch) -> MessageTransportOutcome;
    /// Forget exactly this batch's transport bookkeeping after durable receipt.
    /// This must not type input or modify the terminal's composer.
    async fn release(&self, _batch: &MessageBatch) {}
}

impl MessageInbox {
    /// A pass always observes holders, including held submissions. Admission and
    /// delivery share the inbox lock so a successor cannot race batch selection.
    pub async fn reconcile_delivery(&self, transport: &dyn MessageTransport, now: DateTime<Utc>) -> Result<(), ResourceError> {
        let _guard = self.admission.lock().await;
        // A created record with no status is an interrupted admission. Finish
        // its predecessor cleanup before selecting any batch for transport.
        for message in self.messages.list().await?.items.into_iter().filter(|message| message.status.is_none()) {
            self.accept_locked(&crate::InputMeta::from(&message.metadata), &message.spec, now).await?;
        }
        let replies = self.backend.including_replicas::<Message>(&self.namespace).list().await?.items;
        let mut messages = self.messages.list().await?.items;
        messages.sort_by_key(|message| {
            (
                message.metadata.creation_timestamp,
                message.status.as_ref().and_then(|status| status.accepted_sequence).unwrap_or(u64::MAX),
                message.metadata.name.clone(),
            )
        });
        let mut groups: BTreeMap<String, (ResourceObject<TerminalSession>, Vec<ResourceObject<Message>>)> = BTreeMap::new();
        // Reuse a receiver lookup within this pass, never across passes: holder
        // evidence must refresh even while a submission is held.
        let mut receivers: BTreeMap<String, Option<crate::ReadResourceObject<TerminalSession>>> = BTreeMap::new();
        for message in &messages {
            let phase = message.status.as_ref().map_or(MessagePhase::Accepted, |status| status.phase);
            if phase.is_terminal() {
                self.clear_signal(message).await?;
                continue;
            }
            if phase == MessagePhase::Delivered && message.spec.expectation == MessageExpectation::None {
                apply_status_patch(&self.messages, &message.metadata.name, &MessageStatusPatch::Finish {
                    phase: MessagePhase::Satisfied,
                    reason: "notification was accepted by its receiver".into(),
                    at: now,
                })
                .await?;
                continue;
            }
            if message.spec.deadline.is_some_and(|deadline| now >= deadline)
                && message.status.as_ref().is_none_or(|status| status.submission.is_none() || status.resolved_receiver.is_some())
            {
                apply_status_patch(&self.messages, &message.metadata.name, &MessageStatusPatch::Finish {
                    phase: MessagePhase::Expired,
                    reason: "message deadline elapsed".into(),
                    at: now,
                })
                .await?;
                continue;
            }
            if phase == MessagePhase::Delivered {
                if let MessageExpectation::Outcome { condition } = &message.spec.expectation {
                    if self.outcome_met(condition, now).await? {
                        apply_status_patch(&self.messages, &message.metadata.name, &MessageStatusPatch::Finish {
                            phase: MessagePhase::OutcomeMet,
                            reason: "outcome condition holds".into(),
                            at: now,
                        })
                        .await?;
                    }
                }
                if message.spec.expectation == MessageExpectation::Reply
                    && replies.iter().any(|reply| {
                        reply.object.spec.in_reply_to.as_deref() == Some(message.metadata.name.as_str())
                            && reply.object.spec.sender == message.spec.receiver
                            && reply.object.spec.receiver == message.spec.sender
                    })
                {
                    apply_status_patch(&self.messages, &message.metadata.name, &MessageStatusPatch::Finish {
                        phase: MessagePhase::Answered,
                        reason: "receiver published a correlated reply".into(),
                        at: now,
                    })
                    .await?;
                }
                continue;
            }
            if message.status.as_ref().is_none_or(|status| status.submission.is_none())
                && messages.iter().any(|predecessor| {
                    predecessor.metadata.name != message.metadata.name
                        && crate::message_expectation_open(predecessor)
                        && crate::message_supersedes(&message.spec, predecessor)
                })
            {
                apply_status_patch(&self.messages, &message.metadata.name, &MessageStatusPatch::Finish {
                    phase: MessagePhase::Superseded,
                    reason: "same-subject predecessor was accepted with an open expectation".into(),
                    at: now,
                })
                .await?;
                continue;
            }
            if phase == MessagePhase::WaitingOnReferences {
                continue;
            }
            let holder = if let Some(holder) = receivers.get(&message.spec.receiver) {
                holder.clone()
            } else {
                let holder = resolve_message_receiver(&self.backend, &self.namespace, &message.spec.receiver).await?;
                receivers.insert(message.spec.receiver.clone(), holder.clone());
                holder
            };
            match holder {
                Some(holder) if matches!(holder.provenance, ResourceProvenance::Local) => {
                    groups
                        .entry(holder.object.metadata.name.clone())
                        .or_insert_with(|| (holder.object, Vec::new()))
                        .1
                        .push(message.clone());
                }
                Some(_) => self.wait(message, MessagePhase::Accepted, "receiver holder is not at this message's home", now).await?,
                None => self.wait(message, MessagePhase::Accepted, "receiver role has no current holder", now).await?,
            }
        }
        for (_, (holder, pending)) in groups {
            self.deliver_group(transport, &holder, &pending, &messages, now).await?;
        }
        Ok(())
    }

    async fn deliver_group(
        &self,
        transport: &dyn MessageTransport,
        holder: &ResourceObject<TerminalSession>,
        pending: &[ResourceObject<Message>],
        all: &[ResourceObject<Message>],
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        let existing = pending.iter().find_map(|message| message.status.as_ref().and_then(|status| status.submission.clone()));
        if existing.is_none()
            && holder
                .status
                .as_ref()
                .is_none_or(|status| status.phase != TerminalSessionPhase::Running || status.crew.is_none() || status.session_id.is_none())
        {
            for message in pending {
                self.wait(message, MessagePhase::Deliverable, "waiting for holder terminal startup", now).await?;
            }
            return Ok(());
        }
        let observation = match transport.observe(holder, existing.as_ref()).await {
            Ok(observation) => observation,
            Err(error) => {
                if let Some(submission) = &existing {
                    if now.signed_duration_since(submission.started_at) >= HOLD_BOUND {
                        self.failed(pending, holder, &error, true, now).await?;
                        return Ok(());
                    }
                    for message in pending {
                        self.wait(message, MessagePhase::Deliverable, &format!("held while observing acceptance: {error}"), now).await?;
                    }
                } else {
                    self.failed(pending, holder, &error, false, now).await?;
                }
                return Ok(());
            }
        };
        if let Some(mut submission) = existing {
            for message in pending.iter().filter(|message| !submission.members.contains(&message.metadata.name)) {
                self.wait(message, MessagePhase::Deliverable, &format!("waiting behind recorded batch {}", submission.batch_id), now)
                    .await?;
            }
            // Reconstruct exactly the recorded batch, not today's enlarged inbox.
            let members: Vec<_> = submission
                .members
                .iter()
                .map(|name| {
                    all.iter()
                        .find(|message| message.metadata.name == *name)
                        .cloned()
                        .ok_or_else(|| ResourceError::invalid(format!("recorded message batch member `{name}` is absent")))
                })
                .collect::<Result<_, _>>()?;
            if let Some(receiver) =
                members.iter().find_map(|message| message.status.as_ref().and_then(|status| status.resolved_receiver.clone()))
            {
                // Recover a partially persisted batch receipt without polling or resubmitting.
                self.accepted(&members, &receiver).await?;
                transport.release(&batch(holder, &members, submission.clone())).await;
                self.clear_signal(&members[0]).await?;
                return Ok(());
            }
            let receiver = holder.status.as_ref().and_then(|status| status.crew.as_ref()).map(|crew| crew.id.as_str());
            let same_holder = holder.status.as_ref().and_then(|status| status.session_id.as_deref()) == Some(submission.session.as_str())
                && receiver == Some(submission.crew_id.as_str());
            let evidence = observation.evidence.clone().or_else(|| {
                if same_holder && observation.working {
                    let since = submission.working_since.get_or_insert(now);
                    (now.signed_duration_since(*since) >= chrono::Duration::seconds(1))
                        .then(|| "holder remained Working after input submission".into())
                } else {
                    submission.working_since = None;
                    None
                }
            });
            if same_holder {
                if let Some(evidence) = evidence {
                    self.accepted(
                        &members,
                        &ResolvedMessageReceiver::builder()
                            .crew_id(submission.crew_id.clone())
                            .session(submission.session.clone())
                            .delivered_at(now)
                            .evidence(evidence)
                            .build(),
                    )
                    .await?;
                    transport.release(&batch(holder, &members, submission.clone())).await;
                    self.clear_signal(&members[0]).await?;
                    return Ok(());
                }
            }
            for message in &members {
                if message.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
                    continue;
                }
                let mut status = status_for(message);
                status.submission = Some(submission.clone());
                self.write(message, &status).await?;
            }
            if !same_holder {
                self.failed(&members, holder, "recorded receiver session changed before acceptance was established", true, now).await?;
                return Ok(());
            }
            let batch = batch(holder, &members, submission.clone());
            let outcome = transport.poll(&batch).await;
            if matches!(outcome, MessageTransportOutcome::Pending | MessageTransportOutcome::Unconfirmed { .. })
                && now.signed_duration_since(submission.started_at) >= HOLD_BOUND
            {
                self.failed(&members, holder, "submission acceptance remained unresolved past the hold bound", true, now).await?;
            } else {
                self.outcome(&members, holder, &submission, transport, outcome, now).await?;
            }
            return Ok(());
        }
        if let Some(held) = pending.iter().find(|message| {
            message
                .status
                .as_ref()
                .and_then(|status| status.retry.as_ref())
                .is_some_and(|retry| matches!(retry.disposition, ControllerRetryDisposition::Terminal { .. }))
        }) {
            self.signal(held, now).await?;
        }
        if pending.iter().any(|message| {
            message.status.as_ref().and_then(|status| status.retry.as_ref()).is_some_and(|retry| match &retry.disposition {
                ControllerRetryDisposition::Retryable { next_attempt_at } => now < *next_attempt_at,
                ControllerRetryDisposition::Terminal { .. } => true,
            })
        }) {
            return Ok(());
        }
        if holder
            .status
            .as_ref()
            .is_none_or(|status| status.phase != TerminalSessionPhase::Running || status.crew.is_none() || status.session_id.is_none())
            || !observation.ready
        {
            let reason = observation.waiting_reason.as_deref().unwrap_or("waiting for holder terminal readiness and a turn boundary");
            for message in pending {
                self.wait(message, MessagePhase::Deliverable, reason, now).await?;
            }
            return Ok(());
        }
        let status = holder.status.as_ref().expect("running holder");
        let submission = MessageSubmission::builder()
            .batch_id(format!(
                "batch-{}-{}",
                pending[0].metadata.name,
                pending[0].status.as_ref().and_then(|status| status.retry.as_ref()).map_or(0, |retry| retry.attempts)
            ))
            .crew_id(status.crew.as_ref().expect("holder crew").id.clone())
            .session(status.session_id.clone().expect("holder session"))
            .started_at(now)
            .members(pending.iter().map(|message| message.metadata.name.clone()).collect())
            .maybe_output_digest(observation.output_digest)
            .build();
        // Persist intent before any transport I/O. A crash can hold delivery, but
        // can never turn an ambiguous accepted batch into a fresh submission.
        for message in pending {
            let mut status = status_for(message);
            MessageStatusPatch::Wait { phase: MessagePhase::Deliverable, reason: "awaiting transport acceptance evidence".into(), at: now }
                .apply(&mut status);
            status.submission = Some(submission.clone());
            self.write(message, &status).await?;
        }
        let batch = batch(holder, pending, submission.clone());
        self.outcome(pending, holder, &submission, transport, transport.submit(&batch).await, now).await
    }

    async fn outcome(
        &self,
        members: &[ResourceObject<Message>],
        holder: &ResourceObject<TerminalSession>,
        submission: &MessageSubmission,
        transport: &dyn MessageTransport,
        outcome: MessageTransportOutcome,
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        match outcome {
            MessageTransportOutcome::Accepted { evidence } if !evidence.is_empty() => {
                self.accepted(
                    members,
                    &ResolvedMessageReceiver::builder()
                        .crew_id(submission.crew_id.clone())
                        .session(submission.session.clone())
                        .delivered_at(now)
                        .evidence(evidence)
                        .build(),
                )
                .await?;
                transport.release(&batch(holder, members, submission.clone())).await;
                self.clear_signal(&members[0]).await
            }
            MessageTransportOutcome::NotSubmitted { reason } => self.failed(members, holder, &reason, false, now).await,
            MessageTransportOutcome::Unconfirmed { reason } => {
                self.failed(members, holder, &format!("held pending acceptance evidence: {reason}"), true, now).await
            }
            MessageTransportOutcome::Pending => Ok(()),
            MessageTransportOutcome::Accepted { .. } => {
                self.failed(members, holder, "transport supplied an empty acceptance receipt", true, now).await
            }
        }
    }

    async fn accepted(&self, members: &[ResourceObject<Message>], receiver: &ResolvedMessageReceiver) -> Result<(), ResourceError> {
        for member in members {
            apply_status_patch(&self.messages, &member.metadata.name, &MessageStatusPatch::Delivered {
                receiver: receiver.clone(),
                at: receiver.delivered_at,
            })
            .await?;
            self.clear_signal(member).await?;
        }
        Ok(())
    }

    async fn failed(
        &self,
        members: &[ResourceObject<Message>],
        _holder: &ResourceObject<TerminalSession>,
        reason: &str,
        ambiguous: bool,
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        for member in members {
            let current = self.messages.get(&member.metadata.name).await?;
            let mut status = status_for(&current);
            if status.retry.as_ref().is_some_and(|retry| matches!(retry.disposition, ControllerRetryDisposition::Terminal { .. })) {
                continue;
            }
            let mut retry = ControllerRetry::retryable(status.retry.as_ref(), now, RetryBackoff {
                initial: Duration::from_secs(5),
                maximum: Duration::from_secs(60),
            });
            let held = ambiguous || retry.attempts >= MAX_ATTEMPTS;
            if held {
                retry.disposition = ControllerRetryDisposition::Terminal { needs: reason.into() };
            }
            if !ambiguous {
                status.submission = None;
            }
            MessageStatusPatch::Wait {
                phase: MessagePhase::Deliverable,
                reason: format!(
                    "{}: {reason}; attempt {}/{}",
                    if held { "delivery held" } else { "retrying delivery" },
                    retry.attempts,
                    MAX_ATTEMPTS
                ),
                at: now,
            }
            .apply(&mut status);
            status.retry = Some(retry);
            self.write(&current, &status).await?;
            if held && member.metadata.name == members[0].metadata.name {
                self.signal(&current, now).await?;
            }
        }
        Ok(())
    }

    async fn wait(
        &self,
        message: &ResourceObject<Message>,
        phase: MessagePhase,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        let current = self.messages.get(&message.metadata.name).await?;
        let mut status = status_for(&current);
        MessageStatusPatch::Wait { phase, reason: reason.into(), at: now }.apply(&mut status);
        self.write(&current, &status).await
    }

    async fn write(&self, message: &ResourceObject<Message>, status: &MessageStatus) -> Result<(), ResourceError> {
        let current = self.messages.get(&message.metadata.name).await?;
        if current.status.as_ref() != Some(status) {
            self.messages.update_status(&message.metadata.name, &current.metadata.resource_version, status).await?;
        }
        Ok(())
    }

    async fn signal(&self, message: &ResourceObject<Message>, now: DateTime<Utc>) -> Result<(), ResourceError> {
        let demands = self.backend.using::<Demand>(&self.namespace);
        let name = format!("message-attention-{}", message.metadata.name);
        match demands.get(&name).await {
            Ok(_) => return Ok(()),
            Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
        let resource = ResourceRef::new(api_version(Message::API_PATHS), "Message", &self.namespace, &message.metadata.name);
        demands
            .create(
                &InputMeta::builder()
                    .name(name.clone())
                    .owner_references(vec![OwnerReference {
                        api_version: api_version(Message::API_PATHS),
                        kind: "Message".into(),
                        name: message.metadata.name.clone(),
                        controller: true,
                    }])
                    .build(),
                &DemandSpec::for_dispatching_principal(
                    resource,
                    DemandKind::HumanGate,
                    PrincipalRef::implicit_for_namespace(&self.namespace),
                ),
            )
            .await?;
        apply_status_patch(&demands, &name, &DemandStatusPatch::Raise { as_of: now, authority: "message-delivery".into() }).await?;
        Ok(())
    }
    async fn clear_signal(&self, message: &ResourceObject<Message>) -> Result<(), ResourceError> {
        let demands = self.backend.using::<Demand>(&self.namespace);
        let name = format!("message-attention-{}", message.metadata.name);
        match demands.delete(&name).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn status_for(message: &ResourceObject<Message>) -> MessageStatus {
    message.status.clone().unwrap_or_else(|| {
        MessageStatus::builder()
            .phase(MessagePhase::Accepted)
            .since(message.metadata.creation_timestamp)
            .reason("waiting for receiver resolution".into())
            .build()
    })
}

fn batch(holder: &ResourceObject<TerminalSession>, members: &[ResourceObject<Message>], submission: MessageSubmission) -> MessageBatch {
    let text = members
        .iter()
        .map(|message| {
            let relation = match message.spec.relation {
                crate::MessageRelation::Supervisor => "supervisor",
                crate::MessageRelation::Peer => "peer",
                crate::MessageRelation::Dependency => "dependency",
                crate::MessageRelation::Dependee => "dependee",
                crate::MessageRelation::System => "system",
            };
            let subject = message
                .spec
                .subject
                .as_ref()
                .map(|subject| serde_json::to_string(subject).expect("typed reference serializes"))
                .unwrap_or_else(|| "none".into());
            let expectation = match &message.spec.expectation {
                MessageExpectation::None => "none",
                MessageExpectation::Reply => "reply",
                MessageExpectation::Outcome { .. } => "outcome",
            };
            format!(
                "[{} · relation: {relation} · subject: {subject} · expectation: {expectation} · message: {}]\n\n{}",
                message.spec.sender, message.metadata.name, message.spec.body
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    MessageBatch::builder().id(submission.batch_id.clone()).holder(holder.clone()).submission(submission).text(text).build()
}
