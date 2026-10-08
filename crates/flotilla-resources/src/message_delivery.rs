//! Receiver-side batching and durable transport accounting.
use std::{collections::BTreeMap, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::{
    apply_status_patch,
    delivery_hold::{
        delivery_acceptance_evidence, delivery_demand, delivery_demand_name, delivery_retry_backoff, delivery_retry_exhausted,
        DELIVERY_HOLD_FOR, DELIVERY_MAX_ATTEMPTS,
    },
    message_expectation_open, message_supersedes, resolve_message_receiver, ControllerRetry, ControllerRetryDisposition, Demand,
    DemandStatusPatch, InputMeta, Message, MessageExpectation, MessageInbox, MessagePhase, MessageQuery, MessageReference, MessageRelation,
    MessageSpec, MessageStatus, MessageStatusPatch, MessageSubmission, ReadResourceObject, ResolvedMessageReceiver, ResourceError,
    ResourceObject, ResourceProvenance, StatusPatch, TerminalSession, TerminalSessionPhase,
};

// Bound adapter calls independently of admission.
const TRANSPORT_CALL_BOUND: Duration = Duration::from_secs(30);

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
    /// Drop bookkeeping for explicitly closed batches, without terminal input.
    /// Partial closure must leave an unresolved batch intact.
    async fn release_closed(&self, _messages: &[ResourceObject<Message>]) {}
}

