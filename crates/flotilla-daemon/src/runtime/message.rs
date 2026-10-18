//! Durable Message reconciliation and terminal transport.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::{TerminalDeliveryFailure, TerminalDeliveryOutcome};
use flotilla_resources::{Resource, ResourceError, ResourceObject, TerminalAttentionSource, TerminalAttentionState, TerminalSession};
use flotilla_store::ResourceBackend;
use futures::StreamExt;

use super::state::ControllerRuntimeState;
use super::terminal::{deliver_guarded_and_confirm, PendingTerminalDelivery, TerminalControllerRuntime};

/// Forward dependency changes without reading the inbox. A watch channel holds
/// one dirty bit for the namespace, including changes made during a pass.
async fn watch_delivery<R: Resource>(
    backend: ResourceBackend,
    namespace: String,
    wake: tokio::sync::watch::Sender<()>,
    queued: Arc<Mutex<Option<tokio::time::Instant>>>,
) -> Result<(), ResourceError> {
    let mut events = backend.including_replicas::<R>(&namespace).watch().await?;
    // A restart/relist must repair changes missed while the watch was down.
    queued.lock().expect("Message wake time").get_or_insert_with(tokio::time::Instant::now);
    wake.send_replace(());
    while let Some(event) = events.next().await {
        event?;
        queued.lock().expect("Message wake time").get_or_insert_with(tokio::time::Instant::now);
        wake.send_replace(());
    }
    Err(ResourceError::other("Message dependency watch closed"))
}

pub(super) struct MessageController {
    pub(super) state: Arc<ControllerRuntimeState>,
    pub(super) namespace: String,
}

impl MessageController {
    pub(super) async fn run(
        self,
        resync_interval: std::time::Duration,
        heartbeat: Arc<std::sync::atomic::AtomicU64>,
        metrics: Arc<flotilla_store::load_metrics::ControllerLoadMetrics>,
    ) -> Result<(), ResourceError> {
        let backend = self.state.daemon.resource_backend();
        let queued = Arc::new(Mutex::new(None));
        let (wake, receiver) = tokio::sync::watch::channel(());
        // JoinSet aborts every watch on cancellation, failure, or supervision
        // restart; no detached producer can survive the inbox loop.
        let mut watches = tokio::task::JoinSet::new();
        macro_rules! watch {
            ($kind:ty) => {
                watches.spawn(watch_delivery::<$kind>(backend.clone(), self.namespace.clone(), wake.clone(), Arc::clone(&queued)));
            };
        }
        watch!(flotilla_resources::Message);
        watch!(TerminalSession);
        watch!(flotilla_resources::Convoy);
        watch!(flotilla_resources::ConvoyEnsure);
        watch!(flotilla_resources::ChangeRequest);
        watch!(flotilla_resources::Issue);
        watch!(flotilla_resources::Artifact);
        watch!(flotilla_resources::Usage);
        watch!(flotilla_resources::Vessel);
        drop(wake);
        let inbox = self.state.daemon.message_inbox(&self.namespace).await;
        let transport = TerminalControllerRuntime { state: Arc::clone(&self.state) };
        // Durable batch intent makes passes safe to cancel and rerun after
        // watch failure; recovery observes held submissions without retyping.
        tokio::select! {
            result = run_delivery(&inbox, &transport, receiver, queued, resync_interval, heartbeat, metrics) => result,
            result = watches.join_next() => {
                match result {
                    Some(Ok(result)) => result,
                    Some(Err(error)) => Err(ResourceError::other(format!("Message watch task failed: {error}"))),
                    None => Err(ResourceError::other("Message dependency watches closed")),
                }
            }
        }
    }
}

