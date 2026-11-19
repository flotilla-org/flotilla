//! The engine path's delivery adapter (#3317), behind the `[primitives]`
//! switch: the consumer that takes crew turns from role inboxes.
//!
//! Each pass reads today's role routing through the bindings port
//! (`MessageReceiverSnapshot`), keeps one minimal AgentSession per live crew
//! launch, named by the launch, bound to the inboxes its session holds, and types each session's pending
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
//! store of a kind delivery reads, or a home heard from while an expiry
//! waits on it (its watermark gates expiry); a change to an old-store record routing or conditions read; a
//! turn finishing; and one deadline wake at the earliest of a held turn's
//! hold, an open message's deadline or unbound threshold, and an awaiting
//! reply's deadline. A failed pass retries at a backoff, a finite number of
//! times, then waits for the next event.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use super::terminal::{TerminalDeliveryFailure, TerminalDeliveryOutcome};
use chrono::Utc;
use flotilla_address::Address;
use flotilla_engine_messages::{
    sweep::{held_turns_superseded, OpenMessages, TurnHolds},
    view::turn_header,
    MessagePrimitives,
};
use flotilla_inbox::{InboxBackend, Rules};
use flotilla_messaging::TurnHeader;
use flotilla_resources::TerminalSession;
use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::{
        agent_session::EndReason,
        consumer::Receiver,
        message::{Deliver, Message, MessageRefusal},
    },
    Resource, Transport, WriteError,
};
use futures::TryStreamExt;
use tracing::{debug, info, info_span, warn, Instrument};

use super::{
    state::ControllerRuntimeState,
    terminal::{submit_and_confirm, EngineTurn, PendingTerminalDelivery, TerminalControllerRuntime},
};

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
    /// Confirmed, but a receipt was refused: the next pass retries only the receipts.
    confirmed: bool,
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
    /// The launches whose observation shows a held turn.
    showing_held: BTreeSet<String>,
    /// This home's open messages, followed incrementally from the log.
    open: OpenMessages,
    /// Wakes a pass: every event source notifies it.
    wake: Arc<tokio::sync::Notify>,
    /// The homes the last sweep's expiries wait to hear from: only hearing from one wakes a pass.
    awaiting_heard: Arc<std::sync::Mutex<BTreeSet<String>>>,
}

/// A graceful stop's request to message delivery, and delivery's answer once it has settled its
/// turns (d8): the runtime asks before it aborts its tasks.
#[derive(Debug)]
pub(crate) struct DeliveryStop {
    requested: tokio::sync::watch::Sender<bool>,
    settled: tokio::sync::watch::Sender<bool>,
    running: std::sync::atomic::AtomicBool,
}

