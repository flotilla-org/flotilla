//! Durable Message reconciliation and terminal transport.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::{TerminalDeliveryFailure, TerminalDeliveryOutcome};
use flotilla_resources::{Resource, ResourceError, ResourceObject, TerminalAttentionSource, TerminalAttentionState, TerminalSession};
use flotilla_store::ResourceBackend;
use futures::StreamExt;

use super::state::ControllerRuntimeState;
use super::terminal::{deliver_guarded_and_confirm, PendingTerminalDelivery, TerminalControllerRuntime};

/// Dependency events wake one inbox pass, not one timer per pending record.
/// Federated watches include references and holders authored on other hosts.
pub(super) struct MessageDependencyWatch<R: Resource>(std::marker::PhantomData<R>);

impl<R: Resource> MessageDependencyWatch<R> {
    pub(super) fn boxed() -> Box<dyn flotilla_store::controller::SecondaryWatch<Primary = flotilla_resources::Message>> {
        Box::new(Self(std::marker::PhantomData))
    }
}

impl<R: Resource> flotilla_store::controller::SecondaryWatch for MessageDependencyWatch<R> {
    type Primary = flotilla_resources::Message;
    fn clone_box(&self) -> Box<dyn flotilla_store::controller::SecondaryWatch<Primary = Self::Primary>> {
        Self::boxed()
    }
    fn spawn(
        self: Box<Self>,
        backend: ResourceBackend,
        namespace: String,
        sender: flotilla_store::controller::WorkQueueSender,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            let resolver = backend.including_replicas::<R>(&namespace);
            let mut watch = resolver.watch().await?;
            loop {
                if let Some(message) = backend
                    .using::<flotilla_resources::Message>(&namespace)
                    .query(&flotilla_store::MessageQuery::active())
                    .await?
                    .into_iter()
                    .filter(|message| message.status.as_ref().is_none_or(|status| !status.phase.is_terminal()))
                    .min_by(|left, right| left.metadata.name.cmp(&right.metadata.name))
                {
                    sender.send(message.metadata.name).await.map_err(|_| ResourceError::other("Message controller queue closed"))?;
                }
                match watch.next().await {
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error),
                    None => return Ok(()),
                }
            }
        })
    }
}

pub(super) struct MessageController {
    pub(super) state: Arc<ControllerRuntimeState>,
    pub(super) namespace: String,
}

