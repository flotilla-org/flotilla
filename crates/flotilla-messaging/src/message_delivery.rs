// Receiver-side batching and durable transport accounting.
use std::{
    collections::{btree_map::Entry, BTreeMap, BTreeSet},
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_agent_connector::{
    Delivery, DeliveryTarget, HarnessRef, Operation, Routed, SubmissionRef, TransportId, TransportOutcome, TransportRouter,
};
use tracing::Instrument;

use crate::{
    apply_status_patch,
    delivery_hold::{
        delivery_acceptance_evidence, delivery_demand, delivery_demand_name, delivery_retry_backoff, delivery_retry_exhausted,
        CONTROLLER_FENCE_RECHECK_FOR, DELIVERY_HOLD_FOR, DELIVERY_HOLD_REASON_ANNOTATION, DELIVERY_MAX_ATTEMPTS,
    },
    message_expectation_open, message_supersedes,
    role_routing::topic_receiver_addresses,
    ControllerRetry, ControllerRetryDisposition, Demand, DemandStatusPatch, HeldMessageBatch, InputMeta, Message, MessageExpectation,
    MessageInbox, MessagePhase, MessageQuery, MessageReceiverSnapshot, MessageReference, MessageRelation, MessageSpec, MessageStatus,
    MessageStatusPatch, MessageSubmission, ReadResourceObject, ResolvedMessageReceiver, ResourceError, ResourceObject, ResourceProvenance,
    StatusPatch, TerminalSession, TerminalSessionPhase,
};

// Stable control marker shared by failure persistence and attention readers.
const RECEIVER_PASS_FAILURE: &str = "Message receiver pass failed:";

// Bound adapter calls independently of admission.
const TRANSPORT_CALL_BOUND: Duration = Duration::from_secs(30);

/// Every Message delivery is a new turn for its receiver.
const MESSAGE_OPERATION: Operation = Operation::NewTurn;

/// Evidence and members for one transport attempt, shared by initial and held outcomes.
struct DeliveryAttempt<'a> {
    members: &'a [ResourceObject<Message>],
    holder: &'a ResourceObject<TerminalSession>,
    submission: &'a MessageSubmission,
}

/// The holder as the transport router sees it: values, not the terminal record.
pub fn delivery_target(holder: &ResourceObject<TerminalSession>) -> DeliveryTarget {
    let status = holder.status.as_ref();
    let crew = status.and_then(|status| status.crew.as_ref());
    DeliveryTarget {
        namespace: holder.metadata.namespace.clone(),
        holder: holder.metadata.name.clone(),
        session: status.and_then(|status| status.session_id.clone()),
        crew_id: crew.map(|crew| crew.id.clone()),
        // The running harness version and live shapes are not recorded yet
        // (#3568): every offered shape stays eligible at any version.
        harness: crew.map(|crew| HarnessRef::new(crew.adapter.clone(), None)),
        live_shapes: None,
    }
}

/// The durable submission as the transport router sees it.
pub fn submission_ref(submission: &MessageSubmission) -> SubmissionRef {
    SubmissionRef {
        operator_reason: submission.operator_reason.clone(),
        batch_id: submission.batch_id.clone(),
        crew_id: submission.crew_id.clone(),
        session: submission.session.clone(),
        started_at: submission.started_at,
        members: submission.members.clone(),
        transport: submission.transport.clone().map(TransportId::new),
    }
}

impl MessageInbox {
    /// The namespace owns one replaceable deadline for delivery retries,
    /// expiry, hold escalation and acceptance debounce.
    pub async fn next_delivery_wake(&self, now: DateTime<Utc>) -> Result<Option<Duration>, ResourceError> {
        let messages = self.messages.query(&MessageQuery::active()).await?;
        let next = messages
            .iter()
            .flat_map(|message| {
                let status = message.status.as_ref();
                let submission = status.and_then(|status| status.submission.as_ref());
                [
                    message.spec.deadline.filter(|deadline| *deadline > now),
                    status
                        .filter(|status| {
                            status.submission.is_none() && status.fence_recheck_at.is_some() && HeldMessageBatch::is_held(message)
                        })
                        .map(|status| {
                            status.fence_recheck_at.unwrap_or(now + CONTROLLER_FENCE_RECHECK_FOR).max(now + CONTROLLER_FENCE_RECHECK_FOR)
                        }),
                    status.and_then(|status| status.retry.as_ref()).and_then(|retry| retry.next_attempt_at()).filter(|due| *due > now),
                    submission
                        .map(|submission| submission.started_at + flotilla_resources::delivery_hold::DELIVERY_HOLD_FOR)
                        .filter(|due| *due > now),
                    submission
                        .and_then(|submission| submission.working_since)
                        .map(|since| since + flotilla_resources::delivery_hold::DELIVERY_WORKING_DEBOUNCE_FOR)
                        .filter(|due| *due > now),
                ]
                .into_iter()
                .flatten()
            })
            .min();
        Ok(next.and_then(|due| (due - now).to_std().ok()))
    }