impl Default for DeliveryStop {
    fn default() -> Self {
        Self {
            requested: tokio::sync::watch::Sender::new(false),
            settled: tokio::sync::watch::Sender::new(false),
            running: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl DeliveryStop {
    /// Asks delivery to settle its turns and waits at most `within` for its answer: at once when
    /// no delivery runs. Whether it answered.
    pub(crate) async fn settle(&self, within: Duration) -> bool {
        self.requested.send_replace(true);
        if !self.running.load(std::sync::atomic::Ordering::SeqCst) {
            return true;
        }
        let mut settled = self.settled.subscribe();
        let answered = tokio::time::timeout(within, async { settled.wait_for(|settled| *settled).await.is_ok() }).await;
        answered.unwrap_or(false)
    }
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
            showing_held: BTreeSet::new(),
            open: OpenMessages::default(),
            wake: Arc::new(tokio::sync::Notify::new()),
            awaiting_heard: Arc::default(),
        }
    }

    /// Runs a pass at once, then at each event and at the next deadline. Each pass is counted in
    /// `metrics`, as a controller's reconcile is, for load drivers (#3206).
    pub(super) async fn run(mut self, metrics: Arc<flotilla_store::load_metrics::ControllerLoadMetrics>) {
        let wake = Arc::clone(&self.wake);
        let listener: Arc<flotilla_schema_sqlite::stream::CommitListener> = {
            let (wake, awaiting) = (Arc::clone(&wake), Arc::clone(&self.awaiting_heard));
            let kinds = delivery_kinds();
            Arc::new(move |committed: &flotilla_schema_sqlite::stream::Committed<'_>| {
                if committed.touches(&kinds) || committed.heard.iter().any(|home| awaited(&awaiting, home)) {
                    wake.notify_one();
                }
            })
        };
        // A home heard from without a write wakes a pass only while an expiry waits on it.
        let heard: Arc<flotilla_schema_sqlite::stream::HeardListener> = {
            let (wake, awaiting) = (Arc::clone(&wake), Arc::clone(&self.awaiting_heard));
            Arc::new(move |home: &str| {
                if awaited(&awaiting, home) {
                    wake.notify_one();
                }
            })
        };
        // The store holds the listeners weakly; they live as long as this loop.
        self.primitives.service().on_commit(Arc::downgrade(&listener));
        self.primitives.service().on_heard(Arc::downgrade(&heard));
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
                watches.spawn(flotilla_schema::reads::inherit(watch_changes::<$kind>(
                    backend.clone(),
                    self.namespace.clone(),
                    Arc::clone(&wake),
                )));
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
        let stops = Arc::clone(&self.state.delivery_stop);
        stops.running.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut stop = stops.requested.subscribe();
        let mut failures = 0_u32;
        loop {
            metrics.wakes.increment();
            metrics.woke("pass");
            metrics.queue_peak.observe(1);
            let started = tokio::time::Instant::now();
            let result = {
                let _busy = metrics.busy();
                self.pass().instrument(info_span!("message_delivery.pass")).await
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
            // A stop is taken before any other wake: no pass starts a turn after it.
            tokio::select! {
                biased;
                Ok(()) = async { stop.wait_for(|requested| *requested).await.map(|_| ()) } => {
                    self.settle_turns().await;
                    stops.settled.send_replace(true);
                    return;
                }
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
        let wait = self.primitives.delivery_wait(&self.namespace).instrument(info_span!("message_delivery.delivery_wait")).await?;
        self.health.report_delivery_wait(wait.clone());
        if wait.is_some() {
            return Ok(None);
        }
        let now = Timestamp::from_datetime(&Utc::now());
        // Receipts first: a turn confirmed before its session's teardown records
        // its receipt before rebind unbinds and ends that session (#3894).
        self.collect_turns(false).instrument(info_span!("message_delivery.collect_turns")).await;
        self.confirm_held_turns().instrument(info_span!("message_delivery.confirm_held_turns")).await;
        let holders = self.holders(now).instrument(info_span!("message_delivery.holders")).await?;
        let person = self.primitives.register_person(now).instrument(info_span!("message_delivery.register_person")).await?;
        let mut consumers: BTreeSet<_> = self
            .primitives
            .person_inboxes(&self.namespace)
            .instrument(info_span!("message_delivery.person_inboxes"))
            .await?
            .into_iter()
            .map(|inbox| (inbox, person.clone()))
            .collect();
        consumers.insert(self.inspector().instrument(info_span!("message_delivery.inspector")).await?);
        self.rebind(&holders, consumers, now).instrument(info_span!("message_delivery.rebind", holders = holders.len())).await?;
        let starts = info_span!("message_delivery.start_turns", started = tracing::field::Empty);
        async {
            let mut started = 0_usize;
            for holder in holders.values() {
                if !self.inflight.contains_key(&holder.session_id) {
                    self.start_turn(holder).instrument(info_span!("message_delivery.start_turn", session = %holder.session_id)).await?;
                    started += 1;
                }
            }
            tracing::Span::current().record("started", started);
            Ok::<_, String>(())
        }
        .instrument(starts)
        .await?;
        let report = self.primitives.sweep(&self.namespace, now, &mut self.open).instrument(info_span!("message_delivery.sweep")).await?;
        self.awaiting_heard.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone_from(&report.awaiting_heard);
        if report.acted() {
            debug!(namespace = %self.namespace, ?report, "engine message sweep");
        }
        // The senders' records past their deadlines with no reply present (#3587 layer 7).
        let awaiting = Arc::clone(self.primitives.awaiting());
        let (expired, awaiting_next) = tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || {
            let expired = awaiting.expire_due(now)?;
            Ok::<_, String>((expired, awaiting.next_deadline_after(now)?))
        }))
        .instrument(info_span!("message_delivery.expire_due"))
        .await
        .map_err(|error| error.to_string())??;
        if !expired.is_empty() {
            debug!(namespace = %self.namespace, ?expired, "awaiting-reply records expired");
        }
        self.show_held_turns();
        let until = |at: Timestamp| Duration::from_micros(u64::try_from(at.since(now)).unwrap_or(0));
        let held = self.held.next_expiry().map(|at| at.saturating_duration_since(std::time::Instant::now()));
        Ok([report.next_due.map(until), awaiting_next.map(until), held].into_iter().flatten().min())
    }

    /// This host's checkout inspector (#3108), registered as a service, and its inbox:
    /// gates at any home ask it for an inspection there, and a request that finds the inbox unbound
    /// is dropped as undeliverable, which the gate takes as a failed inspection.
    async fn inspector(&self) -> Result<(Address, Receiver), String> {
        let inbox = flotilla_engine_messages::roles::checkout_inspector(self.primitives.home())?;
        let consumer = self
            .primitives
            .register_service(flotilla_engine_messages::roles::CHECKOUT_INSPECTOR, Timestamp::from_datetime(&Utc::now()))
            .await?;
        Ok((inbox, consumer))
    }

    /// Today's role routing, through the bindings port, names which live crew
    /// session holds each role, and so which inboxes its record is bound to.
    async fn holders(&self, now: Timestamp) -> Result<BTreeMap<String, Holder>, String> {
        let roles = self.primitives.roles(&self.namespace).instrument(info_span!("message_delivery.roles")).await?;
        let mut holders = BTreeMap::new();
        let bound = self.primitives.bindings().holders(&self.namespace, &roles).instrument(info_span!("message_delivery.bindings")).await?;
        for holder in bound {
            // One AgentSession per launch, named by it (#3108).
            let Some(launch) = flotilla_resources::terminal_records::launch_of(&holder.terminal) else {
                debug!(session = %holder.terminal.metadata.name, "a live session's view names no launch");
                continue;
            };
            let consumer = self.primitives.register(launch, now).instrument(info_span!("message_delivery.register")).await?;
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
        let live: BTreeSet<_> = holders.values().map(|holder| holder.consumer.id.clone()).collect();
        for name in self.sessions.difference(&live).cloned().collect::<Vec<_>>() {
            // A turn still settling ends its session only once its receipt is recorded.
            if self.inflight.values().any(|turn| turn.consumer.id == name) {
                continue;
            }
            let reads = self.state.daemon.lifecycle_reads();
            let launch = name.clone();
            // A store read: off the async workers.
            let terminal = tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || reads.terminals()?.get(&launch, None)))
                .await
                .map_err(|error| error.to_string())??;
            let reason = match terminal {
                Some(launch) if launch.is_running() => continue,
                // Stopped, as its view reads: released, or its process exited.
                Some(launch) if launch.exited() || matches!(launch.reading, flotilla_protocol::LifecycleReading::Released) => {
                    EndReason::Completed
                }
                _ => EndReason::Lost,
            };
            let sessions = Arc::clone(self.primitives.sessions());
            let ended = name.clone();
            // A SQLite write: off the async workers.
            tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || sessions.end(&ended, reason, now)))
                .await
                .map_err(|error| error.to_string())??;
            self.sessions.remove(&name);
        }
        self.sessions.extend(live);
        Ok(())
    }

    /// Settles finished turns, or with `settle` every turn once it finishes: a confirmed one
    /// records its receipts, then each message's deliver; an unconfirmed one stays pending, held.
    async fn collect_turns(&mut self, settle: bool) {
        let finished: Vec<_> = {
            let mut deliveries = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned");
            self.inflight
                .keys()
                .filter(|session| settle || deliveries.get(*session).is_none_or(|delivery| delivery.task.is_finished()))
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .map(|session| {
                    let task = deliveries.remove(&session);
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
                Ok(TerminalDeliveryOutcome::Confirmed) => self.record_turn(turn).await,
                // Typed, but no stable busy signal followed: held, never retyped, until the agent's
                // record confirms it or the hold runs out.
                Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed)) => {
                    let until = std::time::Instant::now()
                        + flotilla_resources::delivery_hold::DELIVERY_HOLD_FOR.to_std().expect("positive hold bound");
                    info!(%session, messages = ?turn.members, submitted_at = %turn.submitted_at, "a turn typed without confirmation is held");
                    self.held.hold(turn.members.iter().cloned(), until);
                    self.held_turns.push(turn);
                }
                // Typed before, by a daemon that forgot it across a restart: held from when the
                // pool first accepted it, as if this daemon had seen it typed then.
                Ok(TerminalDeliveryOutcome::AlreadyTyped { at_unix_ms }) => {
                    let typed_at = i64::try_from(at_unix_ms).ok().and_then(chrono::DateTime::<Utc>::from_timestamp_millis);
                    let mut turn = turn;
                    turn.submitted_at = typed_at.unwrap_or(turn.submitted_at);
                    let left = (turn.submitted_at + flotilla_resources::delivery_hold::DELIVERY_HOLD_FOR) - self.state.daemon.clock().now();
                    let until = std::time::Instant::now() + left.to_std().unwrap_or_default();
                    info!(%session, messages = ?turn.members, submitted_at = %turn.submitted_at, "a turn typed before this daemon started is held");
                    self.held.hold(turn.members.iter().cloned(), until);
                    self.held_turns.push(turn);
                }
                // Never typed (cleat refused it): nothing to hold; the next pass retries.
                Ok(TerminalDeliveryOutcome::Deferred(_)) => {}
                Err(error) => debug!(%session, %error, "engine turn delivery did not complete"),
            }
        }
        for message in self.held.expire(std::time::Instant::now()) {
            warn!(message_id = %message, "a held turn's hold ran out unconfirmed; it may be typed again");
        }
    }

    /// A graceful stop (d8): waits for each turn being typed to settle, then records the receipts
    /// of those confirmed and of held turns the agent's record confirms, so a restart does not
    /// leave a turn the agent already took to a hold.
    async fn settle_turns(&mut self) {
        self.collect_turns(true).await;
        self.confirm_held_turns().await;
    }

    /// Shows on each launch's observation when the turn it holds unconfirmed was submitted, and
    /// clears it once no turn is held there: the session view explains the hold (#3108).
    fn show_held_turns(&mut self) {
        let mut held = BTreeMap::<String, chrono::DateTime<Utc>>::new();
        for turn in self.held_turns.iter().filter(|turn| !turn.confirmed && turn.members.iter().any(|message| self.held.is_held(message))) {
            let since = held.entry(turn.consumer.id.clone()).or_insert(turn.submitted_at);
            *since = (*since).min(turn.submitted_at);
        }
        let reads = self.state.daemon.lifecycle_reads();
        let terminals = match reads.terminals() {
            Ok(terminals) => terminals,
            Err(error) => {
                debug!(%error, "held turns cannot be shown without the terminal sessions");
                return;
            }
        };
        // A held turn was typed into a running launch. One that ended meanwhile was forgotten, and
        // is observed again only while its turn is held: clearing the hold forgets it again.
        for launch in self.showing_held.iter().filter(|launch| !held.contains_key(*launch)) {
            let observations = terminals.observations();
            observations.amend(launch, |observation| observation.delivery_unconfirmed_since = None);
            if observations.get(launch).is_some_and(|observation| observation == Default::default()) {
                observations.forget(launch);
            }
        }
        for (launch, since) in &held {
            terminals.observations().update(launch, |observation| observation.delivery_unconfirmed_since = Some(*since));
        }
        self.showing_held = held.into_keys().collect();
    }

    /// Records a confirmed turn: each message's receipt, then its deliver. A message whose receipt
    /// is refused stays held, never retyped: the turn is kept as confirmed, so the next pass retries
    /// only its receipt, until the hold runs out. A message that ended meanwhile (superseded,
    /// withdrawn, expired) can never take a receipt, so it is released, not retried.
    async fn record_turn(&mut self, mut turn: InFlight) {
        let inbox = self.primitives.inbox();
        let mut refused = Vec::new();
        for message in std::mem::take(&mut turn.members) {
            match inbox.receive(&turn.consumer, &message, Transport::Turn).await {
                Ok(()) => {}
                Err(flotilla_inbox::Error::Message(WriteError::Refused(MessageRefusal::Ended))) => {
                    info!(message_id = %message, "a confirmed turn's message ended before its receipt; nothing is recorded or retried");
                    self.held.release(&message);
                    continue;
                }
                Err(error) => {
                    warn!(message_id = %message, %error, "a confirmed turn's receipt was refused; it is retried, never retyped");
                    refused.push(message);
                    continue;
                }
            }
            self.held.release(&message);
            if let Err(error) = inbox.deliver(&message, Deliver { receiver: turn.consumer.clone(), transport: Transport::Turn }).await {
                warn!(message_id = %message, %error, "a confirmed turn's deliver was refused; the sweep reconciles it");
            }
        }
        if refused.is_empty() {
            return;
        }
        // A turn confirmed at once was never held: its hold starts now.
        let until = std::time::Instant::now() + flotilla_resources::delivery_hold::DELIVERY_HOLD_FOR.to_std().expect("positive hold bound");
        self.held.hold(refused.iter().filter(|message| !self.held.is_held(message)).cloned().collect::<Vec<_>>(), until);
        turn.members = refused;
        turn.confirmed = true;
        self.held_turns.push(turn);
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
            if turn.confirmed {
                confirmed.push(index);
                continue;
            }
            let agent = match self.state.daemon.lifecycle_reads().agent_sessions().and_then(|reads| reads.read(&turn.consumer.id, None)) {
                Ok(agent) => agent,
                Err(error) => {
                    debug!(session = %turn.consumer.id, %error, "a held turn has no AgentSession to confirm it");
                    continue;
                }
            };
            let took_up = agent.as_ref().is_some_and(|agent| agent.took_up_after(turn.submitted_at));
            // The session's view, while it still shows the launch the turn was typed in.
            let session = flotilla_resources::terminal_records::launched(&turn.consumer.id);
            let completed = !took_up
                && match self.state.daemon.resource_backend().using::<TerminalSession>(&self.namespace).get(session).await {
                    Ok(terminal) => {
                        flotilla_resources::terminal_records::launch_of(&terminal) == Some(turn.consumer.id.as_str())
                            && terminal
                                .status
                                .as_ref()
                                .is_some_and(|status| status.codex_completed_after(agent.as_ref(), turn.submitted_at))
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
        let confirmed: Vec<_> = confirmed.into_iter().rev().map(|index| self.held_turns.remove(index)).collect();
        for turn in confirmed {
            if !turn.confirmed {
                tracing::info!(session = %turn.consumer.id, messages = ?turn.members, "the agent's record confirms a held turn");
            }
            self.record_turn(turn).await;
        }
        // Only an unconfirmed turn waits on the agent's evidence.
        self.holding.store(self.held_turns.iter().any(|turn| !turn.confirmed), std::sync::atomic::Ordering::SeqCst);
    }

    /// A due operator or supervisor turn supersedes this session's held
    /// system turns (#3455), so they are dropped, with the successor named in
    /// the engine's audit, rather than typed again once their hold runs out.
    async fn supersede_held_system_turns(
        &mut self,
        due: &[(String, TurnHeader)],
        held: Vec<String>,
        roles: &flotilla_engine_messages::roles::FleetRoles,
        records: &mut flotilla_engine_messages::OldStoreRecords,
    ) -> Result<(), String> {
        if held.is_empty() || !due.iter().any(|(_, turn)| turn.is_operator_or_supervisor()) {
            return Ok(());
        }
        let mut loaded = Vec::new();
        for message in held {
            if let Some(resource) = self.primitives.message(&message).await.map_err(|error| error.to_string())? {
                loaded.push((message, resource));
            }
        }
        let headers = self.headers(&loaded, roles, records).await?;
        let views: Vec<_> = loaded.into_iter().zip(headers).filter_map(|((message, _), header)| Some((message, header.ok()?))).collect();
        let Some((successor, older)) = held_turns_superseded(due, &views) else { return Ok(()) };
        for message in older {
            tracing::info!(message_id = %message, %successor, "operator turn supersedes a held system turn");
            self.primitives.supersede(message, successor, true).await;
            self.held.release(message);
        }
        Ok(())
    }

    /// Each of `messages`' turn headers, spelled on the records they name, read into `records`.
    async fn headers(
        &self,
        messages: &[(String, Resource<Message>)],
        roles: &flotilla_engine_messages::roles::FleetRoles,
        records: &mut flotilla_engine_messages::OldStoreRecords,
    ) -> Result<Vec<Result<TurnHeader, String>>, String> {
        self.primitives
            .spell(records, |records| messages.iter().map(|(_, resource)| turn_header(resource, roles, records)).collect())
            .await
            .map_err(|error| error.to_string())
    }

    /// Types one session's pending turns as one turn, through the same
    /// terminal reservation every other turn takes.
    async fn start_turn(&mut self, holder: &Holder) -> Result<(), String> {
        let inbox = self.primitives.inbox();
        let mut turns = Vec::new();
        let mut held = Vec::new();
        for address in &holder.inboxes {
            let pending: Vec<_> = async {
                inbox
                    .pending(address, &holder.consumer, &delivery_rules())
                    .await
                    .map_err(|error| error.to_string())?
                    .try_collect()
                    .await
                    .map_err(|error| error.to_string())
            }
            .instrument(info_span!("message_delivery.pending"))
            .await?;
            for item in pending {
                if item.mode != Transport::Turn {
                    // Notify and Mute record their receipts on pull; deliver them now.
                    let deliver = Deliver { receiver: holder.consumer.clone(), transport: item.mode };
                    if let Err(error) = inbox.deliver(&item.message, deliver).await {
                        warn!(message_id = %item.message, %error, "a pulled notify or mute message's deliver was refused");
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
        // Framing reads the records the turns' headers name: only once a turn is due.
        let mut records = flotilla_engine_messages::OldStoreRecords::empty(&self.namespace);
        let (roles, loaded, headers) = async {
            let roles = self.primitives.roles(&self.namespace).await?;
            let mut loaded = Vec::new();
            for message in turns {
                if let Some(resource) = self.primitives.message(&message).await.map_err(|error| error.to_string())? {
                    loaded.push((message, resource));
                }
            }
            let headers = self.headers(&loaded, &roles, &mut records).await?;
            Ok::<_, String>((roles, loaded, headers))
        }
        .instrument(info_span!("message_delivery.frame"))
        .await?;
        let mut members = Vec::new();
        let mut texts = Vec::new();
        let mut due = Vec::new();
        for ((message, resource), header) in loaded.into_iter().zip(headers) {
            match header {
                Ok(header) => {
                    texts.push(flotilla_messaging::frame_turn(&header));
                    due.push((message.clone(), header));
                }
                Err(error) => {
                    debug!(message_id = %message, %error, "framing an engine turn without its old spelling");
                    texts.push(format!("[from {}]\n\n{:?}", resource.spec().sender, resource.spec().body.value()));
                }
            }
            members.push(message);
        }
        self.supersede_held_system_turns(&due, held, &roles, &mut records).await?;
        let runtime = TerminalControllerRuntime { state: Arc::clone(&self.state) };
        let (pool, adapter) = async {
            Ok::<_, String>((
                runtime.pool_for_spec((&holder.terminal).into()).await?,
                runtime.adapter_for_spec((&holder.terminal).into()).await?,
            ))
        }
        .instrument(info_span!("message_delivery.transport"))
        .await?;
        let text = texts.join("\n\n");
        let mut deliveries = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned");
        if deliveries.contains_key(&holder.session_id) {
            return Ok(());
        }
        let session = holder.session_id.clone();
        let typed = text.clone();
        let submitted_at = self.state.daemon.clock().now();
        let typed_members = members.clone();
        let stop = tokio_util::sync::CancellationToken::new();
        let teardown = stop.clone();
        let trace_spans: Vec<_> = members.iter().map(|message| flotilla_tracing::queued_span(message, "attempt")).collect();
        let primary_span = trace_spans.first().cloned().unwrap_or_else(tracing::Span::none);
        // A finished turn wakes a pass to record its receipts.
        let finished = FinishedTurn(Arc::clone(&self.wake));
        let drafts = Arc::clone(&self.state.composer_drafts);
        let task = tokio::spawn(flotilla_schema::reads::inherit(
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
                type_turn(&*pool, &drafts, adapter.as_deref(), &session, &typed, &typed_members, submitted_at).await
            }
            .instrument(primary_span),
        ));
        deliveries.insert(holder.session_id.clone(), PendingTerminalDelivery { task, engine_turn: EngineTurn::new(stop) });
        self.inflight
            .insert(holder.session_id.clone(), InFlight { consumer: holder.consumer.clone(), members, submitted_at, confirmed: false });
        Ok(())
    }
}

/// How many times one pass retypes a turn judged lost under a new key before it gives up.
const RETYPE_LIMIT: usize = 4;

/// A turn's key: the identity of the messages it carries, as a pool records it (1 to 256 bytes).
pub(super) fn turn_key(members: &[String]) -> String {
    use sha2::Digest;
    let mut digest = sha2::Sha256::new();
    for message in members {
        digest.update(message.as_bytes());
        digest.update(b"\n");
    }
    format!("turn:{:x}", digest.finalize())
}

/// Types the turn `text`, keyed by the identity of its `messages`, into `session` at `now`. The pool
/// writes nothing for a key it already accepted: a turn this daemon forgot it typed, across a
/// restart, is not typed twice. One typed so long ago that its hold has run out was judged lost;
/// it is retyped under a key naming when it was first accepted. Either way the repeat is logged at
/// info, naming the turn's `messages` and the session, since the pool itself writes nothing for it.
pub(super) async fn type_turn(
    pool: &dyn flotilla_terminal::TerminalPool,
    drafts: &flotilla_terminal::drafts::ComposerDrafts,
    adapter: Option<&dyn flotilla_agent::adapter::AgentAdapter>,
    session: &str,
    text: &str,
    messages: &[String],
    now: chrono::DateTime<Utc>,
) -> Result<TerminalDeliveryOutcome, String> {
    let identity = turn_key(messages);
    let mut key = identity.clone();
    for _ in 0..RETYPE_LIMIT {
        let outcome = submit_and_confirm(pool, drafts, adapter, session, text, &key).await?;
        let TerminalDeliveryOutcome::AlreadyTyped { at_unix_ms } = outcome else { return Ok(outcome) };
        let first_accepted = i64::try_from(at_unix_ms).ok().and_then(chrono::DateTime::<Utc>::from_timestamp_millis);
        if !hold_ran_out(at_unix_ms, now) {
            info!(
                %session, ?messages, %key, ?first_accepted,
                "a keyed turn repeat: the pool accepted this turn before and wrote nothing now"
            );
            return Ok(outcome);
        }
        info!(
            %session, ?messages, %key, ?first_accepted,
            "a keyed turn repeat whose hold ran out: judged lost, it is retyped under a new key"
        );
        key = format!("{identity}@{at_unix_ms}");
    }
    Err(format!("turn {identity} was judged lost {RETYPE_LIMIT} times over; it is left pending"))
}

/// Whether a turn first accepted at `at_unix_ms` has outlived its hold by `now`: it was typed,
/// never confirmed, and judged lost.
fn hold_ran_out(at_unix_ms: u64, now: chrono::DateTime<Utc>) -> bool {
    let at = i64::try_from(at_unix_ms).ok().and_then(chrono::DateTime::<Utc>::from_timestamp_millis);
    at.is_some_and(|at| at + flotilla_resources::delivery_hold::DELIVERY_HOLD_FOR <= now)
}

/// Wakes delivery when a turn's task ends, however it ends.
struct FinishedTurn(Arc<tokio::sync::Notify>);

impl Drop for FinishedTurn {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

/// Whether an expiry waits to hear from `home`.
fn awaited(awaiting: &std::sync::Mutex<BTreeSet<String>>, home: &str) -> bool {
    awaiting.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(home)
}

/// The engine kinds whose commits delivery reads: messages, the consumer
/// records that hold receipts, the senders' awaiting records, and convoys,
/// whose ends withdraw the messages pending to their seats.
fn delivery_kinds() -> BTreeSet<flotilla_schema::KindCode> {
    use flotilla_schema::Kind;
    use flotilla_state_machines::{
        machines::{
            agent_session::AgentSession, awaiting_reply::AwaitingReply, convoy::Convoy, message::Message, person::Person, service::Service,
        },
        Resource,
    };
    BTreeSet::from([
        Resource::<Convoy>::CODE,
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
