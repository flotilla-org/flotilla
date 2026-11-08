//! The engine path's delivery adapter (#3317), behind the `[primitives]`
//! switch: the consumer that takes crew turns from role inboxes.
//!
//! Each pass reads today's role routing through the bindings port
//! (`MessageReceiverSnapshot`), keeps one minimal AgentSession per live crew
//! session bound to the inboxes it holds, and types each session's pending
//! turns through the terminal transport. Once the transport confirms the
//! turn, it writes `Receive { message, transport }` on the session's record
//! and the inbox records the message's `deliver`. The same pass settles and
//! drops open messages on the evidence the gates read.
//!
//! The person consumer (#3938) is bound each pass to every person role's
//! inbox. Nothing is typed for it: a person takes a message by acknowledging
//! or answering it through the CLI, which writes the receipt.
//!
//! Mode rules are declared here, never stored (owner ruling, 2026-10-19):
//! every message is a turn; the backend sends dead letters as Notify.
//!
//! A pass runs on events, never on a timer (#3108): a commit to the engine
//! store of a kind delivery reads, or a home heard from (its watermark gates
//! expiry); a change to an old-store record routing or conditions read; a
//! turn finishing; and one deadline wake at the earliest of a held turn's
//! hold, an open message's deadline or unbound threshold, and an awaiting
//! reply's deadline. A failed pass retries at a backoff, a finite number of
//! times, then waits for the next event.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use flotilla_address::Address;
use flotilla_core::primitives::{
    sweep::{held_turns_superseded, OpenMessages, TurnHolds},
    view::old_view,
    MessagePrimitives,
};
use flotilla_crew::reconcilers::terminal_session::{TerminalDeliveryFailure, TerminalDeliveryOutcome};
use flotilla_inbox::{InboxBackend, Rules};
use flotilla_resources::{ResourceError, TerminalSession, TerminalSessionPhase};
use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::{agent_session::EndReason, consumer::Receiver, message::Deliver},
    Transport,
};
use futures::TryStreamExt;
use tracing::{debug, warn, Instrument};

use super::{
    state::ControllerRuntimeState,
    terminal::{submit_and_confirm, EngineTurn, PendingTerminalDelivery, TerminalControllerRuntime},
};

/// Marks a terminal delivery reserved by this adapter in the shared map.
const ENGINE_BATCH: &str = "engine-message-turn";
/// The wait before retrying a failed pass, or reopening a failed watch.
const RETRY: flotilla_resources::RetryBackoff =
    flotilla_resources::RetryBackoff { initial: Duration::from_secs(1), maximum: Duration::from_secs(60) };
/// Consecutive failures retried at [`RETRY`]; past it, the next event retries a pass, and a watch
/// stays closed.
const RETRY_LIMIT: u32 = 10;

/// The mode rules the delivery adapter passes: none, so every message is a
/// turn; dead letters are Notify by the backend's own rule.
pub(crate) fn delivery_rules() -> Rules {
    Rules::default()
}

/// One live crew session and the inboxes it holds.
struct Holder {
    terminal: flotilla_resources::ResourceObject<TerminalSession>,
    session_id: String,
    consumer: Receiver,
    inboxes: BTreeSet<Address>,
}

/// A turn being typed, or typed and held: the messages it carries, for the receipts, and when
/// it was submitted, for the evidence that confirms it.
struct InFlight {
    consumer: Receiver,
    members: Vec<String>,
    submitted_at: chrono::DateTime<Utc>,
}

pub(super) struct MessageDelivery {
    pub(super) state: Arc<ControllerRuntimeState>,
    pub(super) primitives: Arc<MessagePrimitives>,
    pub(super) namespace: String,
    health: super::health::RuntimeHealth,
    bound: BTreeSet<(Address, Receiver)>,
    sessions: BTreeSet<String>,
    inflight: BTreeMap<String, InFlight>,
    /// Messages typed without confirmation, held so a pass never retypes them
    /// before the delivery hold runs out.
    held: TurnHolds,
    /// The turns those messages were typed in, until the agent's record confirms one (#3108).
    held_turns: Vec<InFlight>,
    /// Whether any turn is held: only then does a tool call or prompt submit wake a pass.
    holding: Arc<std::sync::atomic::AtomicBool>,
    /// This home's open messages, followed incrementally from the log.
    open: OpenMessages,
    /// Wakes a pass: every event source notifies it.
    wake: Arc<tokio::sync::Notify>,
}