    /// A pass with active Messages observes holders, including held submissions.
    /// Idle passes skip receiver reads but still repair abandoned gates. Admission and
    /// delivery serialize separately; admission never waits on transport I/O.
    /// Holder evidence is frozen at pass start,
    /// including gate cleanup. Transport calls are bounded individually, but a
    /// pass may contain many groups: holder changes refresh on the next pass.
    /// Cleanup may temporarily retain or remove a gate using that older evidence;
    /// the next pass repairs it from current holders and active Messages.
    pub async fn reconcile_delivery(&self, router: &TransportRouter, now: DateTime<Utc>) -> Result<(), ResourceError> {
        let _delivery = self.delivery.lock().await;
        let mut by_receiver: BTreeMap<String, Vec<ResourceObject<Message>>> = BTreeMap::new();
        let messages = self.messages.query(&MessageQuery::active()).await?;
        // Keep the normal path to one frozen, addressed snapshot. If a bad
        // receiver prevents loading it, retry within each isolated boundary.
        let snapshot = self.load_delivery_receivers(&messages).await.ok();
        let mut receiver_keys: BTreeMap<(String, String), String> = BTreeMap::new();
        for message in messages {
            let address = (message.spec.receiver.clone(), message.spec.sender.clone());
            let key = match receiver_keys.get(&address) {
                Some(key) => key.clone(),
                None => {
                    // Aliases and topics that resolve to the same terminal
                    // still share one FIFO transport batch and failure boundary.
                    let key = match snapshot.as_ref() {
                        Some(snapshot) => match self.resolve_receiver(snapshot, &message).await {
                            Ok(Some(holder)) => format!("terminal/{}", holder.object.metadata.name),
                            _ => format!("address/{}", message.spec.receiver),
                        },
                        None => {
                            // Recover aliases before grouping even when another
                            // receiver prevented the shared snapshot from loading.
                            match self.load_delivery_receivers(std::slice::from_ref(&message)).await {
                                Ok(scoped) => match self.resolve_receiver(&scoped, &message).await {
                                    Ok(Some(holder)) => format!("terminal/{}", holder.object.metadata.name),
                                    _ => format!("address/{}", message.spec.receiver),
                                },
                                Err(_) => format!("address/{}", message.spec.receiver),
                            }
                        }
                    };
                    receiver_keys.insert(address, key.clone());
                    key
                }
            };
            by_receiver.entry(key).or_default().push(message);
        }
        let mut persistence_error = None;
        for (receiver, messages) in by_receiver {
            if let Err(error) = self.reconcile_receiver(router, snapshot.as_ref(), &messages, now).await {
                tracing::warn!(%receiver, %error, "message receiver pass failed");
                // Failure reporting can itself lose the store. Keep trying other
                // receivers before returning that infrastructure failure.
                if let Err(error) = self.record_receiver_failure(&messages, &error, now).await {
                    persistence_error.get_or_insert(error);
                }
            }
        }
        self.cleanup_delivery_gates(snapshot.as_ref(), now).await?;
        tokio::time::timeout(TRANSPORT_CALL_BOUND, router.release_closed()).await.ok();
        match persistence_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn record_receiver_failure(
        &self,
        messages: &[ResourceObject<Message>],
        error: &ResourceError,
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        match self.record_receiver_failure_inner(messages, error, now).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn record_receiver_failure_inner(
        &self,
        messages: &[ResourceObject<Message>],
        error: &ResourceError,
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        let reason = format!("{RECEIVER_PASS_FAILURE} {error}");
        let mut surviving = Vec::new();
        let mut persistence_error = None;
        for message in messages {
            let current = match self.messages.get(&message.metadata.name).await {
                Ok(current) => current,
                Err(ResourceError::NotFound { .. }) => continue,
                Err(error) => {
                    persistence_error.get_or_insert(error);
                    continue;
                }
            };
            let mut status = status_for(&current);
            if status.phase.is_terminal() {
                continue;
            }
            if status.reason.as_deref() != Some(&reason) {
                status.reason = Some(reason.clone());
                match self.write(&current, &status).await {
                    Ok(()) => {}
                    Err(ResourceError::NotFound { .. }) => continue,
                    Err(error) => {
                        persistence_error.get_or_insert(error);
                        continue;
                    }
                }
            }
            surviving.push(current);
        }
        if let Some(error) = persistence_error {
            return Err(error);
        }
        let messages = surviving.as_slice();
        let receivers = match self.load_delivery_receivers(messages).await {
            Ok(receivers) => receivers,
            Err(error) => {
                // The failure is durable on the addressed Messages even when
                // routing cannot identify a unique terminal to raise attention.
                tracing::warn!(%error, "cannot resolve receiver for recorded inbox failure");
                return Ok(());
            }
        };
        if let Some(first) = messages.first() {
            let holder = match self.resolve_receiver(&receivers, first).await {
                Ok(holder) => holder,
                Err(error) => {
                    tracing::warn!(receiver = %first.spec.receiver, %error, "cannot resolve receiver for recorded inbox failure");
                    return Ok(());
                }
            };
            if let Some(holder) = holder {
                let terminals = self.backend.using::<TerminalSession>(&self.namespace);
                let current = match terminals.get(&holder.object.metadata.name).await {
                    Ok(current) => current,
                    Err(ResourceError::NotFound { .. }) => return Ok(()),
                    Err(error) => return Err(error),
                };
                let mut status = current.status.clone().unwrap_or_default();
                // Preserve the fence's original proof and observation time:
                // an inbox error is not another transport attempt.
                if let Some(condition) = status.degraded.as_mut().filter(|condition| condition.is_delivery()) {
                    condition.message = reason.clone();
                    status.message = Some(reason.clone());
                    if current.status.as_ref() != Some(&status) {
                        terminals.update_status(&current.metadata.name, &current.metadata.resource_version, &status).await?;
                    }
                } else {
                    apply_status_patch(
                        &terminals,
                        &holder.object.metadata.name,
                        &crate::TerminalSessionStatusPatch::MarkDeliveryUnconfirmed {
                            message_id: first.metadata.name.clone(),
                            message: reason,
                            observed_at: now,
                        },
                    )
                    .await?;
                }
                match self.messages.get(&first.metadata.name).await {
                    Ok(current) => match self.signal(&receivers, &current, now).await {
                        Ok(()) | Err(ResourceError::NotFound { .. }) => {}
                        Err(error) => return Err(error),
                    },
                    Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }

    // Every receiver-dependent read, admission repair and transport operation
    // stays inside this boundary. A malformed receiver cannot abort its peers.
    async fn reconcile_receiver(
        &self,
        router: &TransportRouter,
        snapshot: Option<&MessageReceiverSnapshot>,
        input: &[ResourceObject<Message>],
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        {
            let _admission = self.admission.lock().await;
            // A created record with no status is an interrupted admission. Finish
            // its predecessor cleanup before selecting any batch for transport.
            for message in input.iter().filter(|message| message.status.is_none()) {
                self.accept_locked(&InputMeta::from(&message.metadata), &message.spec, now).await?;
            }
        }
        let repaired_admission = input.iter().any(|message| message.status.is_none());
        let mut messages = Vec::new();
        for message in input {
            let current = if repaired_admission { self.messages.get(&message.metadata.name).await? } else { message.clone() };
            if current.status.as_ref().is_none_or(|status| !status.phase.is_terminal()) {
                messages.push(current);
            }
        }
        if messages.is_empty() {
            return Ok(());
        }
        let loaded;
        let receivers = match snapshot {
            Some(snapshot) => snapshot,
            None => {
                loaded = self.load_delivery_receivers(&messages).await?;
                &loaded
            }
        };
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
        let mut resolved_receivers: BTreeMap<(String, String), Option<ReadResourceObject<TerminalSession>>> = BTreeMap::new();
        // Legacy receipts predate role_address. Freeze their recorded crew rows
        // on first correlation, then reuse them for every reply in this pass.
        let mut legacy_holders = BTreeMap::new();
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
                            && self.reply_sender_matches(message, &reply.object.spec.sender, &mut legacy_holders).await?
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
            let holder = if let Some(holder) = resolved_receivers.get(&receiver_key) {
                holder.clone()
            } else {
                let holder = self.resolve_receiver(receivers, message).await?;
                resolved_receivers.insert(receiver_key, holder.clone());
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
        let member_names: BTreeSet<_> = messages
            .iter()
            .flat_map(|message| {
                message.status.as_ref().into_iter().flat_map(|status| {
                    status
                        .submission
                        .as_ref()
                        .into_iter()
                        .flat_map(|submission| submission.members.iter())
                        .chain(status.failed_batch_members.iter())
                        .cloned()
                })
            })
            .collect();
        for name in member_names {
            if !messages.iter().any(|message| message.metadata.name == name) {
                messages.push(self.messages.get(&name).await?);
            }
        }
        for (_, (holder, pending)) in groups {
            self.deliver_group(receivers, router, &holder, &pending, &messages, now).await?;
        }
        Ok(())
    }

    /// Authorize exactly one additional attempt, retaining the old uncertain write.
    /// The command never restarts a child or treats authorization as acceptance.
    pub async fn deliver_held_batch(&self, name: &str, reason: &str) -> Result<(), ResourceError> {
        if reason.trim().is_empty() {
            return Err(ResourceError::invalid("operator delivery requires a reason"));
        }
        let _delivery = self.delivery.lock().await;
        let _admission = self.admission.lock().await;
        let message = self.messages.get(name).await?;
        if !HeldMessageBatch::is_held(&message) && !message.status.as_ref().is_some_and(|status| status.operator_delivery.is_some()) {
            return Err(ResourceError::invalid("Message batch is not held"));
        }
        let snapshot = self.load_delivery_receivers(std::slice::from_ref(&message)).await?;
        let holder = self.resolve_receiver(&snapshot, &message).await?.ok_or_else(|| ResourceError::invalid("receiver is unavailable"))?;
        if !matches!(holder.provenance, ResourceProvenance::Local) {
            return Err(ResourceError::invalid("operator delivery must run on the receiver host"));
        }
        let target = delivery_target(&holder.object);
        let request = crate::OperatorMessageDelivery {
            reason: reason.into(),
            session: target.session.ok_or_else(|| ResourceError::invalid("receiver session is unavailable"))?,
            crew_id: target.crew_id.ok_or_else(|| ResourceError::invalid("receiver crew is unavailable"))?,
        };
        let status = status_for(&message);
        let members =
            status.submission.as_ref().map(|submission| submission.members.clone()).unwrap_or_else(|| status.failed_batch_members.clone());
        let members = if members.is_empty() { vec![name.into()] } else { members };
        // Persist the request on each member. Any partial write leaves the batch
        // held: the delivery pass requires matching authorization on all members.
        for name in members {
            let current = self.messages.get(&name).await?;
            if !HeldMessageBatch::is_held(&current) && !current.status.as_ref().is_some_and(|status| status.operator_delivery.is_some()) {
                return Err(ResourceError::invalid("held batch changed during operator authorization"));
            }
            let mut status = status_for(&current);
            status.operator_delivery = Some(request.clone());
            self.write(&current, &status).await?;
        }
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
        let held_message_id = members.first().cloned().unwrap_or_else(|| name.into());
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
        let snapshot = self.load_delivery_receivers(std::slice::from_ref(&message)).await?;
        if let Some(holder) = self.resolve_receiver(&snapshot, &message).await? {
            apply_status_patch(
                &self.backend.using::<TerminalSession>(&self.namespace),
                &holder.object.metadata.name,
                &crate::TerminalSessionStatusPatch::ClearMessageDeliveryHold { message_id: held_message_id },
            )
            .await?;
        }
        self.clear_signal(&snapshot, &message).await
    }

    // Freeze only addressed holders. Topics expand to declared subscriber roles;
    // sender exclusion and holder precedence still run against the frozen batch.
    async fn load_delivery_receivers(&self, messages: &[ResourceObject<Message>]) -> Result<MessageReceiverSnapshot, ResourceError> {
        let mut addresses = BTreeSet::new();
        let topics = messages
            .iter()
            .filter(|message| message.spec.receiver.starts_with("topic:"))
            .map(|message| message.spec.receiver.as_str())
            .collect::<BTreeSet<_>>();
        addresses.extend(topic_receiver_addresses(&self.backend, &self.namespace, &topics.into_iter().collect::<Vec<_>>()).await?);
        for message in messages {
            if message.spec.receiver.starts_with("topic:") {
                continue;
            }
            addresses.insert(message.spec.receiver.clone());
        }
        MessageReceiverSnapshot::load_for_addresses(&self.backend, &self.namespace, &addresses.into_iter().collect::<Vec<_>>()).await
    }

    fn delivered_role_address(
        receivers: &MessageReceiverSnapshot,
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
            receivers
                .convoys()
                .iter()
                .find(|source| source.object.metadata.name == *convoy)
                .ok_or_else(|| ResourceError::not_found(convoy))?
                .object
                .spec
                .project_ref
                .clone()
        } else {
            message.spec.receiver.split('/').next().map(str::to_string)
        };
        Ok(project.map(|project| format!("{project}/{convoy}/{vessel}/{role}")))
    }

    async fn resolve_receiver(
        &self,
        receivers: &MessageReceiverSnapshot,
        message: &ResourceObject<Message>,
    ) -> Result<Option<ReadResourceObject<TerminalSession>>, ResourceError> {
        if message.spec.receiver.starts_with("topic:") {
            crate::role_routing::resolve_topic_receiver_in_snapshot(
                &self.backend,
                &self.namespace,
                &message.spec.receiver,
                &message.spec.sender,
                receivers,
            )
            .await
        } else {
            receivers.resolve(&message.spec.receiver)
        }
    }

    async fn reply_sender_matches(
        &self,
        message: &ResourceObject<Message>,
        sender: &str,
        legacy_holders: &mut BTreeMap<String, Vec<ReadResourceObject<TerminalSession>>>,
    ) -> Result<bool, ResourceError> {
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
        // Unlike routing holders frozen at pass start, compatibility-only rows
        // are read on first correlation. Cache even empty results for the pass.
        let holders = match legacy_holders.entry(receiver.crew_id.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(
                self.backend
                    .query_declared::<TerminalSession>(
                        &self.namespace,
                        &crate::StoreQuery {
                            fields: BTreeMap::from([("status.crew.id".into(), receiver.crew_id.clone())]),
                            include_replicas: true,
                            ..Default::default()
                        },
                    )
                    .await?
                    .items,
            ),
        };
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
        receivers: &MessageReceiverSnapshot,
        router: &TransportRouter,
        holder: &ResourceObject<TerminalSession>,
        pending: &[ResourceObject<Message>],
        all: &[ResourceObject<Message>],
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        let Some(first) = pending.first() else {
            return Ok(());
        };
        // A shared transport submission still belongs to every member's trace.
        let _batch_spans: Vec<_> = pending
            .iter()
            .skip(1)
            .map(|message| {
                let span = flotilla_tracing::queued_span(&message.metadata.name, "attempt");
                span.in_scope(|| tracing::info!("message delivery attempt"));
                span
            })
            .collect();
        self.deliver_group_traced(receivers, router, holder, pending, all, now)
            .instrument(flotilla_tracing::queued_span(&first.metadata.name, "attempt"))
            .await
    }

    async fn deliver_group_traced(
        &self,
        receivers: &MessageReceiverSnapshot,
        router: &TransportRouter,
        holder: &ResourceObject<TerminalSession>,
        pending: &[ResourceObject<Message>],
        all: &[ResourceObject<Message>],
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        tracing::info!("message delivery attempt");
        let remaining = self.supersede_held_system_turns(receivers, pending, all, now).await?;
        // Authorization belongs to one FIFO batch, never to newly queued text.
        let authorization =
            remaining.first().and_then(|message| message.status.as_ref()).and_then(|status| status.operator_delivery.as_ref());
        let pending = &remaining
            .iter()
            .take_while(|message| message.status.as_ref().and_then(|status| status.operator_delivery.as_ref()) == authorization)
            .cloned()
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return Ok(());
        }
        // Authorization cannot migrate to another activation, including after
        // its old submission was archived while waiting for readiness.
        if let Some(request) = authorization {
            let target = delivery_target(holder);
            if target.session.as_ref() != Some(&request.session) || target.crew_id.as_ref() != Some(&request.crew_id) {
                for message in pending {
                    let mut status = status_for(message);
                    status.operator_delivery = None;
                    status.fence_recheck_at = None;
                    let reason = "delivery held: operator authorization belongs to a previous receiver activation; inspect the current conversation before authorizing again";
                    status.retry = Some(ControllerRetry::terminal(status.retry.as_ref(), now, reason));
                    MessageStatusPatch::Wait { phase: MessagePhase::Deliverable, reason: reason.into(), at: now }.apply(&mut status);
                    self.write(message, &status).await?;
                }
                let first = self.messages.get(&pending[0].metadata.name).await?;
                self.signal(receivers, &first, now).await?;
                return Ok(());
            }
        }
        // The session marker is persisted before the batch members. Repair a
        // partially written refusal before observing Working/receipt evidence:
        // this batch is known unsent, even if some members still have their old
        // submission after a store error or daemon restart.
        if let Some(condition) = holder
            .status
            .as_ref()
            .and_then(|status| status.degraded.as_ref())
            .filter(|condition| condition.is_controller_fence_unverifiable())
        {
            if let Some(rejected) = pending.iter().find(|message| {
                condition.message_id.as_ref().is_some_and(|id| {
                    message.metadata.name == *id
                        || message.status.as_ref().is_some_and(|status| {
                            status.failed_batch_members.contains(id)
                                || status.submission.as_ref().is_some_and(|submission| submission.members.contains(id))
                        })
                })
            }) {
                let status = status_for(rejected);
                let names = status.submission.as_ref().map(|submission| &submission.members).unwrap_or(&status.failed_batch_members);
                let members = names
                    .iter()
                    .map(|name| {
                        all.iter()
                            .find(|message| message.metadata.name == *name)
                            .cloned()
                            .ok_or_else(|| ResourceError::invalid(format!("rejected message batch member `{name}` is absent")))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if members.iter().any(|member| {
                    member
                        .status
                        .as_ref()
                        .and_then(|status| status.submission.as_ref())
                        .is_some_and(|submission| submission.started_at <= condition.observed_at)
                }) {
                    self.controller_fence_unverifiable(receivers, &members, &condition.message, now).await?;
                    return Ok(());
                }
            }
        }
        if let Some(request) = pending
            .iter()
            .filter(|message| HeldMessageBatch::is_held(message))
            .find_map(|message| message.status.as_ref().and_then(|status| status.operator_delivery.clone()))
        {
            let authorized: Vec<_> = pending
                .iter()
                .filter(|message| message.status.as_ref().and_then(|status| status.operator_delivery.as_ref()) == Some(&request))
                .cloned()
                .collect();
            let batch = HeldMessageBatch::collect(&authorized);
            if batch
                .first()
                .is_some_and(|batch| batch.members.iter().all(|name| authorized.iter().any(|message| message.metadata.name == *name)))
            {
                let target = delivery_target(holder);
                if target.session.as_ref() == Some(&request.session) && target.crew_id.as_ref() == Some(&request.crew_id) {
                    for message in &authorized {
                        let mut status = status_for(message);
                        if let Some(previous) = status.submission.take() {
                            status.delivery_history.push(previous);
                        }
                        status.retry = None;
                        self.write(message, &status).await?;
                    }
                    // Reload to keep the authorization and pre-write intent together.
                    let mut refreshed = Vec::new();
                    for message in pending {
                        refreshed.push(self.messages.get(&message.metadata.name).await?);
                    }
                    return Box::pin(self.deliver_group_traced(receivers, router, holder, &refreshed, all, now)).await;
                }
            } else {
                // A command interrupted between member writes authorizes no
                // subset of the original batch. Wait for an explicit repair,
                // retaining attention even after the terminal marker was lost.
                if let Some(held) = pending.iter().find(|message| HeldMessageBatch::is_held(message)) {
                    self.signal(receivers, held, now).await?;
                }
                return Ok(());
            }
        }
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
        if existing.is_none() {
            // Preserve FIFO after a definitely-unsent batch exhausts its budget.
            // A later message must not silently bypass the operator's held intent.
            if let Some(held) = pending.iter().find(|message| {
                message
                    .status
                    .as_ref()
                    .and_then(|status| status.retry.as_ref())
                    .is_some_and(|retry| matches!(retry.disposition, ControllerRetryDisposition::Terminal { .. }))
            }) {
                self.signal(receivers, held, now).await?;
            }
            if pending.iter().any(|message| {
                message.status.as_ref().and_then(|status| status.retry.as_ref()).is_some_and(|retry| match &retry.disposition {
                    ControllerRetryDisposition::Retryable { next_attempt_at } => now < *next_attempt_at,
                    ControllerRetryDisposition::Terminal { .. } => {
                        let fence = message.status.as_ref().is_some_and(|status| status.fence_recheck_at.is_some())
                            || holder.status.as_ref().and_then(|status| status.degraded.as_ref()).is_some_and(|condition| {
                                condition.is_controller_fence_unverifiable()
                                    && pending.iter().any(|member| condition.message_id.as_deref() == Some(member.metadata.name.as_str()))
                            });
                        !fence || message.status.as_ref().and_then(|status| status.fence_recheck_at).is_some_and(|due| now < due)
                    }
                })
            }) {
                return Ok(());
            }
        }
        let target = delivery_target(holder);
        let existing_ref = existing.as_ref().map(submission_ref);
        let (planned, observation) =
            match tokio::time::timeout(TRANSPORT_CALL_BOUND, router.observe(MESSAGE_OPERATION, &target, existing_ref.as_ref()))
                .await
                .unwrap_or_else(|_| Err("transport observation timed out".into()))
            {
                Ok(observation) => observation,
                Err(error) => {
                    if let Some(submission) = &existing {
                        let held: Vec<_> =
                            pending.iter().filter(|message| submission.members.contains(&message.metadata.name)).cloned().collect();
                        if now.signed_duration_since(submission.started_at) >= DELIVERY_HOLD_FOR {
                            self.failed(receivers, &held, &error, true, now).await?;
                            return Ok(());
                        }
                        for message in pending {
                            self.wait(message, MessagePhase::Deliverable, &format!("held while observing acceptance: {error}"), now)
                                .await?;
                        }
                    } else {
                        self.failed(receivers, pending, &error, false, now).await?;
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
                self.accepted(receivers, &members, &receiver).await?;
                release(router, &delivery(holder, &members, submission.clone())).await;
                self.clear_signal(receivers, &members[0]).await?;
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
                        receivers,
                        &members,
                        &ResolvedMessageReceiver::builder()
                            .maybe_role_address(Self::delivered_role_address(receivers, &members[0], holder)?)
                            .crew_id(submission.crew_id.clone())
                            .session(submission.session.clone())
                            .delivered_at(now)
                            .evidence(receipt_evidence(&submission, evidence))
                            .maybe_transport(submission.transport.clone())
                            .build(),
                    )
                    .await?;
                    release(router, &delivery(holder, &members, submission.clone())).await;
                    self.clear_signal(receivers, &members[0]).await?;
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
                self.failed(receivers, &members, "recorded receiver session changed before acceptance was established", true, now).await?;
                return Ok(());
            }
            let delivery = delivery(holder, &members, submission.clone());
            let outcome = tokio::time::timeout(TRANSPORT_CALL_BOUND, router.poll(&delivery))
                .await
                .unwrap_or_else(|_| TransportOutcome::Unconfirmed { reason: "transport receipt poll timed out".into() });
            if matches!(outcome, TransportOutcome::Pending | TransportOutcome::Unconfirmed { .. })
                && now.signed_duration_since(submission.started_at) >= DELIVERY_HOLD_FOR
            {
                self.failed(receivers, &members, "submission acceptance remained unresolved past the hold bound", true, now).await?;
            } else {
                self.outcome(receivers, DeliveryAttempt { members: &members, holder, submission: &submission }, router, outcome, now)
                    .await?;
            }
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
        let operator = pending[0].status.as_ref().and_then(|status| status.operator_delivery.as_ref());
        if let Some(request) = operator {
            if request.session != *session
                || request.crew_id != crew.id
                || pending
                    .iter()
                    .any(|message| message.status.as_ref().and_then(|status| status.operator_delivery.as_ref()) != Some(request))
            {
                return Ok(());
            }
        }
        let submission = MessageSubmission::builder()
            .maybe_operator_reason(operator.map(|request| request.reason.clone()))
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
            // Recorded with the intent: the transport that just observed readiness
            // is the one the router tries first, so recovery polls it.
            .transport(planned.0)
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
            if status.fence_recheck_at.is_some() {
                status.retry = None;
            }
            status.submission = Some(submission.clone());
            status.operator_delivery = None;
            status.fence_recheck_at = None;
            self.write(message, &status).await?;
        }
        drop(admission);
        let mut submission = submission;
        let routed = tokio::time::timeout(TRANSPORT_CALL_BOUND, router.submit(&delivery(holder, pending, submission.clone())))
            .await
            // A timeout cannot say which transport was in flight. Any earlier
            // failover moved past a transport that definitely did not deliver, so
            // the planned transport stays recorded: recovery observes and polls it
            // without resubmitting, and holds the batch for an operator past the
            // hold bound if a later transport took it.
            .unwrap_or_else(|_| Routed {
                transport: submission.transport.clone().map(TransportId::new),
                outcome: TransportOutcome::Unconfirmed { reason: "transport submission timed out; input may have been accepted".into() },
                failovers: Vec::new(),
            });
        for failover in &routed.failovers {
            tracing::info!(batch = %submission.batch_id, transport = %failover.transport, reason = %failover.reason, "message delivery failed over");
        }
        let used = routed.transport.map(|transport| transport.0);
        if used != submission.transport {
            // A failover moved the batch: polls and the receipt follow the transport used.
            submission.transport = used;
            if !matches!(routed.outcome, TransportOutcome::NotSubmitted { .. }) {
                self.record_submission(pending, &submission).await?;
            }
        }
        self.outcome(receivers, DeliveryAttempt { members: pending, holder, submission: &submission }, router, routed.outcome, now).await
    }

    /// An operator or supervisor message supersedes the held system turns
    /// queued ahead of it for the same receiver (#3455). A held batch exists
    /// so that ambiguous input is never typed twice; superseding closes it
    /// without retyping, keeps its submission for the audit, and records the
    /// superseding message in each member's reason. Turn rules fire again if
    /// their condition still holds. A held batch with any non-system member
    /// stays held: only an operator may close another person's intent.
    async fn supersede_held_system_turns(
        &self,
        receivers: &MessageReceiverSnapshot,
        pending: &[ResourceObject<Message>],
        all: &[ResourceObject<Message>],
        now: DateTime<Utc>,
    ) -> Result<Vec<ResourceObject<Message>>, ResourceError> {
        let Some(successor) =
            pending.iter().find(|message| message.spec.is_operator_or_supervisor() && !HeldMessageBatch::is_held(message))
        else {
            return Ok(pending.to_vec());
        };
        let held = HeldMessageBatch::collect(all);
        let superseded: Vec<_> = held
            .iter()
            .filter(|batch| batch.system_only)
            .filter(|batch| batch.members.iter().any(|member| pending.iter().any(|message| message.metadata.name == *member)))
            .collect();
        if superseded.is_empty() {
            return Ok(pending.to_vec());
        }
        let _admission = self.admission.lock().await;
        let mut closed = BTreeSet::new();
        for batch in &superseded {
            tracing::info!(
                batch = %batch.batch, receiver = %batch.receiver, successor = %successor.metadata.name,
                "operator message supersedes held system turns"
            );
            for member in &batch.members {
                apply_status_patch(
                    &self.messages,
                    member,
                    &MessageStatusPatch::Finish {
                        phase: MessagePhase::Superseded,
                        reason: format!(
                            "superseded while held by {} message `{}`: {}",
                            if successor.spec.sender.starts_with("principal:") { "operator" } else { "supervisor" },
                            successor.metadata.name,
                            batch.reason
                        ),
                        at: now,
                    },
                )
                .await?;
                closed.insert(member.clone());
            }
        }
        let remaining: Vec<_> = pending.iter().filter(|message| !closed.contains(&message.metadata.name)).cloned().collect();
        // The receiver's gate stays raised while another held batch remains.
        if !remaining.iter().any(HeldMessageBatch::is_held) {
            if let Some(holder) = self.resolve_receiver(receivers, successor).await? {
                if let Some(id) = holder
                    .object
                    .status
                    .as_ref()
                    .and_then(|status| status.degraded.as_ref())
                    .and_then(|condition| condition.message_id.as_ref())
                    .filter(|id| closed.contains(*id))
                {
                    apply_status_patch(
                        &self.backend.using::<TerminalSession>(&self.namespace),
                        &holder.object.metadata.name,
                        &crate::TerminalSessionStatusPatch::ClearMessageDeliveryHold { message_id: id.clone() },
                    )
                    .await?;
                }
            }
            self.clear_signal(receivers, successor).await?;
        }
        Ok(remaining)
    }

    async fn outcome(
        &self,
        receivers: &MessageReceiverSnapshot,
        attempt: DeliveryAttempt<'_>,
        router: &TransportRouter,
        outcome: TransportOutcome,
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        let DeliveryAttempt { members, holder, submission } = attempt;
        match outcome {
            TransportOutcome::Accepted { evidence } if !evidence.is_empty() => {
                self.accepted(
                    receivers,
                    members,
                    &ResolvedMessageReceiver::builder()
                        .maybe_role_address(Self::delivered_role_address(receivers, &members[0], holder)?)
                        .crew_id(submission.crew_id.clone())
                        .session(submission.session.clone())
                        .delivered_at(now)
                        .evidence(receipt_evidence(submission, evidence))
                        .maybe_transport(submission.transport.clone())
                        .build(),
                )
                .await?;
                release(router, &delivery(holder, members, submission.clone())).await;
                self.clear_signal(receivers, &members[0]).await
            }
            // The router answers an exhausted fallback as not submitted; an
            // unsupported refusal that reaches here wrote nothing either.
            TransportOutcome::NotSubmitted { reason } | TransportOutcome::Unsupported { reason } => {
                self.failed(receivers, members, &reason, false, now).await
            }
            TransportOutcome::ControllerFenceUnverifiable { reason } => {
                self.controller_fence_unverifiable(receivers, members, &reason, now).await
            }
            TransportOutcome::Unconfirmed { reason } => {
                self.failed(receivers, members, &format!("held pending acceptance evidence: {reason}"), true, now).await
            }
            TransportOutcome::Pending => Ok(()),
            TransportOutcome::Accepted { .. } => {
                self.failed(receivers, members, "transport supplied an empty acceptance receipt", true, now).await
            }
        }
    }

    async fn accepted(
        &self,
        receivers: &MessageReceiverSnapshot,
        members: &[ResourceObject<Message>],
        receiver: &ResolvedMessageReceiver,
    ) -> Result<(), ResourceError> {
        for member in members {
            async {
                tracing::info!("message received");
                apply_status_patch(
                    &self.messages,
                    &member.metadata.name,
                    &MessageStatusPatch::Delivered { receiver: receiver.clone(), at: receiver.delivered_at },
                )
                .await?;
                if let Some(holder) = self
                    .resolve_receiver(receivers, member)
                    .await?
                    .filter(|holder| holder.object.status.as_ref().is_some_and(|status| status.degraded.is_some()))
                {
                    apply_status_patch(
                        &self.backend.using::<TerminalSession>(&self.namespace),
                        &holder.object.metadata.name,
                        &crate::TerminalSessionStatusPatch::ClearMessageDeliveryHold { message_id: member.metadata.name.clone() },
                    )
                    .await?;
                }
                self.clear_signal(receivers, member).await
            }
            .instrument(flotilla_tracing::queued_span(&member.metadata.name, "receipt"))
            .await?;
        }
        Ok(())
    }

    async fn controller_fence_unverifiable(
        &self,
        receivers: &MessageReceiverSnapshot,
        members: &[ResourceObject<Message>],
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        let Some(first) = members.first() else {
            return Err(ResourceError::invalid("controller fence refusal requires a nonempty batch"));
        };
        // Write the session-level proof first, so a partial batch write can be
        // repaired at the start of deliver_group_traced without retyping input.
        // A receiver lost between result and snapshot is still a known-unsent
        // batch: park it durably; a future holder lookup can raise its gate.
        let mut failures = 1;
        if let Some(holder) = self.resolve_receiver(receivers, first).await? {
            let current = self.messages.get(&first.metadata.name).await?;
            let submitted_at =
                current.status.as_ref().and_then(|status| status.submission.as_ref()).map(|submission| submission.started_at);
            let already_recorded = holder.object.status.as_ref().and_then(|status| status.degraded.as_ref()).is_some_and(|condition| {
                condition.is_controller_fence_unverifiable()
                    && condition.message_id.as_deref() == Some(first.metadata.name.as_str())
                    && submitted_at.is_none_or(|at| at <= condition.observed_at)
            });
            failures = holder
                .object
                .status
                .as_ref()
                .and_then(|status| status.degraded.as_ref())
                .filter(|condition| condition.message_id.as_deref() == Some(first.metadata.name.as_str()))
                .map_or(1, |condition| {
                    if already_recorded {
                        condition.consecutive_failures
                    } else {
                        condition.consecutive_failures.saturating_add(1)
                    }
                });
            if !already_recorded {
                apply_status_patch(
                    &self.backend.using::<TerminalSession>(&self.namespace),
                    &holder.object.metadata.name,
                    &crate::TerminalSessionStatusPatch::MarkControllerFenceUnverifiable {
                        message_id: first.metadata.name.clone(),
                        message: reason.into(),
                        observed_at: now,
                    },
                )
                .await?;
            }
        }
        for member in members {
            let current = self.messages.get(&member.metadata.name).await?;
            if current.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
                continue;
            }
            let mut status = status_for(&current);
            status.retry = Some(ControllerRetry::terminal(status.retry.as_ref(), now, reason));
            status.submission = None;
            status.fence_recheck_at = Some(now + crate::delivery_hold::controller_fence_recheck_delay(failures));
            status.failed_batch_members = members.iter().map(|message| message.metadata.name.clone()).collect();
            MessageStatusPatch::Wait { phase: MessagePhase::Deliverable, reason: reason.into(), at: now }.apply(&mut status);
            self.write(&current, &status).await?;
            if member.metadata.name == first.metadata.name {
                self.signal(receivers, &ResourceObject { status: Some(status), ..current }, now).await?;
            }
        }
        Ok(())
    }

    async fn failed(
        &self,
        receivers: &MessageReceiverSnapshot,
        members: &[ResourceObject<Message>],
        reason: &str,
        ambiguous: bool,
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        for member in members {
            let current = self.messages.get(&member.metadata.name).await?;
            if current.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
                continue;
            }
            let mut status = status_for(&current);
            if status.retry.as_ref().is_some_and(|retry| matches!(retry.disposition, ControllerRetryDisposition::Terminal { .. })) {
                if ambiguous
                    && status
                        .submission
                        .as_ref()
                        .is_some_and(|submission| crate::delivery_hold::delivery_hold_overdue(submission.started_at, now))
                    && member.metadata.name == members[0].metadata.name
                {
                    self.signal(receivers, &current, now).await?;
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
            let current = ResourceObject { status: Some(status.clone()), ..current };
            if held && member.metadata.name == members[0].metadata.name {
                self.signal(receivers, &current, now).await?;
            }
        }
        Ok(())
    }

    /// Rewrites the recorded submission on members still holding this batch.
    async fn record_submission(&self, members: &[ResourceObject<Message>], submission: &MessageSubmission) -> Result<(), ResourceError> {
        for member in members {
            let current = self.messages.get(&member.metadata.name).await?;
            let mut status = status_for(&current);
            if status.submission.as_ref().is_some_and(|recorded| recorded.batch_id == submission.batch_id) {
                status.submission = Some(submission.clone());
                self.write(&current, &status).await?;
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
        let previous = status.clone();
        MessageStatusPatch::Wait { phase, reason: reason.into(), at: now }.apply(&mut status);
        if status == previous {
            return Ok(());
        }
        let trace_span = flotilla_tracing::queued_span(&message.metadata.name, "waiting");
        async {
            tracing::info!(%reason, "message waiting");
            self.write(&current, &status).await
        }
        .instrument(trace_span)
        .await
    }

    async fn write(&self, message: &ResourceObject<Message>, status: &MessageStatus) -> Result<(), ResourceError> {
        let current = self.messages.get(&message.metadata.name).await?;
        if current.status.as_ref().is_some_and(|current| current.phase.is_terminal()) {
            return Ok(());
        }
        let mut status = status.clone();
        // Admission can finish after this pass captured a status-less intent.
        // Delivery must never clear an order assigned since that snapshot.
        status.accepted_sequence = current.status.as_ref().and_then(|current| current.accepted_sequence).or(status.accepted_sequence);
        if current.status.as_ref() != Some(&status) {
            self.messages.update_status(&message.metadata.name, &current.metadata.resource_version, &status).await?;
        }
        Ok(())
    }

    // Repair a crash between terminal closure/receipt and gate deletion. Only
    // actual receiver gates are checked; historical Messages cause no deletes.
    async fn cleanup_delivery_gates(&self, receivers: Option<&MessageReceiverSnapshot>, now: DateTime<Utc>) -> Result<(), ResourceError> {
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
                        && ((status.resolved_receiver.is_none()
                            && (status.operator_delivery.is_some()
                                || status.submission.as_ref().is_some_and(|submission| submission.operator_reason.is_some())
                                || status
                                    .retry
                                    .as_ref()
                                    .is_some_and(|retry| matches!(retry.disposition, ControllerRetryDisposition::Terminal { .. }))))
                            || status.reason.as_deref().is_some_and(|reason| reason.starts_with(RECEIVER_PASS_FAILURE)))
                })
            })
            .collect();
        let mut retained = BTreeSet::new();
        let mut unresolved = BTreeSet::new();
        let mut persistence_error = None;
        for message in &active {
            let resolution = match receivers {
                Some(receivers) => self.resolve_receiver(receivers, message).await,
                None => match self.load_delivery_receivers(std::slice::from_ref(message)).await {
                    Ok(scoped) => self.resolve_receiver(&scoped, message).await,
                    Err(error) => Err(error),
                },
            };
            match resolution {
                Ok(Some(holder)) => {
                    retained.insert(delivery_demand_name(&holder.object));
                }
                Ok(None) => {}
                Err(error) => {
                    unresolved.insert(message.metadata.name.clone());
                    if let Err(error) = self.record_receiver_failure(std::slice::from_ref(message), &error, now).await {
                        persistence_error.get_or_insert(error);
                    }
                }
            }
        }
        for gate in gates {
            if retained.contains(&gate.metadata.name) {
                continue;
            }
            // A routing failure retains only a gate whose owner's durable proof
            // names one of the unresolved Messages. Unrelated gates still retire.
            if !unresolved.is_empty() {
                if let Some(owner) = gate.metadata.owner_references.iter().find(|owner| owner.kind == "TerminalSession" && owner.controller)
                {
                    if let Ok(holder) = self.backend.using::<TerminalSession>(&self.namespace).get(&owner.name).await {
                        if holder
                            .status
                            .as_ref()
                            .and_then(|status| status.degraded.as_ref())
                            .and_then(|condition| condition.message_id.as_ref())
                            .is_some_and(|id| unresolved.contains(id))
                        {
                            continue;
                        }
                    }
                }
            }
            if let Err(error) = self.cleanup_delivery_gate(&gate).await {
                tracing::warn!(gate = %gate.metadata.name, %error, "message receiver gate cleanup failed");
                // The receiver's existing attention gate is also the durable
                // failure record when a malformed terminal cannot be read.
                let current = match demands.get(&gate.metadata.name).await {
                    Ok(current) => current,
                    Err(ResourceError::NotFound { .. }) => continue,
                    Err(error) => {
                        persistence_error.get_or_insert(error);
                        continue;
                    }
                };
                let mut meta = InputMeta::from(&current.metadata);
                let reason = format!("Message receiver gate cleanup failed: {error}");
                if meta.annotations.get(DELIVERY_HOLD_REASON_ANNOTATION) != Some(&reason) {
                    meta.annotations.insert(DELIVERY_HOLD_REASON_ANNOTATION.into(), reason);
                    if let Err(error) = demands.update(&meta, &current.metadata.resource_version, &current.spec).await {
                        persistence_error.get_or_insert(error);
                    }
                }
            }
        }
        match persistence_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn cleanup_delivery_gate(&self, gate: &ResourceObject<Demand>) -> Result<(), ResourceError> {
        let demands = self.backend.using::<Demand>(&self.namespace);
        if let Some(owner) = gate.metadata.owner_references.iter().find(|owner| owner.kind == "TerminalSession" && owner.controller) {
            let terminals = self.backend.using::<TerminalSession>(&self.namespace);
            match terminals.get(&owner.name).await {
                Ok(holder) => {
                    // The legacy queue shares this receiver gate and can still own it.
                    if flotilla_resources::delivery_hold::legacy_delivery_gate_needed(&holder) {
                        return Ok(());
                    }
                    if let Some(id) = holder
                        .status
                        .as_ref()
                        .and_then(|status| status.degraded.as_ref())
                        .filter(|condition| condition.is_delivery())
                        .and_then(|condition| condition.message_id.clone())
                    {
                        apply_status_patch(
                            &terminals,
                            &owner.name,
                            &crate::TerminalSessionStatusPatch::ClearMessageDeliveryHold { message_id: id },
                        )
                        .await?;
                    }
                }
                Err(ResourceError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        match demands.delete(&gate.metadata.name).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
        Ok(())
    }

    async fn signal(
        &self,
        receivers: &MessageReceiverSnapshot,
        message: &ResourceObject<Message>,
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        let Some(holder) = self.resolve_receiver(receivers, message).await? else {
            return Ok(());
        };
        let current = self.backend.using::<TerminalSession>(&self.namespace).get(&holder.object.metadata.name).await?;
        if !current.status.as_ref().and_then(|status| status.degraded.as_ref()).is_some_and(|condition| {
            condition.is_delivery()
                && condition.message_id.as_deref() == Some(message.metadata.name.as_str())
                && (!condition.is_controller_fence_unverifiable()
                    || message.status.as_ref().is_none_or(|status| {
                        status.submission.is_none()
                            || status.reason.as_deref().is_some_and(|reason| reason.starts_with(RECEIVER_PASS_FAILURE))
                    }))
        }) {
            apply_status_patch(
                &self.backend.using::<TerminalSession>(&self.namespace),
                &holder.object.metadata.name,
                &crate::TerminalSessionStatusPatch::MarkDeliveryUnconfirmed {
                    message_id: message.metadata.name.clone(),
                    message: message.status.as_ref().and_then(|status| status.reason.clone()).unwrap_or_else(|| "delivery held".into()),
                    observed_at: now,
                },
            )
            .await?;
        }
        let demands = self.backend.using::<Demand>(&self.namespace);
        let (mut meta, spec) = delivery_demand(&holder.object);
        let description = HeldMessageBatch::collect([message]).first().map(|batch| batch.describe(&self.namespace)).or_else(|| {
            message.status.as_ref().and_then(|status| status.reason.clone()).filter(|reason| reason.starts_with(RECEIVER_PASS_FAILURE))
        });
        match demands.get(&meta.name).await {
            Ok(existing) => {
                // Keep the gate's description current as the held reason changes,
                // and never leave one that no longer describes a held batch.
                if existing.metadata.annotations.get(DELIVERY_HOLD_REASON_ANNOTATION) != description.as_ref() {
                    let mut meta = InputMeta::from(&existing.metadata);
                    match description {
                        Some(description) => meta.annotations.insert(DELIVERY_HOLD_REASON_ANNOTATION.into(), description),
                        None => meta.annotations.remove(DELIVERY_HOLD_REASON_ANNOTATION),
                    };
                    demands.update(&meta, &existing.metadata.resource_version, &existing.spec).await?;
                }
                return Ok(());
            }
            Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
        if let Some(description) = description {
            meta.annotations.insert(DELIVERY_HOLD_REASON_ANNOTATION.into(), description);
        }
        demands.create(&meta, &spec).await?;
        apply_status_patch(&demands, &meta.name, &DemandStatusPatch::Raise { as_of: now, authority: "terminal-delivery".into() }).await?;
        Ok(())
    }

    async fn clear_signal(&self, receivers: &MessageReceiverSnapshot, message: &ResourceObject<Message>) -> Result<(), ResourceError> {
        let Some(holder) = self.resolve_receiver(receivers, message).await? else {
            return Ok(());
        };
        let demands = self.backend.using::<Demand>(&self.namespace);
        match demands.delete(&delivery_demand_name(&holder.object)).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn receipt_evidence(submission: &MessageSubmission, evidence: String) -> String {
    match &submission.operator_reason {
        Some(reason) => format!("operator-authorized delivery ({reason}): {evidence}"),
        None => evidence,
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

fn delivery(holder: &ResourceObject<TerminalSession>, members: &[ResourceObject<Message>], submission: MessageSubmission) -> Delivery {
    let text = members.iter().map(|message| frame_message(&message.spec)).collect::<Vec<_>>().join("\n\n");
    Delivery { operation: MESSAGE_OPERATION, target: delivery_target(holder), submission: submission_ref(&submission), text }
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

/// One Message as its turn text: a header naming sender, relation, subject
/// and expectation, then the body. Both delivery paths frame turns this way.
pub fn frame_message(spec: &MessageSpec) -> String {
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

async fn release(router: &TransportRouter, delivery: &Delivery) {
    // Receipt is already durable. A stuck bookkeeping release cannot reopen it.
    let _ = tokio::time::timeout(TRANSPORT_CALL_BOUND, router.release(delivery)).await;
}

#[cfg(test)]
mod framing_tests {
    use super::*;

    // Deleting a Message after the pass snapshot settles it; recording the
    // receiver failure must still reach every surviving Message in that batch.
    #[tokio::test]
    async fn failure_recording_skips_deleted_snapshot_members() {
        let backend = crate::ResourceBackend::new(flotilla_store_testkit::memory_backend());
        let inbox = MessageInbox::new(backend.clone(), "project");
        let stored = backend.using::<Message>("project");
        let mut intent = messages().remove(0);
        intent.receiver = "project/convoy/work/coder".into();
        let now = Utc::now();
        let mut snapshot = Vec::new();
        for name in ["deleted", "surviving"] {
            let meta = InputMeta::builder().name(name.into()).build();
            inbox.accept(&meta, &intent, now).await.unwrap();
            snapshot.push(stored.get(name).await.unwrap());
        }
        stored.delete("deleted").await.unwrap();
        inbox.record_receiver_failure(&snapshot, &ResourceError::invalid("broken receiver"), now).await.unwrap();
        let current = stored.get("surviving").await.unwrap();
        assert_eq!(current.status.unwrap().reason.as_deref(), Some("Message receiver pass failed: invalid resource: broken receiver"));
        assert!(matches!(stored.get("deleted").await, Err(ResourceError::NotFound { .. })));
    }

    // A pass can hold a snapshot from before admission finishes. Its later
    // status write must preserve the immutable order assigned in the meantime.
    #[tokio::test]
    async fn stale_delivery_snapshot_preserves_completed_admission() {
        let backend = crate::ResourceBackend::new(flotilla_store_testkit::memory_backend());
        let inbox = MessageInbox::new(backend.clone(), "project");
        let stored = backend.using::<Message>("project");
        let mut intent = messages().remove(0);
        intent.receiver = "project/convoy/work/coder".into();
        let meta = InputMeta::builder().name("admission-race".into()).build();
        let snapshot = stored.create(&meta, &intent).await.expect("partial intent");
        let mut proposed = status_for(&snapshot);
        proposed.phase = MessagePhase::Deliverable;
        let admitted = match inbox.accept(&meta, &intent, Utc::now()).await.expect("finish admission") {
            crate::MessageAdmission::Accepted(message) => message,
            other => panic!("unexpected admission {other:?}"),
        };
        assert!(admitted.status.as_ref().expect("admitted status").accepted_sequence.is_some());
        inbox.write(&snapshot, &proposed).await.expect("stale pass must preserve admission order");
        let current = stored.get(&meta.name).await.expect("current message");
        assert_eq!(
            current.status.as_ref().expect("status").accepted_sequence,
            admitted.status.as_ref().expect("admitted status").accepted_sequence
        );
        assert_eq!(current.status.expect("status").phase, MessagePhase::Deliverable);
    }

    // A receiver disappearing before failure persistence does not turn known
    // unsent input into a receipt or a retry. With no holder there is no gate
    // target; the durable failed batch remains for a future holder lookup.
    #[tokio::test]
    async fn unverifiable_fence_without_a_receiver_still_parks_the_batch() {
        let backend = crate::ResourceBackend::new(flotilla_store_testkit::memory_backend());
        let inbox = MessageInbox::new(backend.clone(), "project");
        let stored = backend.using::<Message>("project");
        let mut intent = messages().remove(0);
        intent.receiver = "project/convoy/work/coder".into();
        let message = stored.create(&InputMeta::builder().name("lost-holder".into()).build(), &intent).await.expect("intent");
        let receivers = MessageReceiverSnapshot::load(&backend, "project").await.expect("empty snapshot");
        assert!(inbox.resolve_receiver(&receivers, &message).await.expect("lookup").is_none());
        let reason = crate::CONTROLLER_FENCE_UNVERIFIABLE_MESSAGE;
        inbox.controller_fence_unverifiable(&receivers, &[message], reason, Utc::now()).await.expect("park known-unsent input");
        let status = stored.get("lost-holder").await.expect("message").status.expect("failure");
        assert!(status.resolved_receiver.is_none());
        assert!(status.submission.is_none());
        assert_eq!(status.reason.as_deref(), Some(reason));
        assert!(matches!(status.retry.expect("retry disposition").disposition, ControllerRetryDisposition::Terminal { .. }));
        assert!(backend.using::<Demand>("project").list().await.expect("gates").items.is_empty());
        assert!(inbox.controller_fence_unverifiable(&receivers, &[], reason, Utc::now()).await.is_err());
    }

    #[derive(Clone, Default)]
    struct WaitSpans(std::sync::Arc<std::sync::atomic::AtomicU64>);
    impl tracing::Subscriber for WaitSpans {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    // Review #3715: repeated waits with the same phase/reason emit one span;
    // a changed reason emits another state entry, and preserves the audit time.
    #[tokio::test]
    async fn unchanged_wait_does_not_emit_more_spans() {
        use tracing::instrument::WithSubscriber;
        let backend = crate::ResourceBackend::new(flotilla_store_testkit::memory_backend());
        let inbox = MessageInbox::new(backend.clone(), "project");
        let stored = backend.using::<Message>("project");
        let message = stored.create(&InputMeta::builder().name("waiting".into()).build(), &messages().remove(0)).await.expect("intent");
        let spans = WaitSpans::default();
        async {
            let first = Utc::now();
            for offset in 0..8 {
                inbox
                    .wait(&message, MessagePhase::Deliverable, "holder unavailable", first + chrono::Duration::seconds(offset))
                    .await
                    .expect("wait");
                assert_eq!(spans.0.load(std::sync::atomic::Ordering::SeqCst), 1);
            }
            assert_eq!(stored.get("waiting").await.expect("record").status.expect("status").since, first);
            inbox
                .wait(&message, MessagePhase::Deliverable, "transport unavailable", first + chrono::Duration::seconds(10))
                .await
                .expect("new reason");
            assert_eq!(spans.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        }
        .with_subscriber(spans.clone())
        .await;
    }

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

    // A topic receipt uses this pass's convoy evidence. A missing convoy refuses
    // receipt identity even if that convoy appears later; only a fresh pass sees it.
    #[tokio::test]
    async fn topic_receipt_requires_convoy_in_snapshot() {
        let backend = crate::ResourceBackend::new(flotilla_store_testkit::memory_backend());
        let mut intent = messages().remove(0);
        intent.receiver = "topic:project/supervision".into();
        let message = backend
            .using::<Message>("project")
            .create(&InputMeta::builder().name("message".into()).build(), &intent)
            .await
            .expect("message");
        let holder = backend
            .using::<TerminalSession>("project")
            .create(
                &InputMeta::builder()
                    .name("terminal".into())
                    .labels(std::collections::BTreeMap::from([
                        (crate::CONVOY_LABEL.into(), "convoy".into()),
                        (crate::VESSEL_LABEL.into(), "work".into()),
                        (crate::ROLE_LABEL.into(), "coder".into()),
                    ]))
                    .build(),
                &crate::TerminalSessionSpec::builder()
                    .env_ref("environment".into())
                    .role("coder".into())
                    .source(crate::TerminalSessionSource::Tool { command: "sh".into() })
                    .cwd("/repo".into())
                    .pool("cleat".into())
                    .build(),
            )
            .await
            .expect("holder");
        let old = MessageReceiverSnapshot::load(&backend, "project").await.expect("snapshot");
        backend
            .using::<crate::Convoy>("project")
            .create(
                &InputMeta::builder().name("convoy".into()).build(),
                &crate::ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("project".into()).build(),
            )
            .await
            .expect("convoy created after snapshot");
        assert!(matches!(MessageInbox::delivered_role_address(&old, &message, &holder), Err(ResourceError::NotFound { .. })));
        let fresh = MessageReceiverSnapshot::load(&backend, "project").await.expect("fresh snapshot");
        assert_eq!(
            MessageInbox::delivered_role_address(&fresh, &message, &holder).expect("receipt identity"),
            Some("project/convoy/work/coder".into())
        );
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
        let backend = crate::ResourceBackend::new(flotilla_store_testkit::memory_backend());
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
        let rendered = delivery(&holder, &members, submission);
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
