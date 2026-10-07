//! One-generation adoption of stored crew queues (ADR 0047). Publish every
//! record before removing its old payload; replay uses receiver-scoped IDs.
use chrono::{DateTime, Utc};

use crate::{
    delivery_hold::{delivery_retry_delay, delivery_retry_exhausted},
    ControllerRetry, ControllerRetryDisposition, Convoy, CrewMessageDelivery, CrewMessageSender, InputMeta, Message, MessageAdmission,
    MessageInbox, MessageRelation, MessageSpec, MessageSubmission, ResourceError, ResourceObject, TerminalCrewMessage, TerminalSession,
    TerminalSessionSource, TypedResolver, CONVOY_LABEL, TERMINAL_DELIVERY_EXPIRED_REASON, TERMINAL_DELIVERY_UNCONFIRMED_REASON,
    VESSEL_LABEL,
};

/// Retired sender variants are decoded only to adopt the previous generation's
/// payloads. New producers carry address and relation separately.
pub fn legacy_message_sender(sender: &CrewMessageSender) -> (String, MessageRelation) {
    match sender {
        CrewMessageSender::OperatorResume { principal } | CrewMessageSender::OperatorFollowUp { principal } => (
            format!(
                "principal:{}",
                principal.as_ref().map_or(flotilla_protocol::PrincipalRef::IMPLICIT_NAME, |principal| principal.name.as_str())
            ),
            MessageRelation::Supervisor,
        ),
        CrewMessageSender::Governor { name } => (legacy_actor_address(name), MessageRelation::Supervisor),
        CrewMessageSender::Bosun { name } => (legacy_actor_address(name), MessageRelation::Supervisor),
        CrewMessageSender::Handoff { from } => (legacy_actor_address(from), MessageRelation::Peer),
        CrewMessageSender::FlotillaNudge => ("system:nudge".into(), MessageRelation::System),
        CrewMessageSender::FlotillaTurn { .. } => ("system:turn-rules".into(), MessageRelation::System),
        CrewMessageSender::FlotillaEscalation { .. } => ("system:stall-judge".into(), MessageRelation::Supervisor),
        CrewMessageSender::Unknown => ("system:legacy".into(), MessageRelation::System),
    }
}

fn legacy_actor_address(actor: &str) -> String {
    if let Some((crew, convoy)) = actor.split_once(" in ") {
        if let Some((role, vessel)) = crew.split_once('@') {
            return if let Some((convoy, project)) = convoy.split_once('@') {
                format!("{project}/{convoy}/{vessel}/{role}")
            } else {
                format!("{convoy}/{vessel}/{role}")
            };
        }
    }
    if let Some((role, project)) = actor.split_once('@') {
        return format!("{project}/{role}");
    }
    format!("principal:{}", actor.replace([' ', '/', '@'], "_"))
}

pub fn legacy_message_spec(receiver: &str, message: &TerminalCrewMessage) -> MessageSpec {
    let (mut sender, relation) = legacy_message_sender(&message.sender);
    if let [project, convoy, vessel, _] = receiver.split('/').collect::<Vec<_>>().as_slice() {
        // Old bosun/handoff headers sometimes omit convoy context but retain
        // role@vessel. Governor role@project is a declared project role instead.
        match &message.sender {
            CrewMessageSender::Bosun { name } | CrewMessageSender::Handoff { from: name } if !name.contains(" in ") => {
                if let Some((role, sender_vessel)) = name.split_once('@') {
                    sender = format!("{project}/{convoy}/{sender_vessel}/{role}");
                }
            }
            _ => {}
        }
        let context = crate::MessageAddressContext { project: (*project).into(), convoy: (*convoy).into(), vessel: (*vessel).into() };
        if let Ok(qualified) = crate::qualify_message_address(&sender, &context) {
            sender = qualified;
        }
    }
    MessageSpec::builder().sender(sender).receiver(receiver.into()).relation(relation).body(message.text.clone()).build()
}