/// Schedule the inbox as one unit, rather than reconciling it once per message.
/// Mark the dirty bit seen BEFORE the pass so writes racing the pass get a
/// follow-up. The single deadline is replaced after every pass, never spawned.
async fn run_delivery(
    inbox: &flotilla_messaging::MessageInbox,
    transport: &dyn flotilla_messaging::MessageTransport,
    mut wake: tokio::sync::watch::Receiver<()>,
    queued: Arc<Mutex<Option<tokio::time::Instant>>>,
    resync_interval: std::time::Duration,
    heartbeat: Arc<std::sync::atomic::AtomicU64>,
    metrics: Arc<flotilla_store::load_metrics::ControllerLoadMetrics>,
) -> Result<(), ResourceError> {
    let mut resync = tokio::time::interval(resync_interval);
    resync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut next = None;
    loop {
        heartbeat.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::select! {
            result = wake.changed() => {
                result.map_err(|_| ResourceError::other("Message inbox wake channel closed"))?;
            }
            _ = resync.tick() => {
                metrics.resyncs.increment();
                metrics.resync_enqueued.increment();
            }
            _ = async {
                match next {
                    Some(due) => tokio::time::sleep_until(due).await,
                    None => std::future::pending::<()>().await,
                }
            } => { metrics.requeues.increment(); }
        }
        // Dirty-bit and timestamp updates are independent: a wake racing this
        // pair may be timed against this pass instead of its follow-up. This
        // affects metric attribution only; the watch still schedules that pass.
        wake.borrow_and_update();
        let waited = queued.lock().expect("Message wake time").take().map_or(std::time::Duration::ZERO, |at| at.elapsed());
        metrics.queue_wait.record(waited);
        metrics.wakes.increment();
        metrics.queue_peak.observe(1);
        let _busy = metrics.busy();
        let started = tokio::time::Instant::now();
        let result = async {
            inbox.reconcile_delivery(transport, Utc::now()).await.inspect_err(|error| {
                tracing::warn!(controller = "Message", phase = "delivery", %error, "message inbox pass failed");
            })?;
            inbox.next_delivery_wake(Utc::now()).await.inspect_err(|error| {
                tracing::warn!(controller = "Message", phase = "deadline", %error, "message inbox pass failed");
            })
        }
        .await;
        // Include failed attempts, matching the object-controller histogram.
        metrics.reconcile.record(started.elapsed());
        match result {
            Ok(delay) => next = delay.map(|delay| tokio::time::Instant::now() + delay),
            Err(error) => {
                metrics.failures.increment();
                return Err(error);
            }
        }
    }
}

#[async_trait]
impl flotilla_messaging::MessageTransport for TerminalControllerRuntime {
    async fn observe(
        &self,
        holder: &ResourceObject<TerminalSession>,
        submission: Option<&flotilla_resources::MessageSubmission>,
    ) -> Result<flotilla_messaging::MessageObservation, String> {
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
        // The observer rereads durable state: reject evidence from a
        // replacement receiver even when the caller holds an older snapshot.
        let same_receiver = submission.is_none_or(|submission| {
            status.session_id.as_deref() == Some(submission.session.as_str())
                && status.crew.as_ref().map(|crew| crew.id.as_str()) == Some(submission.crew_id.as_str())
        });
        let evidence = submission.and_then(|submission| {
            if !same_receiver {
                return None;
            }
            if status.codex_completed_after(submission.started_at, now) {
                return Some("fresh Codex turn completion after submission".into());
            }
            if flotilla_resources::delivery_hold::delivery_acceptance_evidence(
                status.last_tool_activity_at.is_some_and(|at| at > submission.started_at)
                    || status.last_hook_working_at.is_some_and(|at| at > submission.started_at),
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
        Ok(flotilla_messaging::MessageObservation {
            ready: same_receiver && !other_delivery && attention.is_some_and(|attention| attention.state == TerminalAttentionState::Idle),
            working: same_receiver
                && !other_delivery
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
    async fn submit(&self, batch: &flotilla_messaging::MessageBatch) -> flotilla_messaging::MessageTransportOutcome {
        match self.adapter_for_spec(&batch.holder.spec) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return flotilla_messaging::MessageTransportOutcome::NotSubmitted {
                    reason: "agent acceptance observations unavailable".into(),
                }
            }
            Err(reason) => return flotilla_messaging::MessageTransportOutcome::NotSubmitted { reason },
        }
        let pool = match self.pool_for_spec(&batch.holder.spec) {
            Ok(pool) => pool,
            Err(reason) => return flotilla_messaging::MessageTransportOutcome::NotSubmitted { reason },
        };
        let adapter = match self.adapter_for_spec(&batch.holder.spec) {
            Ok(adapter) => adapter,
            Err(reason) => return flotilla_messaging::MessageTransportOutcome::NotSubmitted { reason },
        };
        let mut deliveries = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned");
        if deliveries.contains_key(&batch.submission.session) {
            return flotilla_messaging::MessageTransportOutcome::NotSubmitted {
                reason: "another terminal turn reserved the transport".into(),
            };
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
        flotilla_messaging::MessageTransportOutcome::Pending
    }
    async fn release(&self, batch: &flotilla_messaging::MessageBatch) {
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

    async fn poll(&self, batch: &flotilla_messaging::MessageBatch) -> flotilla_messaging::MessageTransportOutcome {
        let delivery = {
            let mut deliveries = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned");
            match deliveries.get(&batch.submission.session) {
                Some(delivery)
                    if delivery.message_batch.as_deref() == Some(batch.submission.batch_id.as_str())
                        && delivery.message == batch.text
                        && !delivery.task.is_finished() =>
                {
                    return flotilla_messaging::MessageTransportOutcome::Pending
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
            None => flotilla_messaging::MessageTransportOutcome::Unconfirmed {
                reason: "submission task unavailable; observing acceptance without resubmission".into(),
            },
        }
    }
}

pub(super) fn message_transport_outcome(result: Result<TerminalDeliveryOutcome, String>) -> flotilla_messaging::MessageTransportOutcome {
    use flotilla_messaging::MessageTransportOutcome as Outcome;
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