impl MessageDelivery {
    pub(super) fn new(
        state: Arc<ControllerRuntimeState>,
        primitives: Arc<MessagePrimitives>,
        namespace: String,
        health: super::health::RuntimeHealth,
    ) -> Self {
        Self {
            state,
            primitives,
            namespace,
            health,
            bound: BTreeSet::new(),
            sessions: BTreeSet::new(),
            inflight: BTreeMap::new(),
            held: TurnHolds::default(),
            held_turns: Vec::new(),
            holding: Arc::default(),
            open: OpenMessages::default(),
            wake: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Runs a pass at once, then at each event and at the next deadline. Each pass is counted in
    /// `metrics`, as a controller's reconcile is, for load drivers (#3206).
    pub(super) async fn run(mut self, metrics: Arc<flotilla_store::load_metrics::ControllerLoadMetrics>) {
        let wake = Arc::clone(&self.wake);
        let listener: Arc<flotilla_schema_sqlite::stream::CommitListener> = {
            let wake = Arc::clone(&wake);
            let kinds = delivery_kinds();
            Arc::new(move |committed: &flotilla_schema_sqlite::stream::Committed<'_>| {
                if committed.touches(&kinds) || !committed.heard.is_empty() {
                    wake.notify_one();
                }
            })
        };
        // The store holds the listener weakly; it lives as long as this loop.
        self.primitives.service().on_commit(Arc::downgrade(&listener));
        // A held turn is confirmed by a tool call or prompt submit, which only this daemon's memory
        // holds: they wake a pass while a turn is held.
        let evidence: Arc<flotilla_resources::agent_activity::ProgressListener> = {
            let (wake, holding) = (Arc::clone(&wake), Arc::clone(&self.holding));
            Arc::new(move |_: &str| {
                if holding.load(std::sync::atomic::Ordering::SeqCst) {
                    wake.notify_one();
                }
            })
        };
        self.state.daemon.crew_progress().on_turn_evidence(Arc::downgrade(&evidence));
        let backend = self.state.daemon.resource_backend();
        let mut watches = tokio::task::JoinSet::new();
        macro_rules! watch {
            ($kind:ty) => {
                watches.spawn(watch_changes::<$kind>(backend.clone(), self.namespace.clone(), Arc::clone(&wake)));
            };
        }
        // Role routing's records (`MessageReceiverSnapshot`, the designation) and the records the
        // conditions read, as the old store's Message controller followed them.
        watch!(TerminalSession);
        watch!(flotilla_resources::Convoy);
        watch!(flotilla_resources::ConvoyEnsure);
        watch!(flotilla_resources::FleetDesignation);
        watch!(flotilla_resources::Project);
        watch!(flotilla_resources::ChangeRequest);
        watch!(flotilla_resources::Issue);
        watch!(flotilla_resources::Artifact);
        watch!(flotilla_resources::Vessel);
        let mut failures = 0_u32;
        loop {
            metrics.wakes.increment();
            metrics.queue_peak.observe(1);
            let started = tokio::time::Instant::now();
            let result = {
                let _busy = metrics.busy();
                self.pass().await
            };
            // Failed passes too, as the object controllers' histogram counts them.
            metrics.reconcile.record(started.elapsed());
            let next = match result {
                Ok(next) => {
                    failures = 0;
                    next
                }
                Err(error) => {
                    metrics.failures.increment();
                    failures = failures.saturating_add(1);
                    warn!(namespace = %self.namespace, %error, failures, "engine message delivery pass failed");
                    (failures <= RETRY_LIMIT).then(|| RETRY.delay(failures))
                }
            };
            tokio::select! {
                () = wake.notified() => {}
                () = async {
                    match next {
                        Some(wait) => tokio::time::sleep(wait).await,
                        None => std::future::pending().await,
                    }
                } => { metrics.requeues.increment(); }
                Some(_) = watches.join_next(), if !watches.is_empty() => {}
            }
        }
    }

    /// One pass; how long until the next deadline it found, if any.
    pub(super) async fn pass(&mut self) -> Result<Option<Duration>, String> {
        let wait = self.primitives.delivery_wait(&self.namespace).await?;
        self.health.report_delivery_wait(wait.clone());
        if wait.is_some() {
            return Ok(None);
        }
        let now = Timestamp::from_datetime(&Utc::now());
        // Receipts first: a turn confirmed before its session's teardown records
        // its receipt before rebind unbinds and ends that session (#3894).
        self.collect_turns().await;
        self.confirm_held_turns().await;
        let holders = self.holders(now).await?;
        let person = self.primitives.register_person(now).await?;
        let mut consumers: BTreeSet<_> =
            self.primitives.person_inboxes(&self.namespace).await?.into_iter().map(|inbox| (inbox, person.clone())).collect();
        consumers.insert(self.inspector().await?);
        self.rebind(&holders, consumers, now).await?;
        for holder in holders.values() {
            if !self.inflight.contains_key(&holder.session_id) {
                self.start_turn(holder).await?;
            }
        }
        let report = self.primitives.sweep(&self.namespace, now, &mut self.open).await?;
        if report.acted() {
            debug!(namespace = %self.namespace, ?report, "engine message sweep");
        }
        // The senders' records past their deadlines with no reply present (#3587 layer 7).
        let awaiting = Arc::clone(self.primitives.awaiting());
        let (expired, awaiting_next) = tokio::task::spawn_blocking(move || {
            let expired = awaiting.expire_due(now)?;
            Ok::<_, String>((expired, awaiting.next_deadline_after(now)?))
        })
        .await
        .map_err(|error| error.to_string())??;
        if !expired.is_empty() {
            debug!(namespace = %self.namespace, ?expired, "awaiting-reply records expired");
        }
        let until = |at: Timestamp| Duration::from_micros(u64::try_from(at.since(now)).unwrap_or(0));
        let held = self.held.next_expiry().map(|at| at.saturating_duration_since(std::time::Instant::now()));
        Ok([report.next_due.map(until), awaiting_next.map(until), held].into_iter().flatten().min())
    }

    /// This host's checkout inspector (#3108), registered as a service, and its inbox:
    /// gates at any home ask it for an inspection there, and a request that finds the inbox unbound
    /// is dropped as undeliverable, which the gate takes as a failed inspection.
    async fn inspector(&self) -> Result<(Address, Receiver), String> {
        let inbox = flotilla_core::primitives::roles::checkout_inspector(self.primitives.home())?;
        let consumer = self
            .primitives
            .register_service(flotilla_core::primitives::roles::CHECKOUT_INSPECTOR, Timestamp::from_datetime(&Utc::now()))
            .await?;
        Ok((inbox, consumer))
    }

    /// Today's role routing, through the bindings port, names which live crew
    /// session holds each role, and so which inboxes its record is bound to.
    async fn holders(&self, now: Timestamp) -> Result<BTreeMap<String, Holder>, String> {
        let roles = self.primitives.roles(&self.namespace).await?;
        let mut holders = BTreeMap::new();
        for holder in self.primitives.bindings().holders(&self.namespace, &roles).await? {
            let consumer = self.primitives.register(&holder.terminal.metadata.name, now).await?;
            holders.insert(
                holder.session_id.clone(),
                Holder { terminal: holder.terminal, session_id: holder.session_id, consumer, inboxes: holder.inboxes },
            );
        }
        Ok(holders)
    }

    /// Binds each live session's record to the inboxes it holds, and the
    /// person and service consumers to `consumers`, unbinds the rest, and
    /// ends the records of sessions no longer live.
    async fn rebind(
        &mut self,
        holders: &BTreeMap<String, Holder>,
        consumers: BTreeSet<(Address, Receiver)>,
        now: Timestamp,
    ) -> Result<(), String> {
        let mut desired: BTreeSet<_> =
            holders.values().flat_map(|holder| holder.inboxes.iter().map(|inbox| (inbox.clone(), holder.consumer.clone()))).collect();
        desired.extend(consumers);
        let inbox = self.primitives.inbox();
        for (address, consumer) in self.bound.difference(&desired).cloned().collect::<Vec<_>>() {
            inbox.unbind(&address, &consumer).await.map_err(|error| error.to_string())?;
            self.bound.remove(&(address, consumer));
        }
        // A bind waits until every observed receipt has its deliver, so one slow inbox
        // must not stall the others: bind concurrently, each bounded. The bound is
        // cancel-safe: each attempt is one blocking write under the inbox's writer
        // lock that runs to completion even when this future is dropped, so a
        // timed-out bind either took effect or never started; binding again is an
        // idempotent set insert.
        let wanted: Vec<_> = desired.difference(&self.bound).cloned().collect();
        let attempts: Vec<_> = wanted
            .iter()
            .map(|(address, consumer)| tokio::time::timeout(Duration::from_secs(5), inbox.bind(address.clone(), consumer.clone())))
            .collect();
        let results = futures::future::join_all(attempts).await;
        for ((address, consumer), result) in wanted.into_iter().zip(results) {
            match result {
                Ok(result) => {
                    result.map_err(|error| error.to_string())?;
                    self.bound.insert((address, consumer));
                }
                Err(_) => debug!(%address, consumer = %consumer.id, "binding waits for an earlier receipt's delivery"),
            }
        }
        let live: BTreeSet<_> = holders.values().map(|holder| holder.terminal.metadata.name.clone()).collect();
        let backend = self.state.daemon.resource_backend();
        for name in self.sessions.difference(&live).cloned().collect::<Vec<_>>() {
            // A turn still settling ends its session only once its receipt is recorded.
            if self.inflight.values().any(|turn| turn.consumer.id == name) {
                continue;
            }
            let terminal = match backend.using::<TerminalSession>(&self.namespace).get(&name).await {
                Ok(terminal) => Some(
                    flotilla_store::read_terminal_session(self.state.daemon.lifecycle_reads(), &terminal, None)
                        .await
                        .map_err(|error| error.to_string())?
                        .0,
                ),
                Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error.to_string()),
            };
            let reason = match terminal {
                Some(terminal) if terminal.status.as_ref().is_some_and(|status| status.phase == TerminalSessionPhase::Running) => continue,
                Some(terminal) if terminal.status.as_ref().is_some_and(|status| status.phase == TerminalSessionPhase::Stopped) => {
                    EndReason::Completed
                }
                _ => EndReason::Lost,
            };
            let sessions = Arc::clone(self.primitives.sessions());
            let ended = name.clone();
            // A SQLite write: off the async workers.
            tokio::task::spawn_blocking(move || sessions.end(&ended, reason, now)).await.map_err(|error| error.to_string())??;
            self.sessions.remove(&name);
        }
        self.sessions.extend(live);
        Ok(())
    }