impl MessageInbox {
    /// A pass always observes holders, including held submissions. Admission and
    /// delivery serialize separately; admission never waits on transport I/O.
    pub async fn reconcile_delivery(&self, transport: &dyn MessageTransport, now: DateTime<Utc>) -> Result<(), ResourceError> {
        let _delivery = self.delivery.lock().await;
        {
            let _admission = self.admission.lock().await;
            // A created record with no status is an interrupted admission. Finish
            // its predecessor cleanup before selecting any batch for transport.
            for message in self.messages.query(&MessageQuery::active()).await?.into_iter().filter(|message| message.status.is_none()) {
                self.accept_locked(&InputMeta::from(&message.metadata), &message.spec, now).await?;
            }
        }
        let mut messages = self.messages.query(&MessageQuery::active()).await?;
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
        let mut receivers: BTreeMap<(String, String), Option<ReadResourceObject<TerminalSession>>> = BTreeMap::new();
        for message in &messages {
            let phase = message.status.as_ref().map_or(MessagePhase::Accepted, |status| status.phase);
            if phase.is_terminal() {
                continue;
            }
            if phase == MessagePhase::Delivered && message.spec.expectation == MessageExpectation::None {
                apply_status_patch(
                    &self.messages,
                    &message.metadata.name,
                    &MessageStatusPatch::Finish {
                        phase: MessagePhase::Satisfied,
                        reason: "notification was accepted by its receiver".into(),
                        at: now,
                    },
                )
                .await?;
                continue;
            }
            if message.spec.deadline.is_some_and(|deadline| now >= deadline)
                && message.status.as_ref().is_none_or(|status| status.submission.is_none() || status.resolved_receiver.is_some())
            {
                apply_status_patch(
                    &self.messages,
                    &message.metadata.name,
                    &MessageStatusPatch::Finish { phase: MessagePhase::Expired, reason: "message deadline elapsed".into(), at: now },
                )
                .await?;
                continue;
            }
            if phase == MessagePhase::Delivered {
                if let MessageExpectation::Outcome { condition } = &message.spec.expectation {
                    if self.outcome_met(condition, now).await? {
                        apply_status_patch(
                            &self.messages,
                            &message.metadata.name,
                            &MessageStatusPatch::Finish {
                                phase: MessagePhase::OutcomeMet,
                                reason: "outcome condition holds".into(),
                                at: now,
                            },
                        )
                        .await?;
                    }
                }
                let mut answered = false;
                if message.spec.expectation == MessageExpectation::Reply {
                    for reply in
                        self.backend.query_messages(&self.namespace, &MessageQuery::ReplyTo { name: message.metadata.name.clone() }).await?
                    {
                        if reply.object.spec.in_reply_to.as_deref() == Some(message.metadata.name.as_str())
                            && reply.object.spec.receiver == message.spec.sender
                            && self.reply_sender_matches(message, &reply.object.spec.sender).await?
                        {
                            answered = true;
                            break;
                        }
                    }
                }
                if answered {
                    apply_status_patch(
                        &self.messages,
                        &message.metadata.name,
                        &MessageStatusPatch::Finish {
                            phase: MessagePhase::Answered,
                            reason: "receiver published a correlated reply".into(),
                            at: now,
                        },
                    )
                    .await?;
                }
                continue;
            }
            if message.status.as_ref().is_none_or(|status| status.submission.is_none())
                && messages.iter().any(|predecessor| {
                    predecessor.metadata.name != message.metadata.name
                        && message_expectation_open(predecessor)
                        && message.spec.supersedes.is_none()
                        && message_supersedes(&message.spec, predecessor)
                })
            {
                apply_status_patch(
                    &self.messages,
                    &message.metadata.name,
                    &MessageStatusPatch::Finish {
                        phase: MessagePhase::Superseded,
                        reason: "same-subject predecessor was accepted with an open expectation".into(),
                        at: now,
                    },
                )
                .await?;
                continue;
            }
            if phase == MessagePhase::WaitingOnReferences {
                continue;
            }
            let receiver_key = (message.spec.receiver.clone(), message.spec.sender.clone());
            let holder = if let Some(holder) = receivers.get(&receiver_key) {
                holder.clone()
            } else {
                let holder = self.resolve_receiver(message).await?;
                receivers.insert(receiver_key, holder.clone());
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
        let member_names: std::collections::BTreeSet<_> = messages
            .iter()
            .flat_map(|message| {
                message
                    .status
                    .as_ref()
                    .and_then(|status| status.submission.as_ref())
                    .into_iter()
                    .flat_map(|submission| submission.members.iter().cloned())
            })
            .collect();
        for name in member_names {
            if !messages.iter().any(|message| message.metadata.name == name) {
                messages.push(self.messages.get(&name).await?);
            }
        }
        for (_, (holder, pending)) in groups {
            self.deliver_group(transport, &holder, &pending, &messages, now).await?;
        }
        self.cleanup_delivery_gates(now).await?;
        let current = self.messages.query(&MessageQuery::active()).await?;
        tokio::time::timeout(TRANSPORT_CALL_BOUND, transport.release_closed(&current)).await.ok();
        Ok(())
    }

    /// Explicit operator closure is distinct from a receipt or an automatic
    /// timeout. Preserve the original submission so the audit cannot imply a
    /// definitely-unsent write; the operator accepts the uncertainty.
    pub async fn fail_batch(&self, name: &str, reason: &str, now: DateTime<Utc>) -> Result<(), ResourceError> {
        if reason.trim().is_empty() {
            return Err(ResourceError::invalid("operator batch failure requires a reason"));
        }
        let _admission = self.admission.lock().await;
        let message = self.messages.get(name).await?;
        let status = message.status.as_ref().ok_or_else(|| ResourceError::invalid("Message has not been admitted"))?;
        if status.phase.is_terminal() && !status.reason.as_deref().is_some_and(|reason| reason.starts_with("operator failed batch:")) {
            return Ok(());
        }
        let exhausted = status.retry.as_ref().is_some_and(|retry| matches!(retry.disposition, ControllerRetryDisposition::Terminal { .. }));
        if status.submission.is_none() && !exhausted {
            return Err(ResourceError::invalid("Message batch is neither held nor exhausted"));
        }
        let members = status.submission.as_ref().map(|submission| submission.members.clone()).unwrap_or_else(|| {
            if status.failed_batch_members.is_empty() {
                vec![name.to_string()]
            } else {
                status.failed_batch_members.clone()
            }
        });
        let members = if members.is_empty() { vec![name.to_string()] } else { members };
        for member in members {
            apply_status_patch(
                &self.messages,
                &member,
                &MessageStatusPatch::Finish {
                    phase: MessagePhase::DeadLettered,
                    reason: format!("operator failed batch: {reason}"),
                    at: now,
                },
            )
            .await?;
        }
        self.clear_signal(&message).await
    }

    async fn delivered_role_address(
        &self,
        message: &ResourceObject<Message>,
        holder: &ResourceObject<TerminalSession>,
    ) -> Result<Option<String>, ResourceError> {
        if let Some(address) = holder.metadata.annotations.get(crate::ROLE_ADDRESS_ANNOTATION) {
            return Ok(Some(address.clone()));
        }
        let labels = &holder.metadata.labels;
        let (Some(convoy), Some(vessel), Some(role)) =
            (labels.get(crate::CONVOY_LABEL), labels.get(crate::VESSEL_LABEL), labels.get(crate::ROLE_LABEL))
        else {
            return Ok(None);
        };
        let project = if message.spec.receiver.starts_with("topic:") {
            self.backend.including_replicas::<crate::Convoy>(&self.namespace).get(convoy).await?.object.spec.project_ref
        } else {
            message.spec.receiver.split('/').next().map(str::to_string)
        };
        Ok(project.map(|project| format!("{project}/{convoy}/{vessel}/{role}")))
    }

    async fn resolve_receiver(
        &self,
        message: &ResourceObject<Message>,
    ) -> Result<Option<ReadResourceObject<TerminalSession>>, ResourceError> {
        if message.spec.receiver.starts_with("topic:") {
            crate::resolve_topic_receiver(&self.backend, &self.namespace, &message.spec.receiver, &message.spec.sender).await
        } else {
            resolve_message_receiver(&self.backend, &self.namespace, &message.spec.receiver).await
        }
    }

    async fn reply_sender_matches(&self, message: &ResourceObject<Message>, sender: &str) -> Result<bool, ResourceError> {
        if sender == message.spec.receiver {
            return Ok(true);
        }
        if message.spec.receiver.starts_with("topic:") {
            return Ok(message
                .status
                .as_ref()
                .and_then(|status| status.resolved_receiver.as_ref())
                .and_then(|receiver| receiver.role_address.as_deref())
                == Some(sender));
        }
        let request: Vec<_> = message.spec.receiver.split('/').collect();
        let reply: Vec<_> = sender.split('/').collect();
        if !matches!((&request[..], &reply[..]), ([project, _], [reply_project, _, _, _]) if project == reply_project) {
            return Ok(false);
        }
        let Some(receiver) = message.status.as_ref().and_then(|status| status.resolved_receiver.as_ref()) else {
            return Ok(false);
        };
        if let Some(address) = &receiver.role_address {
            return Ok(address == sender);
        }
        // N→N+1 fallback: find the recorded incarnation directly, without
        // requiring its old convoy to remain the current project-role holder.
        let holders = self.backend.including_replicas::<TerminalSession>(&self.namespace).list().await?.items;
        Ok(holders.iter().any(|holder| {
            let labels = &holder.object.metadata.labels;
            holder.object.status.as_ref().is_some_and(|status| {
                status.crew.as_ref().is_some_and(|crew| crew.id == receiver.crew_id)
                    && status.session_id.as_deref() == Some(receiver.session.as_str())
            }) && labels.get(crate::CONVOY_LABEL).is_some_and(|value| value == reply[1])
                && labels.get(crate::VESSEL_LABEL).is_some_and(|value| value == reply[2])
                && labels.get(crate::ROLE_LABEL).is_some_and(|value| value == reply[3])
        }))
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
        let running_holder = holder
            .status
            .as_ref()
            .filter(|status| status.phase == TerminalSessionPhase::Running)
            .and_then(|status| Some((status.crew.as_ref()?, status.session_id.as_ref()?)));
        if existing.is_none() && running_holder.is_none() {
            for message in pending {
                self.wait(message, MessagePhase::Deliverable, "waiting for holder terminal startup", now).await?;
            }
            return Ok(());
        }
        let observation = match tokio::time::timeout(TRANSPORT_CALL_BOUND, transport.observe(holder, existing.as_ref()))
            .await
            .unwrap_or_else(|_| Err("transport observation timed out".into()))
        {
            Ok(observation) => observation,
            Err(error) => {
                if let Some(submission) = &existing {
                    let held: Vec<_> =
                        pending.iter().filter(|message| submission.members.contains(&message.metadata.name)).cloned().collect();
                    if now.signed_duration_since(submission.started_at) >= DELIVERY_HOLD_FOR {
                        self.failed(&held, &error, true, now).await?;
                        return Ok(());
                    }
                    for message in pending {
                        self.wait(message, MessagePhase::Deliverable, &format!("held while observing acceptance: {error}"), now).await?;
                    }
                } else {
                    self.failed(pending, &error, false, now).await?;
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
                release(transport, &batch(holder, &members, submission.clone())).await;
                self.clear_signal(&members[0]).await?;
                return Ok(());
            }
            let receiver = holder.status.as_ref().and_then(|status| status.crew.as_ref()).map(|crew| crew.id.as_str());
            let same_holder = holder.status.as_ref().and_then(|status| status.session_id.as_deref()) == Some(submission.session.as_str())
                && receiver == Some(submission.crew_id.as_str());
            let evidence = observation.evidence.clone().or_else(|| {
                if same_holder && observation.working {
                    // Use the same Working debounce as legacy held inputs.
                    let since = submission.working_since.get_or_insert(now);
                    delivery_acceptance_evidence(
                        false,
                        false,
                        true,
                        now.signed_duration_since(*since) >= crate::delivery_hold::DELIVERY_WORKING_DEBOUNCE_FOR,
                        observation
                            .output_digest
                            .as_ref()
                            .is_some_and(|digest| submission.output_digest.as_ref().is_some_and(|previous| previous != digest)),
                    )
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
                            .maybe_role_address(self.delivered_role_address(&members[0], holder).await?)
                            .crew_id(submission.crew_id.clone())
                            .session(submission.session.clone())
                            .delivered_at(now)
                            .evidence(evidence)
                            .build(),
                    )
                    .await?;
                    release(transport, &batch(holder, &members, submission.clone())).await;
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
                self.failed(&members, "recorded receiver session changed before acceptance was established", true, now).await?;
                return Ok(());
            }
            let batch = batch(holder, &members, submission.clone());
            let outcome = tokio::time::timeout(TRANSPORT_CALL_BOUND, transport.poll(&batch))
                .await
                .unwrap_or_else(|_| MessageTransportOutcome::Unconfirmed { reason: "transport receipt poll timed out".into() });
            if matches!(outcome, MessageTransportOutcome::Pending | MessageTransportOutcome::Unconfirmed { .. })
                && now.signed_duration_since(submission.started_at) >= DELIVERY_HOLD_FOR
            {
                self.failed(&members, "submission acceptance remained unresolved past the hold bound", true, now).await?;
            } else {
                self.outcome(&members, holder, &submission, transport, outcome, now).await?;
            }
            return Ok(());
        }
        // Preserve FIFO after a definitely-unsent batch exhausts its budget.
        // A later message must not silently bypass the operator's held intent.
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
        if running_holder.is_none() || !observation.ready {
            let reason = observation.waiting_reason.as_deref().unwrap_or("waiting for holder terminal readiness and a turn boundary");
            for message in pending {
                self.wait(message, MessagePhase::Deliverable, reason, now).await?;
            }
            return Ok(());
        }
        let Some((crew, session)) = running_holder else {
            return Ok(());
        };
        // The key is stable for every possibly submitted attempt. Membership
        // can change only after definitely-unsent evidence clears that attempt.
        let submission = MessageSubmission::builder()
            .batch_id(format!(
                "batch-{}-{}",
                pending[0].metadata.name,
                pending[0].status.as_ref().and_then(|status| status.retry.as_ref()).map_or(0, |retry| retry.attempts)
            ))
            .crew_id(crew.id.clone())
            .session(session.clone())
            .started_at(now)
            .members(pending.iter().map(|message| message.metadata.name.clone()).collect())
            .maybe_output_digest(observation.output_digest)
            .build();
        let admission = self.admission.lock().await;
        // The observation was outside admission. A successor or an operator
        // may have closed a candidate meanwhile; never submit stale intent.
        let mut current_pending = Vec::new();
        for message in pending {
            let current = self.messages.get(&message.metadata.name).await?;
            if current.status.as_ref().is_some_and(|status| status.phase.is_terminal() || status.resolved_receiver.is_some()) {
                continue;
            }
            current_pending.push(current);
        }
        if current_pending.len() != pending.len() {
            return Ok(());
        }
        // Persist intent before any transport I/O. A crash can hold delivery, but
        // can never turn an ambiguous accepted batch into a fresh submission.
        for message in pending {
            let mut status = status_for(message);
            MessageStatusPatch::Wait { phase: MessagePhase::Deliverable, reason: "awaiting transport acceptance evidence".into(), at: now }
                .apply(&mut status);
            status.submission = Some(submission.clone());
            self.write(message, &status).await?;
        }
        drop(admission);
        let batch = batch(holder, pending, submission.clone());
        let outcome = tokio::time::timeout(TRANSPORT_CALL_BOUND, transport.submit(&batch)).await.unwrap_or_else(|_| {
            MessageTransportOutcome::Unconfirmed { reason: "transport submission timed out; input may have been accepted".into() }
        });
        self.outcome(pending, holder, &submission, transport, outcome, now).await
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
                        .maybe_role_address(self.delivered_role_address(&members[0], holder).await?)
                        .crew_id(submission.crew_id.clone())
                        .session(submission.session.clone())
                        .delivered_at(now)
                        .evidence(evidence)
                        .build(),
                )
                .await?;
                release(transport, &batch(holder, members, submission.clone())).await;
                self.clear_signal(&members[0]).await
            }
            MessageTransportOutcome::NotSubmitted { reason } => self.failed(members, &reason, false, now).await,
            MessageTransportOutcome::Unconfirmed { reason } => {
                self.failed(members, &format!("held pending acceptance evidence: {reason}"), true, now).await
            }
            MessageTransportOutcome::Pending => Ok(()),
            MessageTransportOutcome::Accepted { .. } => {
                self.failed(members, "transport supplied an empty acceptance receipt", true, now).await
            }
        }
    }

    async fn accepted(&self, members: &[ResourceObject<Message>], receiver: &ResolvedMessageReceiver) -> Result<(), ResourceError> {
        for member in members {
            apply_status_patch(
                &self.messages,
                &member.metadata.name,
                &MessageStatusPatch::Delivered { receiver: receiver.clone(), at: receiver.delivered_at },
            )
            .await?;
            self.clear_signal(member).await?;
        }
        Ok(())
    }

    async fn failed(
        &self,
        members: &[ResourceObject<Message>],
        reason: &str,
        ambiguous: bool,
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        for member in members {
            let current = self.messages.get(&member.metadata.name).await?;
            let mut status = status_for(&current);
            if status.retry.as_ref().is_some_and(|retry| matches!(retry.disposition, ControllerRetryDisposition::Terminal { .. })) {
                if ambiguous
                    && status
                        .submission
                        .as_ref()
                        .is_some_and(|submission| crate::delivery_hold::delivery_hold_overdue(submission.started_at, now))
                    && member.metadata.name == members[0].metadata.name
                {
                    self.signal(&current, now).await?;
                }
                continue;
            }
            // Share the deployed legacy retry schedule and attempt budget.
            let mut retry = ControllerRetry::retryable(status.retry.as_ref(), now, delivery_retry_backoff());
            let held = ambiguous || delivery_retry_exhausted(retry.attempts);
            if held {
                retry.disposition = ControllerRetryDisposition::Terminal { needs: reason.into() };
            }
            if !ambiguous {
                status.failed_batch_members = members.iter().map(|message| message.metadata.name.clone()).collect();
                status.submission = None;
            }
            MessageStatusPatch::Wait {
                phase: MessagePhase::Deliverable,
                reason: format!(
                    "{}: {reason}; attempt {}/{}; inspect the recorded receiver session for acceptance evidence before replacing held intent",
                    if held { "delivery held" } else { "retrying delivery" },
                    retry.attempts,
                    DELIVERY_MAX_ATTEMPTS
                ),
                at: now,
            }
            .apply(&mut status);
            status.retry = Some(retry);
            self.write(&current, &status).await?;
            let overdue = status
                .submission
                .as_ref()
                .is_some_and(|submission| crate::delivery_hold::delivery_hold_overdue(submission.started_at, now));
            if held && (!ambiguous || overdue) && member.metadata.name == members[0].metadata.name {
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
        if current.status.as_ref().is_some_and(|current| current.phase.is_terminal()) {
            return Ok(());
        }
        if current.status.as_ref() != Some(status) {
            self.messages.update_status(&message.metadata.name, &current.metadata.resource_version, status).await?;
        }
        Ok(())
    }

    // Repair a crash between terminal closure/receipt and gate deletion. Only
    // actual receiver gates are checked; historical Messages cause no deletes.
    async fn cleanup_delivery_gates(&self, _now: DateTime<Utc>) -> Result<(), ResourceError> {
        let demands = self.backend.using::<Demand>(&self.namespace);
        let gates: Vec<_> = demands
            .list()
            .await?
            .items
            .into_iter()
            .filter(|demand| {
                demand.metadata.name.starts_with("terminal-delivery-")
                    && demand.metadata.owner_references.iter().any(|owner| owner.kind == "TerminalSession" && owner.controller)
            })
            .collect();
        if gates.is_empty() {
            return Ok(());
        }
        let active: Vec<_> = self
            .messages
            .query(&MessageQuery::active())
            .await?
            .into_iter()
            .filter(|message| {
                message.status.as_ref().is_some_and(|status| {
                    !status.phase.is_terminal()
                        && status.resolved_receiver.is_none()
                        && status
                            .retry
                            .as_ref()
                            .is_some_and(|retry| matches!(retry.disposition, ControllerRetryDisposition::Terminal { .. }))
                })
            })
            .collect();
        let mut retained = std::collections::BTreeSet::new();
        for message in active {
            if let Some(holder) = self.resolve_receiver(&message).await? {
                retained.insert(delivery_demand_name(&holder.object));
            }
        }
        for gate in gates {
            if retained.contains(&gate.metadata.name) {
                continue;
            }
            match demands.delete(&gate.metadata.name).await {
                Ok(()) | Err(ResourceError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    async fn signal(&self, message: &ResourceObject<Message>, now: DateTime<Utc>) -> Result<(), ResourceError> {
        let Some(holder) = self.resolve_receiver(message).await? else {
            return Ok(());
        };
        let demands = self.backend.using::<Demand>(&self.namespace);
        let (meta, spec) = delivery_demand(&holder.object);
        match demands.get(&meta.name).await {
            Ok(_) => return Ok(()),
            Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
        demands.create(&meta, &spec).await?;
        apply_status_patch(&demands, &meta.name, &DemandStatusPatch::Raise { as_of: now, authority: "terminal-delivery".into() }).await?;
        Ok(())
    }

    async fn clear_signal(&self, message: &ResourceObject<Message>) -> Result<(), ResourceError> {
        let Some(holder) = self.resolve_receiver(message).await? else {
            return Ok(());
        };
        let demands = self.backend.using::<Demand>(&self.namespace);
        match demands.delete(&delivery_demand_name(&holder.object)).await {
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
    let text = members.iter().map(|message| frame_message(&message.spec)).collect::<Vec<_>>().join("\n\n");
    MessageBatch::builder().id(submission.batch_id.clone()).holder(holder.clone()).submission(submission).text(text).build()
}

fn header_value(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '[' => '(',
            ']' => ')',
            '·' => '-',
            character if character.is_control() || (character.is_whitespace() && character != ' ') => ' ',
            character => character,
        })
        .collect()
}

fn frame_message(spec: &MessageSpec) -> String {
    let relation = match spec.relation {
        MessageRelation::Supervisor => "supervisor",
        MessageRelation::Peer => "peer",
        MessageRelation::Dependency => "dependency",
        MessageRelation::Dependee => "dependee",
        MessageRelation::System => "system",
    };
    let mut facts = Vec::new();
    let sender = if let Some(name) = spec.sender.strip_prefix("principal:") {
        if name == flotilla_protocol::PrincipalRef::IMPLICIT_NAME {
            "the operator".into()
        } else {
            format!("operator {}", header_value(name))
        }
    } else if let Some(source) = spec.sender.strip_prefix("system:") {
        facts.push(header_value(&source.replace('-', " ")));
        "flotilla".into()
    } else {
        // Shorten local crew addresses without losing attribution to remote convoys.
        let context = spec.receiver.rsplit_once('/').and_then(|(vessel, _)| vessel.rsplit_once('/')).map(|(convoy, _)| convoy);
        let sender = context.and_then(|context| spec.sender.strip_prefix(&format!("{context}/"))).unwrap_or(&spec.sender);
        header_value(sender)
    };
    if let Some(subject) = &spec.subject {
        let subject = match subject {
            MessageReference::ChangeRequest { number, .. } => format!("PR #{number}"),
            MessageReference::Issue { number, .. } => format!("issue #{number}"),
            MessageReference::Commit { revision, .. } => format!("commit {revision}"),
            MessageReference::Ref { name, .. } => format!("ref {name}"),
            MessageReference::Artifact { resource, .. } => format!("artifact {}", resource.name),
            MessageReference::Comment { id, .. } => format!("comment {id}"),
            MessageReference::ControlRecord { resource, .. } => format!("{} {}", resource.kind, resource.name),
        };
        facts.push(format!("re {}", header_value(&subject)));
    }
    match spec.expectation {
        MessageExpectation::None => {}
        MessageExpectation::Reply => facts.push("reply expected".into()),
        MessageExpectation::Outcome { .. } => facts.push("outcome expected".into()),
    }
    let suffix = if facts.is_empty() { String::new() } else { format!(" · {}", facts.join(" · ")) };
    format!("[from {sender} ({relation}){suffix}]\n\n{}", spec.body)
}

async fn release(transport: &dyn MessageTransport, batch: &MessageBatch) {
    // Receipt is already durable. A stuck bookkeeping release cannot reopen it.
    let _ = tokio::time::timeout(TRANSPORT_CALL_BOUND, transport.release(batch)).await;
}

#[cfg(test)]
mod framing_tests {
    use super::*;

    fn messages() -> Vec<MessageSpec> {
        vec![
            MessageSpec::builder()
                .sender("principal:implicit".into())
                .receiver("project/convoy/work/coder".into())
                .relation(MessageRelation::Supervisor)
                .body("Continue the work.".into())
                .build(),
            MessageSpec::builder()
                .sender("system:turn-rules".into())
                .receiver("project/convoy/work/coder".into())
                .relation(MessageRelation::System)
                .body("Complete this turn.".into())
                .build(),
            MessageSpec::builder()
                .sender("project/convoy/work/reviewer".into())
                .receiver("project/convoy/work/coder".into())
                .relation(MessageRelation::Peer)
                .body("Please address the review.".into())
                .subject(MessageReference::ChangeRequest {
                    service: "github".into(),
                    scope: "flotilla-org/flotilla".into(),
                    number: 2920,
                    revision: "head".into(),
                })
                .references(vec![MessageReference::ChangeRequest {
                    service: "github".into(),
                    scope: "flotilla-org/flotilla".into(),
                    number: 2920,
                    revision: "head".into(),
                }])
                .expectation(MessageExpectation::Reply)
                .build(),
        ]
    }

    // Header fields stay on one line; bodies (including empty bodies) are preserved exactly.
    // Named operators and remote crews retain their identity instead of being shortened as local peers.
    #[test]
    fn sender_context_and_header_boundaries() {
        for (sender, expected) in [
            ("principal:alice", "operator alice"),
            ("principal:alice\u{2028}operator", "operator alice operator"),
            ("other/convoy/work/reviewer", "other/convoy/work/reviewer"),
            ("project/other/work/reviewer", "project/other/work/reviewer"),
            ("principal:alice]\n[spoof · header", "operator alice) (spoof - header"),
        ] {
            let mut spec = messages().remove(0);
            spec.sender = sender.into();
            spec.body.clear();
            assert_eq!(frame_message(&spec), format!("[from {expected} (supervisor)]\n\n"));
        }
    }

    // A FIFO batch retains one plain header per message and no receipt identifiers.
    #[tokio::test]
    async fn multi_message_batch_snapshot() {
        let backend = crate::ResourceBackend::InMemory(crate::InMemoryBackend::default());
        let stored = backend.using::<Message>("project");
        let mut members = Vec::new();
        for (index, spec) in messages().iter().enumerate() {
            members.push(stored.create(&InputMeta::builder().name(format!("message-{index}")).build(), spec).await.expect("message"));
        }
        let holder = backend
            .using::<TerminalSession>("project")
            .create(
                &InputMeta::builder().name("terminal".into()).build(),
                &crate::TerminalSessionSpec {
                    env_ref: "environment".into(),
                    role: "coder".into(),
                    source: crate::TerminalSessionSource::Tool { command: "sh".into() },
                    cwd: "/repo".into(),
                    env: Default::default(),
                    pool: "cleat".into(),
                },
            )
            .await
            .expect("holder");
        let submission = MessageSubmission::builder()
            .batch_id("batch-id".into())
            .crew_id("crew".into())
            .session("session".into())
            .started_at(DateTime::UNIX_EPOCH)
            .members(members.iter().map(|message| message.metadata.name.clone()).collect())
            .build();
        let rendered = batch(&holder, &members, submission);
        assert_eq!(rendered.text, "[from the operator (supervisor)]\n\nContinue the work.\n\n[from flotilla (system) · turn rules]\n\nComplete this turn.\n\n[from work/reviewer (peer) · re PR #2920 · reply expected]\n\nPlease address the review.");
        assert_eq!(rendered.submission.members, vec!["message-0", "message-1", "message-2"]);
    }

    // Format-string glue: exact text snapshots cover each sender and omission of absent facts.
    #[test]
    fn single_message_snapshot() {
        let rendered = messages().iter().map(frame_message).collect::<Vec<_>>();
        assert_eq!(
            rendered,
            vec![
                "[from the operator (supervisor)]\n\nContinue the work.",
                "[from flotilla (system) · turn rules]\n\nComplete this turn.",
                "[from work/reviewer (peer) · re PR #2920 · reply expected]\n\nPlease address the review.",
            ]
        );
        for text in rendered {
            let header = text.lines().next().expect("header");
            assert!(!header.contains("message-") && !header.contains("none") && !header.contains('{'));
        }
    }
}