impl flotilla_store::controller::Reconciler for MessageController {
    type Resource = flotilla_resources::Message;
    type Prepared = Option<std::time::Duration>;
    async fn prepare(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Prepared, ResourceError> {
        if obj.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            let batch = obj.status.as_ref().and_then(|status| status.submission.as_ref()).map(|submission| submission.batch_id.as_str());
            let tracked = batch.is_some_and(|batch| {
                self.state
                    .terminal_deliveries
                    .lock()
                    .expect("terminal deliveries lock poisoned")
                    .values()
                    .any(|delivery| delivery.message_batch.as_deref() == Some(batch))
            });
            if tracked {
                flotilla_store::MessageTransport::release_closed(&TerminalControllerRuntime { state: Arc::clone(&self.state) }, &[]).await;
            }
            return Ok(None);
        }
        let inbox = self.state.daemon.message_inbox(&self.namespace).await;
        inbox.reconcile_delivery(&TerminalControllerRuntime { state: Arc::clone(&self.state) }, Utc::now()).await?;
        let messages = self
            .state
            .daemon
            .resource_backend()
            .using::<flotilla_resources::Message>(&self.namespace)
            .query(&flotilla_store::MessageQuery::active())
            .await?;
        let active: Vec<_> =
            messages.iter().filter(|message| message.status.as_ref().is_none_or(|status| !status.phase.is_terminal())).collect();
        // Only the lexicographically first active record owns the inbox deadline.
        if active.iter().map(|message| message.metadata.name.as_str()).min() != Some(obj.metadata.name.as_str()) {
            return Ok(None);
        }
        let now = Utc::now();
        let next = active
            .iter()
            .flat_map(|message| {
                let status = message.status.as_ref();
                let submission = status.and_then(|status| status.submission.as_ref());
                [
                    message.spec.deadline.filter(|deadline| *deadline > now),
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
    fn reconcile(
        &self,
        _: &ResourceObject<Self::Resource>,
        next: &Self::Prepared,
        _: chrono::DateTime<Utc>,
    ) -> flotilla_store::controller::ReconcileOutcome<Self::Resource> {
        let mut outcome = flotilla_store::controller::ReconcileOutcome::new(None);
        outcome.requeue_after = *next;
        outcome
    }
    async fn run_finalizer(&self, _: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        Ok(())
    }
    fn finalizer_name(&self) -> Option<&'static str> {
        None
    }
}

#[async_trait]
impl flotilla_store::MessageTransport for TerminalControllerRuntime {
    async fn observe(
        &self,
        holder: &ResourceObject<TerminalSession>,
        submission: Option<&flotilla_resources::MessageSubmission>,
    ) -> Result<flotilla_store::MessageObservation, String> {
        let status = holder.status.as_ref().ok_or("holder status is absent")?;
        let session = status.session_id.as_deref().ok_or("holder session is absent")?;
        let now = Utc::now();
        // Terminal reconciliation owns attention refresh. Reading its durable
        // observations avoids feeding our own observation writes back into the
        // Message dependency watch indefinitely.
        let current = self
            .state
            .daemon
            .resource_backend()
            .using::<TerminalSession>(&holder.metadata.namespace)
            .get(&holder.metadata.name)
            .await
            .map_err(|error| error.to_string())?;
        let status = current.status.as_ref().ok_or("holder status is absent")?;
        let attention = status.attention.as_ref().filter(|attention| !attention.is_stale_at(now));
        let evidence = submission.and_then(|submission| {
            if flotilla_resources::delivery_hold::delivery_acceptance_evidence(
                status.last_tool_activity_at.is_some_and(|at| at > submission.started_at),
                attention.is_some_and(|attention| {
                    attention.state == TerminalAttentionState::Working
                        && attention.as_of > submission.started_at
                        && attention.source == TerminalAttentionSource::Hook
                }),
                false,
                false,
                false,
            ) {
                Some("fresh agent tool activity after submission".into())
            } else {
                None
            }
        });
        let other_delivery = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned").contains_key(session);
        let evidence = if other_delivery { None } else { evidence };
        Ok(flotilla_store::MessageObservation {
            ready: !other_delivery && attention.is_some_and(|attention| attention.state == TerminalAttentionState::Idle),
            working: !other_delivery
                && attention.is_some_and(|attention| {
                    attention.state == TerminalAttentionState::Working
                        && submission.is_some_and(|submission| attention.as_of > submission.started_at)
                }),
            evidence,
            output_digest: status.last_output_digest.clone(),
            waiting_reason: Some("waiting for a fresh idle holder observation".into()),
        })
    }
    async fn release_closed(&self, _messages: &[ResourceObject<flotilla_resources::Message>]) {
        let batches: Vec<_> = self
            .state
            .terminal_deliveries
            .lock()
            .expect("terminal deliveries lock poisoned")
            .values()
            .filter_map(|delivery| delivery.message_batch.clone())
            .collect();
        let mut messages = Vec::new();
        let Ok(namespaces) = self.state.daemon.resource_backend().local_namespaces::<flotilla_resources::Message>().await else { return };
        for namespace in namespaces {
            for id in &batches {
                let Ok(members) = self
                    .state
                    .daemon
                    .resource_backend()
                    .using::<flotilla_resources::Message>(&namespace)
                    .query(&flotilla_store::MessageQuery::Batch { id: id.clone() })
                    .await
                else {
                    continue;
                };
                messages.extend(members);
            }
        }
        let mut deliveries = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned");
        let closed: Vec<_> = deliveries
            .iter()
            .filter_map(|(session, delivery)| {
                let batch_id = delivery.message_batch.as_deref()?;
                let submission = messages.iter().find_map(|message| {
                    message
                        .status
                        .as_ref()
                        .and_then(|status| status.submission.as_ref())
                        .filter(|submission| submission.batch_id == batch_id && submission.session == *session)
                })?;
                let all_closed = !submission.members.is_empty()
                    && submission.members.iter().all(|member| {
                        messages.iter().any(|message| {
                            message.metadata.name == *member && message.status.as_ref().is_some_and(|status| status.phase.is_terminal())
                        })
                    });
                all_closed.then(|| session.clone())
            })
            .collect();
        for session in closed {
            if let Some(delivery) = deliveries.remove(&session) {
                delivery.task.abort();
            }
        }
    }
    async fn submit(&self, batch: &flotilla_store::MessageBatch) -> flotilla_store::MessageTransportOutcome {
        match self.adapter_for_spec(&batch.holder.spec) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return flotilla_store::MessageTransportOutcome::NotSubmitted { reason: "agent acceptance observations unavailable".into() }
            }
            Err(reason) => return flotilla_store::MessageTransportOutcome::NotSubmitted { reason },
        }
        let pool = match self.pool_for_spec(&batch.holder.spec) {
            Ok(pool) => pool,
            Err(reason) => return flotilla_store::MessageTransportOutcome::NotSubmitted { reason },
        };
        let adapter = match self.adapter_for_spec(&batch.holder.spec) {
            Ok(adapter) => adapter,
            Err(reason) => return flotilla_store::MessageTransportOutcome::NotSubmitted { reason },
        };
        let mut deliveries = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned");
        if deliveries.contains_key(&batch.submission.session) {
            return flotilla_store::MessageTransportOutcome::NotSubmitted { reason: "another terminal turn reserved the transport".into() };
        }
        let session = batch.submission.session.clone();
        let text = batch.text.clone();
        let daemon = Arc::clone(&self.state.daemon);
        let namespace = batch.holder.metadata.namespace.clone();
        let members = batch.submission.members.clone();
        let task = tokio::spawn(async move {
            let inbox = daemon.message_inbox(&namespace).await;
            deliver_guarded_and_confirm(&*pool, adapter.as_deref(), &session, &text, &inbox, &members).await
        });
        deliveries.insert(
            batch.submission.session.clone(),
            PendingTerminalDelivery { message_batch: Some(batch.submission.batch_id.clone()), message: batch.text.clone(), task },
        );
        flotilla_store::MessageTransportOutcome::Pending
    }
    async fn release(&self, batch: &flotilla_store::MessageBatch) {
        let delivery = {
            let mut deliveries = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned");
            if deliveries
                .get(&batch.submission.session)
                .is_some_and(|delivery| delivery.message_batch.as_deref() == Some(batch.submission.batch_id.as_str()))
            {
                deliveries.remove(&batch.submission.session)
            } else {
                None
            }
        };
        if let Some(delivery) = delivery {
            delivery.task.abort();
        }
    }

    async fn poll(&self, batch: &flotilla_store::MessageBatch) -> flotilla_store::MessageTransportOutcome {
        let delivery = {
            let mut deliveries = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned");
            match deliveries.get(&batch.submission.session) {
                Some(delivery)
                    if delivery.message_batch.as_deref() == Some(batch.submission.batch_id.as_str())
                        && delivery.message == batch.text
                        && !delivery.task.is_finished() =>
                {
                    return flotilla_store::MessageTransportOutcome::Pending
                }
                Some(delivery)
                    if delivery.message_batch.as_deref() == Some(batch.submission.batch_id.as_str()) && delivery.message == batch.text =>
                {
                    deliveries.remove(&batch.submission.session)
                }
                _ => None,
            }
        };
        match delivery {
            Some(delivery) => message_transport_outcome(delivery.task.await.unwrap_or_else(|error| Err(error.to_string()))),
            None => flotilla_store::MessageTransportOutcome::Unconfirmed {
                reason: "submission task unavailable; observing acceptance without resubmission".into(),
            },
        }
    }
}

pub(super) fn message_transport_outcome(result: Result<TerminalDeliveryOutcome, String>) -> flotilla_store::MessageTransportOutcome {
    use flotilla_store::MessageTransportOutcome as Outcome;
    match result {
        Ok(TerminalDeliveryOutcome::Pending) => Outcome::Pending,
        Ok(TerminalDeliveryOutcome::Confirmed) => Outcome::Accepted { evidence: "holder remained Working after cleat submission".into() },
        Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::StartupNotReady)) => {
            Outcome::NotSubmitted { reason: "terminal was not ready before submission".into() }
        }
        Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed)) => {
            Outcome::Unconfirmed { reason: "cleat submission has no acceptance evidence yet".into() }
        }
        Err(reason) => Outcome::Unconfirmed { reason },
    }
}