impl TypedResolver<TerminalSession> {
    /// Test-only fixture entry point for previous-generation sender envelopes.
    /// Production producers construct MessageSpec directly.
    /// Remove after the first fleet roll deploying #2710.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn accept_crew_message(
        &self,
        terminal: &ResourceObject<TerminalSession>,
        message: &TerminalCrewMessage,
        now: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        let convoy_name =
            terminal.metadata.labels.get(CONVOY_LABEL).ok_or_else(|| ResourceError::invalid("crew message requires convoy context"))?;
        let vessel =
            terminal.metadata.labels.get(VESSEL_LABEL).ok_or_else(|| ResourceError::invalid("crew message requires vessel context"))?;
        let convoy = self.backend.including_replicas::<Convoy>(&self.namespace).get(convoy_name).await?;
        let project = convoy.object.spec.project_ref.as_deref().unwrap_or(&self.namespace);
        let receiver = format!("{project}/{convoy_name}/{vessel}/{}", terminal.spec.role);
        let spec = legacy_message_spec(&receiver, message);
        let name = crate::message_record_name(&receiver, &spec.sender, &message.id);
        MessageInbox::new(self.backend.clone(), &self.namespace).accept(&InputMeta::builder().name(name).build(), &spec, now).await?;
        Ok(())
    }

    /// The receiver home alone adopts its stored queue. Unconfirmed submissions
    /// retain their original holder and retry ceiling rather than being retyped.
    /// Returns true after changing the terminal; callers must reread its spec.
    pub async fn adopt_legacy_messages(
        &self,
        terminal: &ResourceObject<TerminalSession>,
        now: DateTime<Utc>,
    ) -> Result<bool, ResourceError> {
        let TerminalSessionSource::Agent { message: Some(head), .. } = &terminal.spec.source else {
            // After payload adoption, delivery degradation belongs to Message.
            // Retry a conflicted cleanup without interpreting it as a receipt.
            if matches!(&terminal.spec.source, TerminalSessionSource::Agent { message: None, .. })
                && terminal.status.as_ref().and_then(|status| status.degraded.as_ref()).is_some_and(|condition| condition.is_delivery())
            {
                let mut status = terminal.status.clone().expect("delivery condition");
                status.degraded = None;
                status.message = None;
                self.update_status(&terminal.metadata.name, &terminal.metadata.resource_version, &status).await?;
                return Ok(true);
            }
            return Ok(false);
        };
        let convoy_name = terminal
            .metadata
            .labels
            .get(CONVOY_LABEL)
            .ok_or_else(|| ResourceError::invalid("legacy message adoption awaits its convoy address"))?;
        let vessel = terminal
            .metadata
            .labels
            .get(VESSEL_LABEL)
            .ok_or_else(|| ResourceError::invalid("legacy message adoption awaits its vessel address"))?;
        let convoy = self.backend.including_replicas::<Convoy>(&self.namespace).get(convoy_name).await?;
        let project = convoy.object.spec.project_ref.as_deref().unwrap_or(&self.namespace);
        let receiver = format!("{project}/{convoy_name}/{vessel}/{}", terminal.spec.role);
        let inbox = MessageInbox::new(self.backend.clone(), &self.namespace);
        let delivered = terminal.status.as_ref().and_then(|status| status.delivered_message_id.as_deref());
        for (index, legacy) in head.pending_after(delivered).into_iter().enumerate() {
            let spec = legacy_message_spec(&receiver, legacy);
            let name = crate::message_record_name(&receiver, &spec.sender, &legacy.id);
            let MessageAdmission::Accepted(record) = inbox.accept(&InputMeta::builder().name(name.clone()).build(), &spec, now).await?
            else {
                continue;
            };
            if record
                .status
                .as_ref()
                .is_some_and(|status| status.submission.is_some() || status.phase.has_delivery_evidence() || status.phase.is_terminal())
            {
                continue;
            }
            let old_status = terminal.status.as_ref();
            let degraded = old_status
                .and_then(|status| status.degraded.as_ref())
                .filter(|degraded| degraded.message_id.as_deref() == Some(&legacy.id));
            let uncertain = index == 0
                && (legacy.delivery == CrewMessageDelivery::LaunchBrief
                    || degraded.is_some_and(|condition| {
                        matches!(condition.reason.as_str(), TERMINAL_DELIVERY_UNCONFIRMED_REASON | TERMINAL_DELIVERY_EXPIRED_REASON)
                    }));
            if uncertain || degraded.is_some() {
                let mut status = record.status.clone().unwrap_or_default();
                if uncertain {
                    status.submission = Some(
                        MessageSubmission::builder()
                            .batch_id(format!("legacy-{name}"))
                            .crew_id(old_status.and_then(|status| status.crew.as_ref()).map_or_else(String::new, |crew| crew.id.clone()))
                            .session(old_status.and_then(|status| status.session_id.clone()).unwrap_or_default())
                            .started_at(degraded.map_or(
                                old_status.and_then(|status| status.started_at).unwrap_or(terminal.metadata.creation_timestamp),
                                |condition| condition.observed_at,
                            ))
                            .members(vec![name.clone()])
                            .maybe_legacy_launch((legacy.delivery == CrewMessageDelivery::LaunchBrief).then(|| {
                                crate::LegacyMessageLaunch {
                                    terminal: terminal.metadata.name.clone(),
                                    terminal_created_at: terminal.metadata.creation_timestamp,
                                    terminal_started_at: old_status.and_then(|status| status.started_at),
                                    content: legacy.text.clone(),
                                }
                            }))
                            .maybe_output_digest(old_status.and_then(|status| status.last_output_digest.clone()))
                            .build(),
                    );
                }
                let failure_at = degraded.map_or(now, |condition| condition.observed_at);
                let attempts = degraded.map_or(1, |condition| condition.consecutive_failures);
                status.retry = Some(ControllerRetry {
                    attempts,
                    first_failure_at: failure_at,
                    disposition: if uncertain || delivery_retry_exhausted(attempts) {
                        ControllerRetryDisposition::Terminal {
                            needs: "establish acceptance of the adopted delivery before resending".into(),
                        }
                    } else {
                        ControllerRetryDisposition::Retryable {
                            next_attempt_at: failure_at
                                + chrono::Duration::from_std(delivery_retry_delay(attempts)).expect("shared retry delay fits chrono"),
                        }
                    },
                });
                status.reason = Some("adopted prior-generation delivery state".into());
                self.backend.using::<Message>(&self.namespace).update_status(&name, &record.metadata.resource_version, &status).await?;
            }
        }
        let mut current = terminal.clone();
        let mut status = current.status.clone().unwrap_or_default();
        // Delivery degradation has been durably transferred to the adopted
        // Message above. Clear it together with receipts before retiring payload.
        if status.degraded.as_ref().is_some_and(|condition| condition.is_delivery()) {
            status.degraded = None;
            status.message = None;
        }
        let mut receipts = head.acknowledged.clone();
        for legacy in std::iter::once(head).chain(head.following.iter()) {
            if head.delivered_through(delivered, &legacy.id) {
                receipts.insert(legacy.id.clone());
            }
        }
        let witness = status.crew.as_ref().zip(status.session_id.as_ref()).map(|(crew, session)| crate::ResolvedMessageReceiver {
            crew_id: crew.id.clone(),
            session: session.clone(),
            delivered_at: now,
            evidence: "explicit previous-generation delivered-message receipt".into(),
            role_address: None,
        });
        for id in receipts {
            let sender = std::iter::once(head)
                .chain(head.following.iter())
                .find(|message| message.id == id)
                .map(|message| legacy_message_spec(&receiver, message).sender);
            // Only the latest explicit receipt retains its original session.
            // Historical acknowledgements may precede a restart and have no
            // per-message receiver identity in the old stored shape.
            let receiver = (delivered == Some(id.as_str())).then(|| witness.clone()).flatten();
            status.legacy_message_receipts.entry(id).or_insert(crate::LegacyMessageReceipt { sender, receiver });
        }
        if current.status.as_ref() != Some(&status) {
            current = self.update_status(&current.metadata.name, &current.metadata.resource_version, &status).await?;
        }
        let mut spec = terminal.spec.clone();
        if let TerminalSessionSource::Agent { message, .. } = &mut spec.source {
            *message = None;
        }
        self.update(&InputMeta::from(&current.metadata), &current.metadata.resource_version, &spec).await?;
        Ok(true)
    }
}