    /// Settles finished turns: a confirmed one records its receipts, then
    /// each message's deliver; an unconfirmed one stays pending, held.
    async fn collect_turns(&mut self) {
        let finished: Vec<_> = {
            let mut deliveries = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned");
            self.inflight
                .keys()
                .filter(|session| {
                    deliveries
                        .get(*session)
                        .is_none_or(|delivery| delivery.message_batch.as_deref() != Some(ENGINE_BATCH) || delivery.task.is_finished())
                })
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .map(|session| {
                    let task = deliveries
                        .get(&session)
                        .filter(|delivery| delivery.message_batch.as_deref() == Some(ENGINE_BATCH))
                        .is_some()
                        .then(|| deliveries.remove(&session))
                        .flatten();
                    (session, task)
                })
                .collect()
        };
        for (session, delivery) in finished {
            let Some(turn) = self.inflight.remove(&session) else { continue };
            let outcome = match delivery {
                Some(delivery) => delivery.task.await.unwrap_or_else(|error| Err(error.to_string())),
                None => Err("terminal delivery reservation was taken".into()),
            };
            match outcome {
                // Turn first, then receipt (ADR 0064): the receipt is written only once the
                // transport confirms the turn, then the deliver. A receipt whose deliver is
                // refused or interrupted stays committed: the message is no longer pending
                // for this consumer, so it is never retyped, and the sweep records its deliver.
                Ok(TerminalDeliveryOutcome::Confirmed) => self.record_turn(&turn).await,
                // Typed, but no stable busy signal followed: held, never retyped, until the agent's
                // record confirms it or the hold runs out.
                Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed)) => {
                    let until = std::time::Instant::now()
                        + flotilla_resources::delivery_hold::DELIVERY_HOLD_FOR.to_std().expect("positive hold bound");
                    self.held.hold(turn.members.iter().cloned(), until);
                    self.held_turns.push(turn);
                }
                Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::ControllerFenceUnverifiable)) => {
                    let Some(message_id) = turn.members.first() else {
                        warn!(%session, "empty turn cannot record a controller fence delivery failure");
                        continue;
                    };
                    let terminals = self.state.daemon.resource_backend().using::<TerminalSession>(&self.namespace);
                    let patch = flotilla_resources::TerminalSessionStatusPatch::MarkControllerFenceUnverifiable {
                        message_id: message_id.clone(),
                        message: flotilla_resources::CONTROLLER_FENCE_UNVERIFIABLE_MESSAGE.into(),
                        observed_at: Utc::now(),
                    };
                    if let Err(error) = flotilla_store::apply_status_patch(&terminals, &turn.consumer.id, &patch).await {
                        warn!(%session, %error, "could not record controller fence failure");
                    }
                }
                // Never typed (the terminal was not ready in time): nothing to hold; the next pass retries.
                Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::StartupNotReady))
                | Ok(TerminalDeliveryOutcome::Pending | TerminalDeliveryOutcome::Deferred(_) | TerminalDeliveryOutcome::BatchSuperseded) => {
                }
                Err(error) => debug!(%session, %error, "engine turn delivery did not complete"),
            }
        }
        self.held.expire(std::time::Instant::now());
    }

    /// Records a confirmed turn: each message's receipt, then its deliver.
    async fn record_turn(&mut self, turn: &InFlight) {
        let inbox = self.primitives.inbox();
        for message in &turn.members {
            self.held.release(message);
            if let Err(error) = inbox.receive(&turn.consumer, message, Transport::Turn).await {
                warn!(%message, %error, "a confirmed turn's receipt was refused");
                continue;
            }
            if let Err(error) = inbox.deliver(message, Deliver { receiver: turn.consumer.clone(), transport: Transport::Turn }).await {
                warn!(%message, %error, "a confirmed turn's deliver was refused; the sweep reconciles it");
            }
        }
    }

    /// Confirms held turns the agent took up (#3108): its AgentSession began working after the
    /// turn was submitted, or this daemon saw a prompt submit or tool call after it, or, for a
    /// notify-only Codex crew, its next completion. A turn whose hold ran out, or whose messages
    /// were superseded, is no longer held.
    async fn confirm_held_turns(&mut self) {
        for turn in &mut self.held_turns {
            turn.members.retain(|message| self.held.is_held(message));
        }
        self.held_turns.retain(|turn| !turn.members.is_empty());
        let mut confirmed = Vec::new();
        for (index, turn) in self.held_turns.iter().enumerate() {
            let agent = match self.state.daemon.lifecycle_reads().agent_sessions().and_then(|reads| reads.read(&turn.consumer.id, None)) {
                Ok(agent) => agent,
                Err(error) => {
                    debug!(session = %turn.consumer.id, %error, "a held turn has no AgentSession to confirm it");
                    continue;
                }
            };
            let took_up = agent.as_ref().is_some_and(|agent| agent.took_up_after(turn.submitted_at));
            let completed = !took_up
                && match self.state.daemon.resource_backend().using::<TerminalSession>(&self.namespace).get(&turn.consumer.id).await {
                    Ok(terminal) => {
                        terminal.status.as_ref().is_some_and(|status| status.codex_completed_after(agent.as_ref(), turn.submitted_at))
                    }
                    Err(error) => {
                        debug!(session = %turn.consumer.id, %error, "a held turn's session could not be read");
                        false
                    }
                };
            if took_up || completed {
                confirmed.push(index);
            }
        }
        for index in confirmed.into_iter().rev() {
            let turn = self.held_turns.remove(index);
            tracing::info!(session = %turn.consumer.id, messages = ?turn.members, "the agent's record confirms a held turn");
            self.record_turn(&turn).await;
        }
        self.holding.store(!self.held_turns.is_empty(), std::sync::atomic::Ordering::SeqCst);
    }

    /// A due operator or supervisor turn supersedes this session's held
    /// system turns (#3455), so they are dropped, with the successor named in
    /// the engine's audit, rather than typed again once their hold runs out.
    async fn supersede_held_system_turns(
        &mut self,
        due: &[(String, flotilla_resources::MessageSpec)],
        held: Vec<String>,
        roles: &flotilla_core::primitives::roles::FleetRoles,
        records: &flotilla_core::primitives::OldStoreRecords,
    ) -> Result<(), String> {
        if held.is_empty() || !due.iter().any(|(_, spec)| spec.is_operator_or_supervisor()) {
            return Ok(());
        }
        let inbox = self.primitives.inbox();
        let mut views = Vec::new();
        for message in held {
            let Some(resource) = inbox.get_async(&message).await.map_err(|error| error.to_string())? else { continue };
            if let Ok(view) = old_view(&resource, &self.namespace, roles, records) {
                views.push((message, view.spec));
            }
        }
        let Some((successor, older)) = held_turns_superseded(due, &views) else { return Ok(()) };
        for message in older {
            tracing::info!(%message, %successor, "operator turn supersedes a held system turn");
            self.primitives.supersede(message, successor, true).await;
            self.held.release(message);
        }
        Ok(())
    }

    /// Types one session's pending turns as one turn, through the same
    /// terminal reservation every other turn takes.
    async fn start_turn(&mut self, holder: &Holder) -> Result<(), String> {
        // A fence failure is scoped to this child activation; MarkRunning on
        // an operator's resume clears it. No receipts or timed retries here.
        // `holders` was read before collect_turns persisted any newly completed
        // failures in this pass. Re-read so that stale snapshot cannot resubmit
        // a just-refused turn before the next tick.
        let terminal = self
            .state
            .daemon
            .resource_backend()
            .using::<TerminalSession>(&self.namespace)
            .get(&holder.terminal.metadata.name)
            .await
            .map_err(|error| error.to_string())?;
        if terminal
            .status
            .as_ref()
            .and_then(|status| status.degraded.as_ref())
            .is_some_and(|condition| condition.is_controller_fence_unverifiable())
        {
            return Ok(());
        }
        let inbox = self.primitives.inbox();
        let mut turns = Vec::new();
        let mut held = Vec::new();
        for address in &holder.inboxes {
            let pending: Vec<_> = inbox
                .pending(address, &holder.consumer, &delivery_rules())
                .await
                .map_err(|error| error.to_string())?
                .try_collect()
                .await
                .map_err(|error| error.to_string())?;
            for item in pending {
                if item.mode != Transport::Turn {
                    // Notify and Mute record their receipts on pull; deliver them now.
                    let deliver = Deliver { receiver: holder.consumer.clone(), transport: item.mode };
                    if let Err(error) = inbox.deliver(&item.message, deliver).await {
                        warn!(message = %item.message, %error, "a pulled notify or mute message's deliver was refused");
                    }
                    continue;
                }
                if self.held.is_held(&item.message) {
                    held.push(item.message);
                } else {
                    turns.push(item.message);
                }
            }
        }
        if turns.is_empty() {
            return Ok(());
        }
        // Framing reads old-store records: only once a turn is due.
        let roles = self.primitives.roles(&self.namespace).await?;
        let records = self.primitives.records(&self.namespace).await.map_err(|error| error.to_string())?;
        let mut members = Vec::new();
        let mut texts = Vec::new();
        let mut due = Vec::new();
        for message in turns {
            let Some(resource) = inbox.get_async(&message).await.map_err(|error| error.to_string())? else { continue };
            match old_view(&resource, &self.namespace, &roles, &*records) {
                Ok(view) => {
                    texts.push(flotilla_messaging::frame_message(&view.spec));
                    due.push((message.clone(), view.spec));
                }
                Err(error) => {
                    debug!(%message, %error, "framing an engine turn without its old spelling");
                    texts.push(format!("[from {}]\n\n{:?}", resource.spec().sender, resource.spec().body.value()));
                }
            }
            members.push(message);
        }
        self.supersede_held_system_turns(&due, held, &roles, &records).await?;
        let runtime = TerminalControllerRuntime { state: Arc::clone(&self.state) };
        let pool = runtime.pool_for_spec((&holder.terminal).into()).await?;
        let adapter = runtime.adapter_for_spec((&holder.terminal).into()).await?;
        let text = texts.join("\n\n");
        let mut deliveries = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned");
        if deliveries.contains_key(&holder.session_id) {
            return Ok(());
        }
        let session = holder.session_id.clone();
        let typed = text.clone();
        let submitted_at = self.state.daemon.clock().now();
        let stop = tokio_util::sync::CancellationToken::new();
        let teardown = stop.clone();
        let trace_spans: Vec<_> = members.iter().map(|message| flotilla_tracing::queued_span(message, "attempt")).collect();
        let primary_span = trace_spans.first().cloned().unwrap_or_else(tracing::Span::none);
        // A finished turn wakes a pass to record its receipts.
        let finished = FinishedTurn(Arc::clone(&self.wake));
        let drafts = Arc::clone(&self.state.composer_drafts);
        let task = tokio::spawn(
            async move {
                let _finished = finished;
                // Keep every member's attempt open for the shared transport call.
                let _trace_spans = trace_spans;
                for span in &_trace_spans {
                    span.in_scope(|| tracing::info!("message transport attempt"));
                }
                // Teardown stops a turn not yet typed; once typed, it runs to its confirmation.
                if teardown.is_cancelled() {
                    return Ok(TerminalDeliveryOutcome::Deferred("teardown stopped the turn before it was typed; no input sent"));
                }
                submit_and_confirm(&*pool, &drafts, adapter.as_deref(), &session, &typed).await
            }
            .instrument(primary_span),
        );
        deliveries.insert(
            holder.session_id.clone(),
            PendingTerminalDelivery {
                message_batch: Some(ENGINE_BATCH.into()),
                message: text,
                task,
                engine_turn: Some(EngineTurn::new(stop)),
            },
        );
        self.inflight.insert(holder.session_id.clone(), InFlight { consumer: holder.consumer.clone(), members, submitted_at });
        Ok(())
    }
}

