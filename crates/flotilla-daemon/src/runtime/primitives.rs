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

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use flotilla_address::Address;
use flotilla_core::primitives::{
    sweep::{held_turns_superseded, OpenMessages, SweepReport, TurnHolds},
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

/// A turn being typed: the messages it carries, for the receipts.
struct InFlight {
    consumer: Receiver,
    members: Vec<String>,
}

pub(super) struct MessageDelivery {
    pub(super) state: Arc<ControllerRuntimeState>,
    pub(super) primitives: Arc<MessagePrimitives>,
    pub(super) namespace: String,
    bound: BTreeSet<(Address, Receiver)>,
    sessions: BTreeSet<String>,
    inflight: BTreeMap<String, InFlight>,
    /// Messages typed without confirmation, held so a pass never retypes them
    /// before the delivery hold runs out.
    held: TurnHolds,
    /// This home's open messages, followed incrementally from the log.
    open: OpenMessages,
}

impl MessageDelivery {
    pub(super) fn new(state: Arc<ControllerRuntimeState>, primitives: Arc<MessagePrimitives>, namespace: String) -> Self {
        Self {
            state,
            primitives,
            namespace,
            bound: BTreeSet::new(),
            sessions: BTreeSet::new(),
            inflight: BTreeMap::new(),
            held: TurnHolds::default(),
            open: OpenMessages::default(),
        }
    }

    pub(super) async fn run(mut self, interval: Duration) -> Result<(), ResourceError> {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(error) = self.pass().await {
                warn!(namespace = %self.namespace, %error, "engine message delivery pass failed");
            }
        }
    }

    pub(super) async fn pass(&mut self) -> Result<(), String> {
        let now = Timestamp::from_datetime(&Utc::now());
        // Receipts first: a turn confirmed before its session's teardown records
        // its receipt before rebind unbinds and ends that session (#3894).
        self.collect_turns().await;
        let holders = self.holders(now).await?;
        let person = self.primitives.register_person(now).await?;
        let people = self.primitives.person_inboxes(&self.namespace).await?.into_iter().map(|inbox| (inbox, person.clone()));
        self.rebind(&holders, people.collect(), now).await?;
        for holder in holders.values() {
            if !self.inflight.contains_key(&holder.session_id) {
                self.start_turn(holder).await?;
            }
        }
        let report = self.primitives.sweep(&self.namespace, now, &mut self.open).await?;
        if report != SweepReport::default() {
            debug!(namespace = %self.namespace, ?report, "engine message sweep");
        }
        // The senders' records past their deadlines with no reply present (#3587 layer 7).
        let awaiting = Arc::clone(self.primitives.awaiting());
        let expired = tokio::task::spawn_blocking(move || awaiting.expire_due(now)).await.map_err(|error| error.to_string())??;
        if !expired.is_empty() {
            debug!(namespace = %self.namespace, ?expired, "awaiting-reply records expired");
        }
        Ok(())
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
    /// person consumer to `people`, unbinds the rest, and ends the records of
    /// sessions no longer live.
    async fn rebind(
        &mut self,
        holders: &BTreeMap<String, Holder>,
        people: BTreeSet<(Address, Receiver)>,
        now: Timestamp,
    ) -> Result<(), String> {
        let mut desired: BTreeSet<_> =
            holders.values().flat_map(|holder| holder.inboxes.iter().map(|inbox| (inbox.clone(), holder.consumer.clone()))).collect();
        desired.extend(people);
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
            let reason = match backend.using::<TerminalSession>(&self.namespace).get(&name).await {
                Ok(terminal) if terminal.status.as_ref().is_some_and(|status| status.phase == TerminalSessionPhase::Running) => continue,
                Ok(terminal) if terminal.status.as_ref().is_some_and(|status| status.phase == TerminalSessionPhase::Stopped) => {
                    EndReason::Completed
                }
                Ok(_) | Err(ResourceError::NotFound { .. }) => EndReason::Lost,
                Err(error) => return Err(error.to_string()),
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
                Ok(TerminalDeliveryOutcome::Confirmed) => {
                    let inbox = self.primitives.inbox();
                    for message in &turn.members {
                        self.held.release(message);
                        if let Err(error) = inbox.receive(&turn.consumer, message, Transport::Turn).await {
                            warn!(%message, %error, "a confirmed turn's receipt was refused");
                            continue;
                        }
                        if let Err(error) =
                            inbox.deliver(message, Deliver { receiver: turn.consumer.clone(), transport: Transport::Turn }).await
                        {
                            warn!(%message, %error, "a confirmed turn's deliver was refused; the sweep reconciles it");
                        }
                    }
                }
                // Typed, but no stable busy signal followed: held, never retyped, until the hold runs out.
                Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed)) => {
                    let until = std::time::Instant::now()
                        + flotilla_resources::delivery_hold::DELIVERY_HOLD_FOR.to_std().expect("positive hold bound");
                    self.held.hold(turn.members, until);
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
        let stop = tokio_util::sync::CancellationToken::new();
        let teardown = stop.clone();
        let trace_spans: Vec<_> = members.iter().map(|message| flotilla_tracing::queued_span(message, "attempt")).collect();
        let primary_span = trace_spans.first().cloned().unwrap_or_else(tracing::Span::none);
        let task = tokio::spawn(
            async move {
                // Keep every member's attempt open for the shared transport call.
                let _trace_spans = trace_spans;
                for span in &_trace_spans {
                    span.in_scope(|| tracing::info!("message transport attempt"));
                }
                // Teardown stops a turn not yet typed; once typed, it runs to its confirmation.
                if teardown.is_cancelled() {
                    return Ok(TerminalDeliveryOutcome::Deferred("teardown stopped the turn before it was typed; no input sent"));
                }
                submit_and_confirm(&*pool, adapter.as_deref(), &session, &typed).await
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
        self.inflight.insert(holder.session_id.clone(), InFlight { consumer: holder.consumer.clone(), members });
        Ok(())
    }
}