/// Wakes delivery when a turn's task ends, however it ends.
struct FinishedTurn(Arc<tokio::sync::Notify>);

impl Drop for FinishedTurn {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

/// The engine kinds whose commits delivery reads: messages, the consumer
/// records that hold receipts, and the senders' awaiting records.
fn delivery_kinds() -> BTreeSet<flotilla_schema::KindCode> {
    use flotilla_schema::Kind;
    use flotilla_state_machines::{
        machines::{agent_session::AgentSession, awaiting_reply::AwaitingReply, message::Message, person::Person, service::Service},
        Resource,
    };
    BTreeSet::from([
        Resource::<Message>::CODE,
        Resource::<AgentSession>::CODE,
        Resource::<Person>::CODE,
        Resource::<Service>::CODE,
        Resource::<AwaitingReply>::CODE,
    ])
}

/// Wakes delivery at each change to `R` in the old store, local or replicated. A watch that
/// fails is reopened at a backoff, a finite number of times in a row.
async fn watch_changes<R: flotilla_resources::Resource>(
    backend: flotilla_store::ResourceBackend,
    namespace: String,
    wake: Arc<tokio::sync::Notify>,
) {
    use futures::StreamExt;
    let mut failures = 0_u32;
    loop {
        match backend.including_replicas::<R>(&namespace).watch().await {
            Ok(mut events) => {
                // A reopened watch may have missed changes: read again.
                wake.notify_one();
                while let Some(event) = events.next().await {
                    if let Err(error) = event {
                        debug!(kind = R::API_PATHS.kind, %error, "delivery watch failed");
                        break;
                    }
                    failures = 0;
                    wake.notify_one();
                }
            }
            Err(error) => debug!(kind = R::API_PATHS.kind, %error, "delivery watch did not open"),
        }
        failures = failures.saturating_add(1);
        if failures > RETRY_LIMIT {
            warn!(kind = R::API_PATHS.kind, failures, "delivery stops watching this kind; other events still wake it");
            return;
        }
        tokio::time::sleep(RETRY.delay(failures)).await;
    }
}
