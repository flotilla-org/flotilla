//! Quint ITF trace replay through the engine (#3220, #3319).
//!
//! `specs/primitives/check.sh` simulates the six scenarios in
//! `specs/primitives/scenarios/` and writes one ITF file per trace; nothing
//! here is a stored fixture. Each trace drives the real single writers
//! ([`Home::apply`], [`Home::record_attempt`] and [`Home::record_receipt`])
//! and the placement [`Ledger`].
//!
//! The model only takes enabled steps: a machine operation in a trace is one
//! the spec accepted, and every other step (`unchanged`, a tick, a stream or
//! binding change) changes only the world. So the replay asserts that Rust
//! accepts every machine operation and applies nothing on a world step. World
//! variables become the typed reads in each machine's `Memory*World`, rebuilt
//! from the state the step starts in.
//!
//! After every step the replay compares state values. A machine's state is a
//! value, as in the specs, so each Rust resource's [`Resource::state`] (with
//! its immutable spec where the model's state carries request data, and its
//! attempts where the model's `history` is attempts) is rendered in the
//! spec's ITF shape and compared with the model's store, slot for slot.
//! Nothing reads edge events to answer what a state is; the message replay
//! reads them only to audit that each accepted edge left one event and that
//! nothing delivered twice.
//!
//! Model integers name resources (`1` is the resource named "1"), model ticks
//! are [`UNIT`] microseconds, and opaque strings map to themselves. The
//! remaining differences, each as small as the engine allows:
//!
//! - **Message envelope without a deadline.** The model's envelope keeps a
//!   `deadline` integer even when `hasDeadline` is false; Rust's
//!   `deadline: Option<Timestamp>` has none. Both sides compare it as 0 then.
//! - **Message drop `receiver`.** The model's drop keeps the receiver's
//!   session only; Rust keeps the whole [`message::Receiver`]. The rendering
//!   keeps its session (0 for none), and Awaiting compares session and host.
//! - **Message drop `bound` and `receipt`.** Rust stores the applied
//!   watermark only for an expiry that a bound consumer constrained, which is
//!   exactly the spec's `bound`, so `bound` renders as "a watermark is
//!   recorded". The spec's `receipt` is false on every drop it can reach (an
//!   undeliverable drop is refused with a receipt), and Rust has no field.
//! - **Ensure `history`.** The model's history lists the fulfiller each
//!   failure named; Rust's history is the Ensure's attempt records, and the
//!   replay records that fulfiller as each failure's cause detail. Rust never
//!   refuses `HistoryFull` (attempts are unbounded), and the model never takes
//!   a step it refuses, so this changes no replayed step.
//! - **Vessels.** A vessel is a placement at the [`Ledger`] plus a lifecycle
//!   resource. The ledger forgets a vessel when it returns the lease, so the
//!   harness keeps each returned lease to render the model's `Gone(placement)`.
//! - **`placedOn`.** A lease holds only its request and host. Per the owner's
//!   ruling the harness keeps what each placement read ([`Ledger::evidence`] at
//!   place time), compares it with the scenario's `placedOn`, and checks the
//!   `queuePriority` rule over it. The model's waiting set includes the placed
//!   request itself; the ledger's excludes it, so the harness adds it back.
//!
//! To also cover what the model refuses, each reached state is probed: an
//! ended resource must refuse every edge and record and stay unchanged, and
//! every edge the Rust gates would accept (a dry-run step through
//! [`MachineDef::step`](flotilla_state_machines::MachineDef::step), or a
//! placement on a cloned ledger) must keep the scenario's safety properties.
//! The scenarios' negative picks (`auth`, `seen` or `matches` false, an empty
//! reason) are never taken steps, so they are probed too: Rust must refuse them.

#[path = "common/itf.rs"]
mod itf;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::Path,
    time::Instant,
};

use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::{
        agent_session::{
            AgentSession, AgentSessionRefusal, AgentSessionState, Attach, Channel, Detach, End, EndReason, MemoryAgentSessionWorld,
        },
        convoy::{
            self, Convoy, ConvoyEdge, ConvoyId, ConvoyRequest, ConvoyState, MemoryConvoyWorld, Priority, Promise, PromiseId, PromiseState,
            Quota, VesselClass,
        },
        ensure::{self, Ensure, EnsureSpec, EnsureState, FulfillerPhase, FulfillerStatus, MemoryEnsureWorld, UnbindReason},
        lifecycle::{self, Lifecycle, LifecycleEdge, LifecycleKind, LifecycleRefusal, LifecycleState, MemoryLifecycleWorld, Observed},
        message::{
            self, DropReason, Expectation, InboxBinding, MemoryMessageWorld, Message, MessageEdge, MessageEdgeKind, MessageSpec,
            MessageState,
        },
    },
    placement::{self, Dimension, Evidence, Host, Lease, Ledger, LedgerError, Units, VesselId},
    Attempt, AttemptPolicy, Cause, CauseCategory, Edge, Ended, Handle, Home, Kind, Machine, Outcome, Receipt, Ref, Resource, Transport,
    WriteError,
};
use itf::{address, at, boolean, int, list, map_of, number, pairs, payload, set, set_of, show, slot, tag, text, variant};
use serde_json::{json, Value};

/// Microseconds per model clock tick (ten minutes).
const UNIT: i64 = 600_000_000;
/// The inbox every replayed message and Ensure names.
const INBOX: &str = "inbox";
/// The one message (id 1) of the message scenarios, and the one Ensure (id 1)
/// of `ensure_bind`: every resource is named by its model id.
const ONE: &str = "1";

fn moment(ticks: i64) -> Timestamp {
    Timestamp::from_micros(ticks * UNIT)
}

/// A replayed time in model ticks. Every time the replay supplies is whole.
fn ticks(time: Timestamp) -> i64 {
    let micros = time.micros();
    assert_eq!(micros % UNIT, 0, "{time} is not a whole model tick");
    micros / UNIT
}

fn small(v: &Value) -> u32 {
    u32::try_from(number(v)).expect("small non-negative model integer")
}

/// A resource name as the model integer it stands for.
fn id_of(name: &str) -> i64 {
    name.parse().unwrap_or_else(|_| panic!("resource name {name} is not a model id"))
}

fn cause(detail: &str) -> Cause {
    Cause::new(CauseCategory::Transient, detail)
}

/// The authorised party's reference; its value is the model's `authorised` flag.
fn owner<K: Kind>() -> Ref<K> {
    Ref::new("owner")
}

/// A sum value without a payload, as ITF encodes it.
fn constant(name: &str) -> Value {
    variant(name, itf::unit())
}

// ---------------------------------------------------------------------------
// Traces

/// One trace file's states.
struct Trace<'a> {
    file: &'a str,
    states: &'a [Value],
}

/// One model step: the state it starts in and the state it reaches.
#[derive(Clone, Copy)]
struct Step<'a> {
    file: &'a str,
    index: usize,
    before: &'a Value,
    after: &'a Value,
}

impl<'a> Trace<'a> {
    fn steps(&self) -> impl Iterator<Item = Step<'a>> + 'a {
        let file = self.file;
        self.states.windows(2).enumerate().map(move |(i, pair)| Step { file, index: i + 1, before: &pair[0], after: &pair[1] })
    }
}

impl<'a> Step<'a> {
    fn action(&self) -> &'a str {
        text(&self.after["mbt::actionTaken"])
    }

    fn pick(&self, key: &str) -> &'a Value {
        let picked = &self.after["mbt::nondetPicks"][key];
        assert_eq!(tag(picked), "Some", "{self}: no pick for {key}");
        payload(picked)
    }

    fn chosen(&self, key: &str) -> i64 {
        number(self.pick(key))
    }

    fn yes(&self, key: &str) -> bool {
        boolean(self.pick(key))
    }

    /// A model time for scenarios without a clock: one tick per step.
    fn tick(&self) -> Timestamp {
        moment(i64::try_from(self.index).expect("step index"))
    }
}

impl fmt::Display for Step<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} step {} ({})", self.file, self.index, self.action())
    }
}

/// The imported store whose qualified name ends in `suffix`, e.g. `::convoys`.
fn store<'v>(state: &'v Value, suffix: &str) -> &'v Value {
    let mut found = state.as_object().expect("ITF state").iter().filter(|(name, _)| name.ends_with(suffix));
    let (_, value) = found.next().unwrap_or_else(|| panic!("no store ending in {suffix}"));
    assert!(found.next().is_none(), "one store ending in {suffix}");
    value
}

fn slot_at<'v>(state: &'v Value, suffix: &str, id: i64) -> Option<&'v Value> {
    slot(at(store(state, suffix), id))
}

fn entries<'v>(state: &'v Value, suffix: &str) -> impl Iterator<Item = (i64, Option<&'v Value>)> {
    pairs(store(state, suffix)).iter().map(|pair| (number(&pair[0]), slot(&pair[1])))
}

// ---------------------------------------------------------------------------
// Value comparison

/// Rust and the model hold the same value, up to set and map order.
fn same(rust: &Value, model: &Value, step: &Step, what: &str) {
    assert!(itf::normalise(rust) == itf::normalise(model), "{step}: {what} differs\n   Rust: {}\n  model: {}", show(rust), show(model));
}

/// A model store (`id -> Absent | Present(State)`) rendered from Rust: each
/// of the model's ids, with `render`'s value for that id, or `Absent`.
fn slots(model: &Value, render: impl Fn(i64) -> Option<Value>) -> Value {
    map_of(pairs(model).iter().map(|pair| {
        let id = number(&pair[0]);
        (int(id), render(id).map_or_else(|| constant("Absent"), |value| variant("Present", value)))
    }))
}

/// Every resource at `home` agrees with the model store `model`: the same
/// value in each slot, and no resource outside the model's ids.
fn check_store<M: Machine>(home: &Home<M>, model: &Value, render: impl Fn(&Resource<M>) -> Value, step: &Step, what: &str) {
    let ids: BTreeSet<i64> = pairs(model).iter().map(|pair| number(&pair[0])).collect();
    let stray: Vec<&str> = home.resources().map(Resource::name).filter(|name| !ids.contains(&id_of(name))).collect();
    assert!(stray.is_empty(), "{step}: {what} {stray:?} exist in Rust only");
    same(&slots(model, |id| home.get(&id.to_string()).map(&render)), model, step, what);
}

// ---------------------------------------------------------------------------
// Shared checks

/// What one scenario's replay exercised.
#[derive(Clone, Copy, Default)]
struct Tally {
    traces: usize,
    steps: usize,
    /// Creations, edges and records the model took and Rust accepted.
    writes: usize,
    /// Writes Rust refused as expected: ends, model no-ops and late receipts.
    refusals: usize,
    /// Dry-run edges checked against the scenario's safety properties.
    probes: usize,
}

impl Tally {
    fn add(&mut self, other: &Tally) {
        self.traces += other.traces;
        self.steps += other.steps;
        self.writes += other.writes;
        self.refusals += other.refusals;
        self.probes += other.probes;
    }
}

impl fmt::Display for Tally {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:>4} traces {:>6} steps {:>5} writes {:>5} refusals {:>6} probes",
            self.traces, self.steps, self.writes, self.refusals, self.probes
        )
    }
}

/// A model step took this edge, so the Rust machine must accept it.
fn accept<M: Machine>(
    home: &mut Home<M>,
    name: &str,
    edge: impl Into<M::Edge>,
    world: &M::World,
    now: Timestamp,
    step: &Step,
    tally: &mut Tally,
) {
    if let Err(error) = home.apply(name, edge, world, now) {
        panic!("{step}: the model took this edge on {} {name}; Rust refused it: {error:?}", M::NAME);
    }
    tally.writes += 1;
}

/// A model step created this resource, so the Rust machine must admit it.
fn create<M: Machine>(home: &mut Home<M>, name: &str, request: M::Request, world: &M::World, step: &Step, tally: &mut Tally) {
    if let Err(error) = home.create(name, request, world, step.tick()) {
        panic!("{step}: the model created {} {name}; Rust refused it: {error:?}", M::NAME);
    }
    tally.writes += 1;
}

/// Whether the Rust gates would accept `edge` now. Changes nothing.
fn would_accept<M: Machine>(
    home: &Home<M>,
    name: &str,
    edge: impl Into<M::Edge>,
    world: &M::World,
    now: Timestamp,
    tally: &mut Tally,
) -> bool {
    tally.probes += 1;
    let resource = home.get(name).expect("probed resource");
    home.definition().step(&mut Handle::on(resource, world, now), &edge.into()).is_ok()
}

/// Whether `name` exists at `home` and is in an end.
fn ended<M: Machine>(home: &Home<M>, name: &str) -> bool {
    home.get(name).is_some_and(|resource| home.definition().end_reason(resource.label()).is_some())
}

fn shape<M: Machine>(resource: &Resource<M>) -> (M::State, u64, usize, usize, usize) {
    (resource.state().clone(), resource.version().0, resource.events().len(), resource.attempts().len(), resource.receipts().len())
}

/// An ended resource refuses every edge, attempt and receipt with its end's
/// reason, and none of them changes it.
fn assert_end_refuses<M: Machine>(
    home: &mut Home<M>,
    name: &str,
    probes: Vec<M::Edge>,
    world: &M::World,
    now: Timestamp,
    step: &Step,
    tally: &mut Tally,
) {
    let resource = home.get(name).expect("ended resource");
    let state = resource.label();
    let reason = home.definition().end_reason(state).unwrap_or_else(|| panic!("{step}: {} {name} is not in an end", M::NAME));
    let ended: M::Refusal = Ended { state, reason }.into();
    let before = shape(resource);
    for edge in probes {
        let kind = edge.kind();
        match home.apply(name, edge, world, now) {
            Err(WriteError::Refused(refusal)) if refusal == ended => {}
            other => panic!("{step}: ended {} {name} answered {kind:?} with {other:?}", M::NAME),
        }
        tally.refusals += 1;
    }
    let attempt = home.record_attempt(name, Attempt::succeeded(now)).map_err(|error| error.refusal().cloned());
    assert_eq!(attempt, Err(Some(ended.clone())), "{step}: ended {} {name} refuses attempts", M::NAME);
    let receipt = home.record_receipt(name, Receipt { message: ONE.into(), transport: Transport::Turn, at: now });
    assert_eq!(receipt.map_err(|error| error.refusal().cloned()), Err(Some(ended)), "{step}: ended {} {name} refuses receipts", M::NAME);
    tally.refusals += 2;
    assert_eq!(shape(home.get(name).expect("ended resource")), before, "{step}: refusals changed {} {name}", M::NAME);
}

// ---------------------------------------------------------------------------
// Lifecycle (spec `machines/lifecycle.qnt`)

/// The lifecycle kinds the scenarios model, each with an opaque request.
/// `machine_edges` exercises the shared machine on a clone.
struct CloneKind;
impl LifecycleKind for CloneKind {
    const NAME: &'static str = "Clone";
    type Request = ();
}

/// `ensure_bind`'s fulfillers: terminal sessions an Ensure keeps bound.
struct TerminalSession;
impl LifecycleKind for TerminalSession {
    const NAME: &'static str = "TerminalSession";
    type Request = ();
}

/// `admission_placement`'s vessels; the ledger holds their placement requests.
struct VesselKind;
impl LifecycleKind for VesselKind {
    const NAME: &'static str = "Vessel";
    type Request = ();
}

/// The spec's `Requested | Realised(str) | Gone | GaveUp(str)`.
fn lifecycle_value(state: &LifecycleState) -> Value {
    match state {
        LifecycleState::Requested => constant("Requested"),
        LifecycleState::Realised { identity } => variant("Realised", json!(identity)),
        LifecycleState::Gone => constant("Gone"),
        LifecycleState::GaveUp { cause } => variant("GaveUp", json!(cause.detail)),
    }
}

/// Applies a lifecycle operation the model took, with evidence as the model
/// saw it: `seen` makes the observation present and `matches` gives it the
/// expected identity. Each operation reads a fresh world, so missing evidence
/// cannot inherit an earlier observation.
#[allow(clippy::too_many_arguments)]
fn lifecycle_step<K: LifecycleKind>(
    home: &mut Home<Lifecycle<K>>,
    name: &str,
    op: &str,
    seen: bool,
    matches: bool,
    identity: &str,
    now: Timestamp,
    step: &Step,
    tally: &mut Tally,
) {
    let mut world = MemoryLifecycleWorld::default();
    let edge: LifecycleEdge = match op {
        "realise" => {
            let observed = Ref::new(name);
            if seen {
                world.realisations.put(&observed, Observed { identity: if matches { identity } else { "other" }.into() });
            }
            lifecycle::Realise { identity: identity.into(), observed }.into()
        }
        "release" => {
            let teardown = Ref::new(name);
            if seen {
                world.teardowns.put(&teardown, true);
            }
            lifecycle::Release { teardown }.into()
        }
        "vanish" => {
            let observed = Ref::new(name);
            if seen {
                world.absences.put(&observed, true);
            }
            lifecycle::Vanish { observed }.into()
        }
        other => panic!("{step}: lifecycle operation {other}"),
    };
    accept(home, name, edge, &world, now, step, tally);
}

/// An ended lifecycle refuses each edge even with every observation present
/// (the spec's `terminalRefuses` probes).
fn lifecycle_end_refuses<K: LifecycleKind>(
    home: &mut Home<Lifecycle<K>>,
    name: &str,
    identity: &str,
    now: Timestamp,
    step: &Step,
    tally: &mut Tally,
) {
    let witness = Ref::new("witness");
    let mut world = MemoryLifecycleWorld::default();
    world.realisations.put(&witness, Observed { identity: identity.into() });
    world.teardowns.put(&Ref::new("witness"), true);
    world.absences.put(&Ref::new("witness"), true);
    let probes = vec![
        lifecycle::Realise { identity: identity.into(), observed: witness }.into(),
        lifecycle::Release { teardown: Ref::new("witness") }.into(),
        lifecycle::Vanish { observed: Ref::new("witness") }.into(),
        lifecycle::GiveUp { cause: cause("failure") }.into(),
    ];
    assert_end_refuses(home, name, probes, &world, now, step, tally);
}

// ---------------------------------------------------------------------------
// AgentSession (spec `machines/agent_session.qnt`)

/// The spec's `Live(Channel) | Ended(EndReason)`.
fn session_value(state: &AgentSessionState) -> Value {
    match state {
        AgentSessionState::Live { channel } => variant(
            "Live",
            constant(match channel {
                Channel::Detached => "Detached",
                Channel::Attached => "Attached",
            }),
        ),
        AgentSessionState::Ended { reason } => variant("Ended", constant(end_reason_name(*reason))),
    }
}

fn end_reason_name(reason: EndReason) -> &'static str {
    match reason {
        EndReason::Completed => "Completed",
        EndReason::Lost => "Lost",
    }
}

/// Every Ended session refuses attach, detach, either end and every record
/// with `AlreadyEnded`, even with every observation present, and stays unchanged.
fn sessions_end_refuse(sessions: &mut Home<AgentSession>, now: Timestamp, step: &Step, tally: &mut Tally) {
    let mut world = MemoryAgentSessionWorld::default();
    world.ends.put(&Ref::new("completed"), EndReason::Completed);
    world.ends.put(&Ref::new("lost"), EndReason::Lost);
    world.channels.put(&Ref::new("present"), true);
    world.channels.put(&Ref::new("absent"), false);
    let names: Vec<String> = sessions.resources().map(|session| session.name().to_string()).filter(|name| ended(sessions, name)).collect();
    for name in &names {
        let probes = vec![
            Attach { observed: Ref::new("present") }.into(),
            Detach { observed: Ref::new("absent") }.into(),
            End { reason: EndReason::Completed, observed: Ref::new("completed") }.into(),
            End { reason: EndReason::Lost, observed: Ref::new("lost") }.into(),
        ];
        assert_end_refuses(sessions, name, probes, &world, now, step, tally);
    }
}

// ---------------------------------------------------------------------------
// machine_edges: the shared lifecycle and AgentSession edge by edge

fn machine_edges(trace: &Trace, tally: &mut Tally) {
    let mut things = Home::<Lifecycle<CloneKind>>::new("home").expect("declaration");
    let mut sessions = Home::<AgentSession>::new("consumer").expect("declaration");
    for step in trace.steps() {
        tally.steps += 1;
        let now = step.tick();
        let (seen, matches) = (step.yes("seen"), step.yes("matches"));
        match step.action() {
            // L::request and A::request share a name; the store that changed says which.
            "request" if slot_at(step.before, "::resources", 1).is_none() && slot_at(step.after, "::resources", 1).is_some() => {
                create(&mut things, ONE, (), &MemoryLifecycleWorld::default(), &step, tally);
            }
            "request" => create(&mut sessions, ONE, (), &MemoryAgentSessionWorld::default(), &step, tally),
            op @ ("realise" | "release" | "vanish") => lifecycle_step(&mut things, ONE, op, seen, matches, "model", now, &step, tally),
            // The scenario's `failed` flag: a failure that cannot be retried exhausts the attempts.
            "recordFailure" => {
                things.record_attempt(ONE, Attempt::failed(now, cause("failure"), false)).unwrap_or_else(|e| panic!("{step}: {e:?}"));
                tally.writes += 1;
            }
            "giveUp" => {
                let edge = lifecycle::GiveUp { cause: cause(if matches { "failure" } else { "other" }) };
                accept(&mut things, ONE, edge, &MemoryLifecycleWorld::default(), now, &step, tally);
            }
            "attach" => {
                let mut world = MemoryAgentSessionWorld::default();
                let observed = Ref::new(ONE);
                if seen {
                    world.channels.put(&observed, true);
                }
                accept(&mut sessions, ONE, Attach { observed }, &world, now, &step, tally);
            }
            "detach" => {
                let mut world = MemoryAgentSessionWorld::default();
                let observed = Ref::new(ONE);
                if seen {
                    world.channels.put(&observed, false);
                }
                accept(&mut sessions, ONE, Detach { observed }, &world, now, &step, tally);
            }
            "end" => {
                let (reason, other) = match tag(step.pick("reason")) {
                    "Completed" => (EndReason::Completed, EndReason::Lost),
                    "Lost" => (EndReason::Lost, EndReason::Completed),
                    other => panic!("end reason {other}"),
                };
                let mut world = MemoryAgentSessionWorld::default();
                let observed = Ref::new(ONE);
                if seen {
                    world.ends.put(&observed, if matches { reason } else { other });
                }
                accept(&mut sessions, ONE, End { reason, observed }, &world, now, &step, tally);
            }
            other => panic!("{step}: machine_edges operation {other}"),
        }
        check_store(&things, store(step.after, "::resources"), |r| lifecycle_value(r.state()), &step, "lifecycle");
        check_store(&sessions, store(step.after, "::sessions"), |r| session_value(r.state()), &step, "session");
        if ended(&things, ONE) {
            lifecycle_end_refuses(&mut things, ONE, "model", now, &step, tally);
        }
        sessions_end_refuse(&mut sessions, now, &step, tally);
        refuse_without_evidence(&things, &sessions, now, &step, tally);
    }
}

/// The scenario's negative picks (`seen` or `matches` false) are never taken
/// steps, since the model refuses them. From each reached state, every edge
/// with its observation absent, or present with the wrong identity, reason or
/// cause, must be refused.
fn refuse_without_evidence(
    things: &Home<Lifecycle<CloneKind>>,
    sessions: &Home<AgentSession>,
    now: Timestamp,
    step: &Step,
    tally: &mut Tally,
) {
    let none = MemoryLifecycleWorld::default();
    let mut stranger = MemoryLifecycleWorld::default();
    stranger.realisations.put(&Ref::new(ONE), Observed { identity: "other".into() });
    if things.get(ONE).is_some() && !ended(things, ONE) {
        let realise = || lifecycle::Realise { identity: "model".into(), observed: Ref::new(ONE) };
        let probes: [(LifecycleEdge, &MemoryLifecycleWorld); 5] = [
            (realise().into(), &none),
            (realise().into(), &stranger),
            (lifecycle::Release { teardown: Ref::new(ONE) }.into(), &none),
            (lifecycle::Vanish { observed: Ref::new(ONE) }.into(), &none),
            (lifecycle::GiveUp { cause: cause("other") }.into(), &none),
        ];
        for (edge, world) in probes {
            let kind = edge.kind();
            assert!(!would_accept(things, ONE, edge, world, now, tally), "{step}: {kind:?} without its evidence");
        }
    }
    if sessions.get(ONE).is_some() && !ended(sessions, ONE) {
        let mut present = MemoryAgentSessionWorld::default();
        present.channels.put(&Ref::new(ONE), true);
        let mut absent = MemoryAgentSessionWorld::default();
        absent.channels.put(&Ref::new(ONE), false);
        let mut lost = MemoryAgentSessionWorld::default();
        lost.ends.put(&Ref::new(ONE), EndReason::Lost);
        let empty = MemoryAgentSessionWorld::default();
        for world in [&empty, &absent] {
            assert!(
                !would_accept(sessions, ONE, Attach { observed: Ref::new(ONE) }, world, now, tally),
                "{step}: attach without a channel"
            );
        }
        for world in [&empty, &present] {
            assert!(
                !would_accept(sessions, ONE, Detach { observed: Ref::new(ONE) }, world, now, tally),
                "{step}: detach without an absence"
            );
        }
        for world in [&empty, &lost] {
            let end = End { reason: EndReason::Completed, observed: Ref::new(ONE) };
            assert!(!would_accept(sessions, ONE, end, world, now, tally), "{step}: end without its reason observed");
        }
    }
}

// ---------------------------------------------------------------------------
// Convoy (spec `machines/convoy.qnt`)

fn convoy_work(state: &Value) -> &Value {
    match tag(state) {
        "Abandoned" => &payload(state)["work"],
        _ => payload(state),
    }
}

fn quota(v: &Value) -> Quota {
    pairs(v).iter().map(|pair| (VesselClass(small(&pair[0])), small(&pair[1]))).collect()
}

fn quota_value(quota: &Quota) -> Value {
    map_of(quota.iter().map(|(class, count)| (int(class.0.into()), int((*count).into()))))
}

/// The request the model admitted, from the state it admitted: its spec,
/// and the quota in force at admission.
fn convoy_request(state: &Value) -> ConvoyRequest {
    let work = convoy_work(state);
    let spec = &work["spec"];
    ConvoyRequest {
        project: address(&spec["project"]),
        promises: pairs(&spec["promises"])
            .iter()
            .map(|pair| (PromiseId(small(&pair[0])), Promise { kind: text(&pair[1]["kind"]).into(), target: address(&pair[1]["target"]) }))
            .collect(),
        quota: quota(&work["quota"]),
        priority: Priority(small(&spec["priority"])),
    }
}

fn promise_name(state: PromiseState) -> &'static str {
    match state {
        PromiseState::Pending => "Pending",
        PromiseState::Kept => "Kept",
        PromiseState::Retracted => "Retracted",
    }
}

/// The spec's `Open(Work) | Delivered(Work) | Abandoned({ work, reason })`,
/// where `Work = { spec, promises, quota }`: the resource's spec, and each
/// promise's resolution and the quota in force from its state.
fn convoy_value(resource: &Resource<Convoy>) -> Value {
    let spec = resource.spec();
    let work = |promises: &convoy::Resolution, quota: &Quota| {
        json!({
            "spec": {
                "project": spec.project,
                "promises": map_of(spec.promises.iter().map(|(id, promise)| {
                    (int(id.0.into()), json!({ "kind": promise.kind, "target": promise.target }))
                })),
                "priority": int(spec.priority.0.into()),
            },
            "promises": map_of(promises.iter().map(|(id, state)| (int(id.0.into()), constant(promise_name(*state))))),
            "quota": quota_value(quota),
        })
    };
    match resource.state() {
        ConvoyState::Open { resolution, quota } => variant("Open", work(resolution, quota)),
        ConvoyState::Delivered { resolution, quota } => variant("Delivered", work(resolution, quota)),
        ConvoyState::Abandoned { resolution, quota, reason } => {
            variant("Abandoned", json!({ "work": work(resolution, quota), "reason": reason }))
        }
    }
}

fn check_convoys(home: &Home<Convoy>, state: &Value, step: &Step) {
    check_store(home, store(state, "::convoys"), convoy_value, step, "convoy");
}

fn convoy_world(authorised: bool) -> MemoryConvoyWorld {
    let mut world = MemoryConvoyWorld::default();
    world.authority.put(&owner(), authorised);
    world
}

fn admit(home: &mut Home<Convoy>, step: &Step, id: i64, tally: &mut Tally) {
    let model = slot_at(step.after, "::convoys", id).expect("admitted convoy");
    let request = convoy_request(model);
    // The spec's admission reads no world: every project it names exists.
    let world = MemoryConvoyWorld::default().with_project(&request.project);
    create(home, &id.to_string(), request, &world, step, tally);
}

/// From the reached state: an Open convoy delivers only with no promise
/// pending (`deliveredPromisesSettled`) and never re-settles a settled promise
/// (`settledPromisesFinal`); an ended convoy refuses everything.
fn probe_convoys(home: &mut Home<Convoy>, state: &Value, now: Timestamp, step: &Step, tally: &mut Tally) {
    let world = convoy_world(true);
    for (id, model) in entries(state, "::convoys") {
        let Some(model) = model else { continue };
        let name = id.to_string();
        let promises = pairs(&convoy_work(model)["promises"]);
        let current = home.get(&name).expect("convoy").state().quota().clone();
        if tag(model) != "Open" {
            let probes = vec![
                convoy::Keep { promise: PromiseId(0) }.into(),
                convoy::Retract { promise: PromiseId(0), by: "owner".into(), authority: owner() }.into(),
                convoy::Deliver.into(),
                convoy::Abandon { by: "owner".into(), reason: "decision".into(), authority: owner() }.into(),
                convoy::Readmit { quota: current, by: "owner".into(), authority: owner() }.into(),
            ];
            assert_end_refuses(home, &name, probes, &world, now, step, tally);
            continue;
        }
        // The negative picks: without authority nothing is retracted, abandoned
        // or readmitted, and an empty reason abandons nothing.
        let refused = convoy_world(false);
        let decisions: [(ConvoyEdge, &MemoryConvoyWorld); 4] = [
            (convoy::Retract { promise: PromiseId(0), by: "owner".into(), authority: owner() }.into(), &refused),
            (convoy::Abandon { by: "owner".into(), reason: "decision".into(), authority: owner() }.into(), &refused),
            (convoy::Abandon { by: "owner".into(), reason: String::new(), authority: owner() }.into(), &world),
            (convoy::Readmit { quota: current, by: "owner".into(), authority: owner() }.into(), &refused),
        ];
        for (edge, decision_world) in decisions {
            let kind = edge.kind();
            assert!(!would_accept(home, &name, edge, decision_world, now, tally), "{step}: convoy {id} {kind:?} unauthorised");
        }
        if would_accept(home, &name, convoy::Deliver, &world, now, tally) {
            assert!(promises.iter().all(|pair| tag(&pair[1]) != "Pending"), "{step}: convoy {id} delivers with a promise pending");
        }
        for pair in promises.iter().filter(|pair| tag(&pair[1]) != "Pending") {
            let promise = PromiseId(small(&pair[0]));
            assert!(!would_accept(home, &name, convoy::Keep { promise }, &world, now, tally), "{step}: convoy {id} re-keeps {promise:?}");
            let retract = convoy::Retract { promise, by: "owner".into(), authority: owner() };
            assert!(!would_accept(home, &name, retract, &world, now, tally), "{step}: convoy {id} retracts settled {promise:?}");
        }
    }
}

fn convoy_deliver(trace: &Trace, tally: &mut Tally) {
    let mut home = Home::<Convoy>::new("home").expect("declaration");
    for step in trace.steps() {
        tally.steps += 1;
        let id = step.chosen("id");
        let name = id.to_string();
        let now = moment(number(&step.before["now"]));
        let world = convoy_world(step.yes("auth"));
        let promise = || PromiseId(small(step.pick("p")));
        match step.action() {
            "admitRequest" => admit(&mut home, &step, id, tally),
            "keep" => accept(&mut home, &name, convoy::Keep { promise: promise() }, &world, now, &step, tally),
            "acceptRetraction" => {
                let edge = convoy::Retract { promise: promise(), by: "owner".into(), authority: owner() };
                accept(&mut home, &name, edge, &world, now, &step, tally);
            }
            "deliver" => accept(&mut home, &name, convoy::Deliver, &world, now, &step, tally),
            "abandon" => {
                let edge = convoy::Abandon { by: "owner".into(), reason: text(step.pick("reason")).into(), authority: owner() };
                accept(&mut home, &name, edge, &world, now, &step, tally);
            }
            // The scenario readmits with a fixed quota, `Map(0 -> 2, 1 -> 1)`.
            "readmit" => {
                let quota = BTreeMap::from([(VesselClass(0), 2), (VesselClass(1), 1)]);
                accept(&mut home, &name, convoy::Readmit { quota, by: "owner".into(), authority: owner() }, &world, now, &step, tally);
            }
            "unchanged" => {}
            other => panic!("{step}: convoy_deliver operation {other}"),
        }
        check_convoys(&home, step.after, &step);
        probe_convoys(&mut home, step.after, moment(number(&step.after["now"])), &step, tally);
    }
}

// ---------------------------------------------------------------------------
// admission_placement: convoy caps, the placement ledger and vessel lifecycles

fn units(v: &Value) -> Units {
    pairs(v).iter().map(|pair| (Dimension(small(&pair[0])), small(&pair[1]))).collect()
}

fn units_value(units: &Units) -> Value {
    map_of(units.iter().map(|(dimension, count)| (int(dimension.0.into()), int((*count).into()))))
}

fn placement_request(v: &Value) -> placement::Request {
    placement::Request {
        convoy: ConvoyId::new(number(&v["convoy"]).to_string(), "home"),
        class: VesselClass(small(&v["class"])),
        shape: units(&v["shape"]),
        priority: Priority(small(&v["priority"])),
    }
}

/// The spec's vessel `Request`.
fn request_value(request: &placement::Request) -> Value {
    json!({
        "convoy": int(id_of(&request.convoy.name)),
        "class": int(request.class.0.into()),
        "shape": units_value(&request.shape),
        "priority": int(request.priority.0.into()),
    })
}

/// The spec's `Placement = { request, host }`: a lease, one-to-one.
fn lease_value(lease: &Lease) -> Value {
    json!({ "request": request_value(&lease.request), "host": int(lease.host.0.into()) })
}

/// What a placement read: the request placed and the ledger's evidence.
struct Placed {
    request: placement::Request,
    evidence: Evidence,
}

/// The scenario's world: convoys, the ledger, vessel lifecycles, and what
/// the harness keeps beyond them (each placement's evidence, and the leases
/// the ledger returned and forgot).
struct Fleet {
    convoys: Home<Convoy>,
    ledger: Ledger,
    vessels: Home<Lifecycle<VesselKind>>,
    placed: BTreeMap<i64, Placed>,
    returned: BTreeMap<i64, Lease>,
}

const HOSTS: [Host; 2] = [Host(1), Host(2)];

impl Fleet {
    /// The scenario's hosts: two dimensions, so shapes cannot pool across hosts.
    fn new() -> Self {
        let mut ledger = Ledger::new();
        ledger.add_host(Host(1), BTreeMap::from([(Dimension(0), 2), (Dimension(1), 1)])).expect("host 1");
        ledger.add_host(Host(2), BTreeMap::from([(Dimension(0), 1), (Dimension(1), 2)])).expect("host 2");
        Fleet {
            convoys: Home::new("home").expect("declaration"),
            ledger,
            vessels: Home::new("home").expect("declaration"),
            placed: BTreeMap::new(),
            returned: BTreeMap::new(),
        }
    }

    /// The ledger's caps follow the convoy's quota in force.
    fn sync_caps(&mut self, convoy: i64) {
        let name = convoy.to_string();
        let quota = self.convoys.get(&name).expect("admitted convoy").state().quota().clone();
        self.ledger.set_caps(ConvoyId::new(name, "home"), quota);
    }

    /// The spec's `Waiting(Request) | Leased({ placement, lifecycle }) | Gone(Placement)`,
    /// from the ledger, the vessel's lifecycle resource and a returned lease.
    fn vessel_value(&self, id: i64, step: &Step) -> Option<Value> {
        let name = id.to_string();
        let life = self.vessels.get(&name).map(Resource::state);
        match (self.ledger.vessel(&VesselId::new(name)), self.returned.get(&id), life) {
            (None, None, None) => None,
            (Some(placement::Vessel::Waiting(request)), None, None) => Some(variant("Waiting", request_value(request))),
            (Some(placement::Vessel::Leased(lease)), None, Some(life)) if !life.gone() => {
                Some(variant("Leased", json!({ "placement": lease_value(lease), "lifecycle": lifecycle_value(life) })))
            }
            (None, Some(lease), Some(LifecycleState::Gone)) => Some(variant("Gone", lease_value(lease))),
            (held, returned, life) => {
                panic!("{step}: vessel {id} is inconsistent: ledger {held:?}, returned {returned:?}, lifecycle {life:?}")
            }
        }
    }

    /// The scenario's `placedOn`: each placement's available capacity and the
    /// waiting requests, which in the model include the placed one.
    fn placed_value(&self) -> Value {
        map_of(self.placed.iter().map(|(id, placed)| {
            let mut waiting: BTreeSet<&placement::Request> = placed.evidence.waiting.iter().collect();
            waiting.insert(&placed.request);
            let value = json!({
                "available": units_value(&placed.evidence.available),
                "waiting": set_of(waiting.into_iter().map(request_value)),
            });
            (int(*id), value)
        }))
    }

    /// Convoys, vessels and the recorded placement evidence equal the model's,
    /// and every live lease keeps `queuePriority` over the evidence it read.
    fn check(&self, state: &Value, step: &Step) {
        check_convoys(&self.convoys, state, step);
        let model = store(state, "::vessels");
        same(&slots(model, |id| self.vessel_value(id, step)), model, step, "vessel");
        same(&self.placed_value(), &state["placedOn"], step, "placedOn");
        for (vessel, lease) in self.ledger.leases() {
            let placed = self.placed.get(&id_of(&vessel.0)).unwrap_or_else(|| panic!("{step}: {vessel:?} leased without evidence"));
            let jumped = placed
                .evidence
                .waiting
                .iter()
                .find(|w| w.priority > lease.request.priority && placement::fits(w, &placed.evidence.available));
            assert!(jumped.is_none(), "{step}: {vessel:?} was placed ahead of {jumped:?}");
        }
    }

    /// From the reached state, every placement the ledger would grant keeps
    /// `convoyCaps`, `hostCapacity` and `queuePriority`.
    fn probe(&self, state: &Value, step: &Step, tally: &mut Tally) {
        for (id, model) in entries(state, "::vessels") {
            if !model.is_some_and(|model| tag(model) == "Waiting") {
                continue;
            }
            let vessel = VesselId::new(id.to_string());
            for host in HOSTS {
                tally.probes += 1;
                let mut trial = self.ledger.clone();
                let evidence = trial.evidence(&vessel, host).expect("a waiting vessel's evidence");
                let Ok(lease) = trial.place(&vessel, host).cloned() else { continue };
                for convoy in [1, 2] {
                    let convoy = ConvoyId::new(convoy.to_string(), "home");
                    for class in [VesselClass(0), VesselClass(1)] {
                        let (live, cap) = (trial.live(&convoy, class), trial.cap(&convoy, class));
                        assert!(live <= cap, "{step}: placing vessel {id} on {host:?} leases {live} of {convoy:?} {class:?}, cap {cap}");
                    }
                }
                for checked in HOSTS {
                    assert!(trial.available(checked).is_ok(), "{step}: placing vessel {id} on {host:?} oversubscribes {checked:?}");
                }
                let jumped =
                    evidence.waiting.iter().find(|w| w.priority > lease.request.priority && placement::fits(w, &evidence.available));
                assert!(jumped.is_none(), "{step}: placing vessel {id} on {host:?} jumps {jumped:?}");
            }
        }
    }

    /// A Gone vessel's lifecycle refuses everything, and the ledger, which
    /// forgot it, neither returns nor places it again.
    fn gone_refuses(&mut self, state: &Value, now: Timestamp, step: &Step, tally: &mut Tally) {
        for (id, model) in entries(state, "::vessels") {
            if !model.is_some_and(|model| tag(model) == "Gone") {
                continue;
            }
            let vessel = VesselId::new(id.to_string());
            lifecycle_end_refuses(&mut self.vessels, &vessel.0, "vessel", now, step, tally);
            let unknown = Err(LedgerError::UnknownVessel(vessel.clone()));
            assert_eq!(self.ledger.release(&vessel).map(|_| ()), unknown, "{step}: vessel {id} returned twice");
            assert_eq!(self.ledger.place(&vessel, Host(1)).map(|_| ()), unknown, "{step}: vessel {id} placed again");
            tally.refusals += 2;
        }
    }
}

fn admission_placement(trace: &Trace, tally: &mut Tally) {
    let mut fleet = Fleet::new();
    for step in trace.steps() {
        tally.steps += 1;
        let now = step.tick();
        let (convoy, id) = (step.chosen("convoy"), step.chosen("id"));
        let (vessel, name) = (VesselId::new(id.to_string()), id.to_string());
        let ledger_ok = |result: Result<(), LedgerError>, tally: &mut Tally| {
            result.unwrap_or_else(|error| panic!("{step}: the model took this placement step; the ledger refused: {error:?}"));
            tally.writes += 1;
        };
        match step.action() {
            "admitRequest" => {
                admit(&mut fleet.convoys, &step, convoy, tally);
                fleet.sync_caps(convoy);
            }
            // The scenario readmits authorised, with `Map(0 -> count, 1 -> 1)`.
            "readmit" => {
                let quota = BTreeMap::from([(VesselClass(0), small(step.pick("count"))), (VesselClass(1), 1)]);
                let edge = convoy::Readmit { quota, by: "owner".into(), authority: owner() };
                accept(&mut fleet.convoys, &convoy.to_string(), edge, &convoy_world(true), now, &step, tally);
                fleet.sync_caps(convoy);
            }
            "request" => {
                let request = placement_request(payload(slot_at(step.after, "::vessels", id).expect("requested vessel")));
                ledger_ok(fleet.ledger.request(vessel, request), tally);
            }
            // The harness records what the placement read, then the vessel's lifecycle starts.
            "place" => {
                let host = Host(small(step.pick("host")));
                let evidence = fleet.ledger.evidence(&vessel, host).unwrap_or_else(|error| panic!("{step}: evidence: {error:?}"));
                ledger_ok(fleet.ledger.place(&vessel, host).map(|_| ()), tally);
                let request = match fleet.ledger.vessel(&vessel) {
                    Some(placement::Vessel::Leased(lease)) => lease.request.clone(),
                    other => panic!("{step}: placed vessel {id} is {other:?}"),
                };
                fleet.placed.insert(id, Placed { request, evidence });
                create(&mut fleet.vessels, &name, (), &MemoryLifecycleWorld::default(), &step, tally);
            }
            "realise" => lifecycle_step(&mut fleet.vessels, &name, "realise", true, true, "vessel", now, &step, tally),
            // Gone returns the lease; the harness keeps it.
            op @ ("release" | "vanish") => {
                lifecycle_step(&mut fleet.vessels, &name, op, true, true, "vessel", now, &step, tally);
                let lease = fleet.ledger.release(&vessel).unwrap_or_else(|error| panic!("{step}: release refused: {error:?}"));
                fleet.returned.insert(id, lease);
                tally.writes += 1;
            }
            "unchanged" => {}
            other => panic!("{step}: admission_placement operation {other}"),
        }
        fleet.check(step.after, &step);
        probe_convoys(&mut fleet.convoys, step.after, now, &step, tally);
        fleet.probe(step.after, &step, tally);
        fleet.gone_refuses(step.after, now, &step, tally);
    }
}

// ---------------------------------------------------------------------------
// ensure_bind: an Ensure and the lifecycles of its fulfillers

type Fulfillers = Home<Lifecycle<TerminalSession>>;

fn phase(state: &LifecycleState) -> FulfillerPhase {
    match state {
        LifecycleState::Requested => FulfillerPhase::Requested,
        LifecycleState::Realised { .. } => FulfillerPhase::Realised,
        LifecycleState::Gone => FulfillerPhase::Gone,
        LifecycleState::GaveUp { .. } => FulfillerPhase::GaveUp,
    }
}

/// What the Ensure reads: each fulfiller's phase from its lifecycle resource,
/// a drained inbox and a complete teardown (the scenario passes `true` for
/// both), and the authority as the step's `auth` pick.
fn ensure_world(fulfillers: &Fulfillers, authorised: bool) -> MemoryEnsureWorld {
    let mut world = MemoryEnsureWorld::default();
    for resource in fulfillers.resources() {
        world.fulfillers.put(&Ref::new(resource.name()), FulfillerStatus { phase: phase(resource.state()), can_receive: true });
    }
    world.drains.put(&Ref::new(INBOX), true);
    world.authorities.put(&owner(), authorised);
    world.teardowns.put(&Ref::new("fulfillers"), true);
    world
}

fn ensure_work(model: &Value) -> &Value {
    match tag(model) {
        "Fulfilled" => &payload(model)["work"],
        _ => payload(model),
    }
}

fn fulfiller_set(fulfillers: &ensure::Fulfillers) -> Value {
    set_of(fulfillers.iter().map(|fulfiller| int(id_of(fulfiller.name()))))
}

/// The spec's `Unfulfilled(Work) | Fulfilled({ work, bound }) | Held(Work) | Retired(Work)`
/// with `Work = { recorded, created, history, window }`: the state's data, and
/// the history as the Ensure's attempts, each failure naming its fulfiller.
fn ensure_value(resource: &Resource<Ensure>) -> Value {
    let history = resource.attempts().iter().map(|row| match &row.record.outcome {
        Outcome::Failed { cause, .. } => int(id_of(&cause.detail)),
        Outcome::Succeeded => panic!("{}: the scenario records only failures", resource.name()),
    });
    let state = resource.state();
    let work = json!({
        "recorded": fulfiller_set(state.recorded()),
        "created": fulfiller_set(state.created()),
        "history": Value::Array(history.collect()),
        "window": int(i64::from(state.window())),
    });
    match state {
        EnsureState::Unfulfilled { .. } => variant("Unfulfilled", work),
        EnsureState::Fulfilled { bound, .. } => variant("Fulfilled", json!({ "work": work, "bound": int(id_of(bound.name())) })),
        EnsureState::Held { .. } => variant("Held", work),
        EnsureState::Retired { .. } => variant("Retired", work),
    }
}

/// From the reached state: a create the Rust gates would accept names a
/// recorded fulfiller while none is live (`recordedBeforeCreate`,
/// `oneLiveFulfiller`); a bind names a recorded one (`boundRecorded`); a
/// Retired Ensure refuses everything.
fn probe_ensure(home: &mut Home<Ensure>, fulfillers: &Fulfillers, state: &Value, now: Timestamp, step: &Step, tally: &mut Tally) {
    let Some(model) = slot_at(state, "::ensures", 1) else { return };
    let world = ensure_world(fulfillers, true);
    let fulfiller = |id: i64| Ref::<ensure::Fulfiller>::new(id.to_string());
    if tag(model) == "Retired" {
        let probes = vec![
            ensure::Record { fulfiller: fulfiller(1) }.into(),
            ensure::Create { fulfiller: fulfiller(2) }.into(),
            ensure::Bind { fulfiller: fulfiller(1) }.into(),
            ensure::Unbind { reason: UnbindReason::Gone, fulfiller: fulfiller(1) }.into(),
            ensure::GiveUp.into(),
            ensure::Resume { by: "owner".into(), authority: owner() }.into(),
            ensure::Retire { by: "owner".into(), authority: owner(), teardown: Ref::new("fulfillers") }.into(),
        ];
        assert_end_refuses(home, ONE, probes, &world, now, step, tally);
        return;
    }
    // The negative `auth` pick: without authority, resume and retire refuse.
    let refused = ensure_world(fulfillers, false);
    let resume = ensure::Resume { by: "owner".into(), authority: owner() };
    assert!(!would_accept(home, ONE, resume, &refused, now, tally), "{step}: resumes without authority");
    let retire = ensure::Retire { by: "owner".into(), authority: owner(), teardown: Ref::new("fulfillers") };
    assert!(!would_accept(home, ONE, retire, &refused, now, tally), "{step}: retires without authority");
    let recorded: BTreeSet<i64> = set(&ensure_work(model)["recorded"]).iter().map(number).collect();
    for id in [1, 2] {
        if would_accept(home, ONE, ensure::Create { fulfiller: fulfiller(id) }, &world, now, tally) {
            assert!(recorded.contains(&id), "{step}: creates unrecorded fulfiller {id}");
            let live: Vec<&str> = fulfillers.resources().filter(|r| r.state().live()).map(Resource::name).collect();
            assert!(live.is_empty(), "{step}: creates fulfiller {id} while {live:?} live");
        }
        if would_accept(home, ONE, ensure::Bind { fulfiller: fulfiller(id) }, &world, now, tally) {
            assert!(recorded.contains(&id), "{step}: binds unrecorded fulfiller {id}");
        }
    }
}

/// The scenario's `L::releaseAll(recorded, true)` on retire: each recorded
/// fulfiller is released, and one the model left unchanged (already ended)
/// must be refused.
fn release_all(fulfillers: &mut Fulfillers, step: &Step, now: Timestamp, tally: &mut Tally) {
    let model = slot_at(step.before, "::ensures", 1).expect("retiring Ensure");
    let mut world = MemoryLifecycleWorld::default();
    world.teardowns.put(&Ref::new("fulfillers"), true);
    for id in set(&ensure_work(model)["recorded"]).iter().map(number) {
        let name = id.to_string();
        if fulfillers.get(&name).is_none() {
            continue;
        }
        let changed = slot_at(step.before, "::resources", id) != slot_at(step.after, "::resources", id);
        let result = fulfillers.apply(&name, lifecycle::Release { teardown: Ref::new("fulfillers") }, &world, now).map(|_| ());
        match (changed, result) {
            (true, Ok(())) => tally.writes += 1,
            (false, Err(WriteError::Refused(LifecycleRefusal::Ended(_)))) => tally.refusals += 1,
            (changed, result) => panic!("{step}: releasing fulfiller {id} (model changed it: {changed}) answered {result:?}"),
        }
    }
}

fn ensure_bind(trace: &Trace, tally: &mut Tally) {
    let mut home = Home::<Ensure>::new("home").expect("declaration");
    let mut fulfillers = Fulfillers::new("home").expect("declaration");
    for step in trace.steps() {
        tally.steps += 1;
        let now = step.tick();
        let who = step.chosen("who");
        let name = who.to_string();
        let world = ensure_world(&fulfillers, step.yes("auth"));
        let fulfiller = Ref::<ensure::Fulfiller>::new(name.clone());
        match step.action() {
            // One failure exhausts a window: the spec gives up once `history - window >= 1`.
            "request" => {
                create(&mut home, ONE, EnsureSpec { inbox: INBOX.into(), policy: AttemptPolicy::immediate(1) }, &world, &step, tally)
            }
            "record" => accept(&mut home, ONE, ensure::Record { fulfiller }, &world, now, &step, tally),
            "create" => {
                accept(&mut home, ONE, ensure::Create { fulfiller }, &world, now, &step, tally);
                create(&mut fulfillers, &name, (), &MemoryLifecycleWorld::default(), &step, tally);
            }
            op @ ("realise" | "release" | "vanish") => lifecycle_step(&mut fulfillers, &name, op, true, true, "model", now, &step, tally),
            "bind" => accept(&mut home, ONE, ensure::Bind { fulfiller }, &world, now, &step, tally),
            "unbind" => accept(&mut home, ONE, ensure::Unbind { reason: UnbindReason::Gone, fulfiller }, &world, now, &step, tally),
            // The spec's `Fail({ id: who })`: the failure names its fulfiller as its cause.
            "recordFailure" => {
                home.record_attempt(ONE, Attempt::failed(now, cause(&name), true))
                    .unwrap_or_else(|error| panic!("{step}: the model recorded a failure; Rust refused: {error:?}"));
                tally.writes += 1;
            }
            "giveUp" => accept(&mut home, ONE, ensure::GiveUp, &world, now, &step, tally),
            "resume" => accept(&mut home, ONE, ensure::Resume { by: "owner".into(), authority: owner() }, &world, now, &step, tally),
            "retire" => {
                let edge = ensure::Retire { by: "owner".into(), authority: owner(), teardown: Ref::new("fulfillers") };
                accept(&mut home, ONE, edge, &world, now, &step, tally);
                release_all(&mut fulfillers, &step, now, tally);
            }
            "unchanged" => {}
            other => panic!("{step}: ensure_bind operation {other}"),
        }
        check_store(&home, store(step.after, "::ensures"), ensure_value, &step, "Ensure");
        check_store(&fulfillers, store(step.after, "::resources"), |r| lifecycle_value(r.state()), &step, "fulfiller");
        for id in [1, 2] {
            if ended(&fulfillers, &id.to_string()) {
                lifecycle_end_refuses(&mut fulfillers, &id.to_string(), "model", now, &step, tally);
            }
        }
        probe_ensure(&mut home, &fulfillers, step.after, now, &step, tally);
    }
}

// ---------------------------------------------------------------------------
// inbox_delivery and message_race: one Message against a consumer stream

/// The two message scenarios differ in their consumer stream and evidence.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Feed {
    /// Two consumer sessions, a binding that changes, no authority.
    Inbox,
    /// One always-bound consumer; supersession and authority both hold.
    Race,
}

impl Feed {
    fn stream(self, state: &Value) -> &Value {
        match self {
            Feed::Inbox => &state["stream"],
            Feed::Race => &state["source"],
        }
    }
}

/// One stream row: when, what kind, and which session wrote it.
struct Row {
    at: i64,
    kind: String,
    session: i64,
}

/// A consumer stream (`world/stream.qnt`): rows in order and the applied prefix.
struct Stream {
    rows: Vec<Row>,
    applied: usize,
}

impl Stream {
    fn of(v: &Value) -> Self {
        let rows = list(&v["rows"]).iter().map(|row| Row {
            at: number(&row["at"]),
            kind: text(&row["kind"]).into(),
            session: number(&row["session"]),
        });
        Stream { rows: rows.collect(), applied: usize::try_from(number(&v["applied"])).expect("applied prefix") }
    }

    fn seen(&self) -> &[Row] {
        &self.rows[..self.applied]
    }

    fn sessions(&self, kind: &str) -> BTreeSet<i64> {
        self.seen().iter().filter(|row| row.kind == kind).map(|row| row.session).collect()
    }

    fn any(&self, kind: &str) -> bool {
        self.seen().iter().any(|row| row.kind == kind)
    }

    /// `S::watermark`: the newest applied keepalive, else zero.
    fn watermark(&self) -> i64 {
        self.seen().iter().filter(|row| row.kind == "keepalive").map(|row| row.at).fold(0, i64::max)
    }
}

fn drop_reason(v: &Value) -> DropReason {
    match tag(v) {
        "Undeliverable" => DropReason::Undeliverable,
        "Expired" => DropReason::Expired,
        "Superseded" => DropReason::Superseded { by: Ref::new("newer") },
        "Withdrawn" => DropReason::Withdrawn { by: "owner".into(), authority: owner() },
        "ConsumerUnreachable" => DropReason::ConsumerUnreachable { by: "owner".into(), authority: owner() },
        other => panic!("drop reason {other}"),
    }
}

fn reason_name(reason: &DropReason) -> &'static str {
    match reason {
        DropReason::Undeliverable => "Undeliverable",
        DropReason::Expired => "Expired",
        DropReason::Superseded { .. } => "Superseded",
        DropReason::Withdrawn { .. } => "Withdrawn",
        DropReason::ConsumerUnreachable { .. } => "ConsumerUnreachable",
    }
}

fn session_number(receiver: &message::Receiver) -> i64 {
    id_of(&receiver.session)
}

/// The spec's `Envelope = { expectation, hasDeadline, deadline }`, from the
/// message's spec. Without a deadline Rust has no value, so it renders 0.
fn envelope_value(spec: &MessageSpec) -> Value {
    let expectation = match spec.expectation {
        Expectation::Nothing => "Nothing",
        Expectation::Reply => "Reply",
        Expectation::Outcome { .. } => "Outcome",
    };
    json!({
        "expectation": constant(expectation),
        "hasDeadline": spec.deadline.is_some(),
        "deadline": int(spec.deadline.map_or(0, ticks)),
    })
}

/// The model's message store with each deadline-less envelope's unused
/// `deadline` read as 0, as [`envelope_value`] renders it.
fn without_unset_deadline(v: &Value) -> Value {
    match v {
        Value::Object(fields) => {
            let mut fields: serde_json::Map<String, Value> = fields.iter().map(|(k, v)| (k.clone(), without_unset_deadline(v))).collect();
            if fields.get("hasDeadline") == Some(&Value::Bool(false)) {
                fields.insert("deadline".into(), int(0));
            }
            Value::Object(fields)
        }
        Value::Array(items) => Value::Array(items.iter().map(without_unset_deadline).collect()),
        other => other.clone(),
    }
}

/// The spec's `Pending(Envelope) | Awaiting({ envelope, receiver }) |
/// Settled(Envelope) | Dropped(Drop)`: the spec's envelope and the state's
/// data. A drop's `bound` is whether it recorded a watermark, and its
/// `receipt` is false (see the module doc).
fn message_value(resource: &Resource<Message>) -> Value {
    let envelope = envelope_value(resource.spec());
    match resource.state() {
        MessageState::Pending => variant("Pending", envelope),
        MessageState::Awaiting { receiver } => variant(
            "Awaiting",
            json!({ "envelope": envelope, "receiver": { "session": int(session_number(receiver)), "host": receiver.host } }),
        ),
        MessageState::Settled => variant("Settled", envelope),
        MessageState::Dropped { reason, dropped_receiver: receiver, at, watermark } => variant(
            "Dropped",
            json!({
                "reason": constant(reason_name(reason)),
                "envelope": envelope,
                "receiver": int(receiver.as_ref().map_or(0, session_number)),
                "at": int(ticks(*at)),
                "watermark": int(watermark.map_or(0, ticks)),
                "bound": watermark.is_some(),
                "receipt": false,
            }),
        ),
    }
}

fn deliver(session: i64) -> message::Deliver {
    message::Deliver { receiver: message::Receiver { session: session.to_string(), host: "consumer".into() }, transport: Transport::Turn }
}

struct MessageReplay<'a> {
    feed: Feed,
    home: Home<Message>,
    /// The consumer sessions (inbox_delivery only).
    sessions: Option<Home<AgentSession>>,
    expectation: &'a str,
    deadline: Option<i64>,
    /// Accepted message edges so far.
    edges: usize,
    /// Receipts each session should hold.
    receipts: BTreeMap<String, usize>,
}

impl<'a> MessageReplay<'a> {
    fn new(trace: &Trace<'a>, feed: Feed) -> Self {
        let initial = slot_at(&trace.states[0], "::messages", 1).expect("initial message");
        assert_eq!(tag(initial), "Pending", "{}: initial message", trace.file);
        let envelope = payload(initial);
        let expectation = tag(&envelope["expectation"]);
        let deadline = boolean(&envelope["hasDeadline"]).then(|| number(&envelope["deadline"]));
        let spec = MessageSpec {
            inbox: INBOX.into(),
            expectation: match expectation {
                "Nothing" => Expectation::Nothing,
                "Reply" => Expectation::Reply,
                "Outcome" => Expectation::Outcome { condition: Ref::new("outcome") },
                other => panic!("expectation {other}"),
            },
            deadline: deadline.map(moment),
            // Both scenarios drop as undeliverable once `now - unboundAt > 1`.
            unbound_after: UNIT,
        };
        let mut home = Home::<Message>::new("home").expect("declaration");
        home.create(ONE, spec, &MemoryMessageWorld::default(), Timestamp(0)).expect("message");
        let sessions = (feed == Feed::Inbox).then(|| {
            let mut sessions = Home::<AgentSession>::new("consumer").expect("declaration");
            for (id, _) in entries(&trace.states[0], "::sessions") {
                sessions.create(id.to_string(), (), &MemoryAgentSessionWorld::default(), Timestamp(0)).expect("session");
            }
            sessions
        });
        MessageReplay { feed, home, sessions, expectation, deadline, edges: 0, receipts: BTreeMap::new() }
    }

    /// What the message's gates read in `state`. `witness` grants authority
    /// and a supersession whatever the scenario says, for end probes.
    fn world(&self, state: &Value, witness: bool) -> MemoryMessageWorld {
        let stream = Stream::of(self.feed.stream(state));
        let mut world = MemoryMessageWorld::default();
        let own = Ref::new(ONE);
        world.receipts.put(&own, stream.sessions("receipt").iter().map(|session| (session.to_string(), Transport::Turn)).collect());
        if stream.any("reply") {
            world.reply.put(&Ref::new(ONE), ());
        }
        world.condition.put(&Ref::new("outcome"), stream.any("outcome"));
        if stream.any("failure") {
            world.delivery_failure.put(&Ref::new(ONE), ());
        }
        world.applied_through.put(&Ref::new(INBOX), moment(stream.watermark()));
        let binding = match self.feed {
            Feed::Race => InboxBinding::Bound { session: "1".into() },
            Feed::Inbox => match number(&state["bound"]) {
                0 => InboxBinding::Unbound { since: moment(number(&state["unboundAt"])) },
                session => InboxBinding::Bound { session: session.to_string() },
            },
        };
        world.binding.put(&Ref::new(INBOX), binding);
        let evidence = witness || self.feed == Feed::Race;
        world.authority.put(&owner(), evidence);
        if evidence {
            world.supersession.put(&Ref::new("newer"), message::Supersedes { older: ONE.into() });
        }
        world
    }

    fn step(&mut self, step: &Step, tally: &mut Tally) {
        tally.steps += 1;
        let now = moment(number(&step.before["now"]));
        let who = match self.feed {
            Feed::Inbox => step.chosen("who"),
            Feed::Race => 1,
        };
        let world = self.world(step.before, false);
        let edge: Option<MessageEdge> = match (self.feed, step.action()) {
            (_, "deliver") => Some(deliver(who).into()),
            (_, "settle") => Some(
                message::Settle {
                    by: if self.expectation == "Reply" { message::Settlement::Answered } else { message::Settlement::OutcomeObserved },
                }
                .into(),
            ),
            (_, "drop") => {
                let model = slot_at(step.after, "::messages", 1).expect("message");
                assert_eq!(tag(model), "Dropped", "{step}: a drop drops");
                let reason = &payload(model)["reason"];
                if self.feed == Feed::Race {
                    assert_eq!(step.pick("reason"), reason, "{step}: the model records the reason it chose");
                }
                Some(message::DropMessage { reason: drop_reason(reason) }.into())
            }
            (Feed::Inbox, "end") => {
                // The model's `end` is observed exactly when its end row reached the
                // stream (`canWrite`); reading that from the trace keeps the replay
                // independent of the scenario's `maxRows`, which the deep tier raises.
                let before = Stream::of(&step.before["stream"]).rows.len();
                let can_write = Stream::of(&step.after["stream"]).rows.len() > before;
                let observed = Ref::new(who.to_string());
                let mut ends = MemoryAgentSessionWorld::default();
                if can_write {
                    ends.ends.put(&observed, EndReason::Completed);
                }
                let sessions = self.sessions.as_mut().expect("sessions");
                accept(sessions, &who.to_string(), End { reason: EndReason::Completed, observed }, &ends, now, step, tally);
                None
            }
            // The consumer records a receipt on its session when it publishes one;
            // an ended session refuses it, while the row still reaches the stream
            // the message home reads.
            (Feed::Inbox, "publish") if text(step.pick("kind")) == "receipt" => {
                let name = who.to_string();
                let live = slot_at(step.before, "::sessions", who).is_some_and(|s| tag(s) == "Live");
                let sessions = self.sessions.as_mut().expect("sessions");
                let result = sessions.record_receipt(&name, Receipt { message: ONE.into(), transport: Transport::Turn, at: now });
                match (live, result) {
                    (true, Ok(_)) => {
                        *self.receipts.entry(name).or_default() += 1;
                        tally.writes += 1;
                    }
                    (false, Err(WriteError::Refused(AgentSessionRefusal::AlreadyEnded))) => tally.refusals += 1,
                    (live, result) => panic!("{step}: receipt on session {who} (live: {live}) answered {result:?}"),
                }
                None
            }
            (
                Feed::Inbox,
                "turn" | "publish" | "bind" | "unbindEndedWorld" | "unbindSilentWorld" | "applySource" | "applyHome" | "disconnect"
                | "connect" | "die" | "recover" | "tick",
            )
            | (Feed::Race, "publish" | "applySource" | "disconnect" | "connect" | "tick") => None,
            (_, other) => panic!("{step}: message operation {other}"),
        };
        if let Some(edge) = edge {
            accept(&mut self.home, ONE, edge, &world, now, step, tally);
            self.edges += 1;
        }
        self.check(step);
        self.probe(step, tally);
        if let Some(sessions) = self.sessions.as_mut() {
            check_store(sessions, store(step.after, "::sessions"), |r| session_value(r.state()), step, "session");
            for (name, expected) in &self.receipts {
                let session = sessions.get(name).expect("session");
                assert_eq!(session.receipts().len(), *expected, "{step}: receipts on session {name}");
            }
            sessions_end_refuse(sessions, moment(number(&step.after["now"])), step, tally);
        }
    }

    /// The message's value equals the model's. As an audit of the history,
    /// each accepted edge left one event and at most one was a delivery.
    fn check(&self, step: &Step) {
        let model = without_unset_deadline(store(step.after, "::messages"));
        check_store(&self.home, &model, message_value, step, "message");
        let rust = self.home.get(ONE).expect("message");
        assert_eq!(rust.events().len(), self.edges, "{step}: one edge event per accepted edge");
        let deliveries = rust.events().iter().filter(|row| row.record.edge == MessageEdgeKind::Deliver).count();
        assert!(deliveries <= 1, "{step}: delivered {deliveries} times");
    }

    /// From the reached state: an ended message refuses everything; otherwise
    /// each drop or delivery the Rust gates would accept keeps the scenario's
    /// safety properties.
    fn probe(&mut self, step: &Step, tally: &mut Tally) {
        let state = step.after;
        let now = moment(number(&state["now"]));
        if ended(&self.home, ONE) {
            let probes = vec![
                deliver(1).into(),
                message::Settle { by: message::Settlement::Answered }.into(),
                message::Settle { by: message::Settlement::OutcomeObserved }.into(),
                message::DropMessage { reason: DropReason::Withdrawn { by: "owner".into(), authority: owner() } }.into(),
                message::DropMessage { reason: DropReason::Expired }.into(),
                message::DropMessage { reason: DropReason::Undeliverable }.into(),
            ];
            let witness = self.world(state, true);
            assert_end_refuses(&mut self.home, ONE, probes, &witness, now, step, tally);
            return;
        }
        let current = self.home.get(ONE).expect("message").state().clone();
        let world = self.world(state, false);
        let stream = Stream::of(self.feed.stream(state));
        // `noUndeliverableReceipt`, `undeliverableHasNoReceiver`.
        if would_accept(&self.home, ONE, message::DropMessage { reason: DropReason::Undeliverable }, &world, now, tally) {
            assert!(!stream.any("receipt"), "{step}: undeliverable despite an observed receipt");
            assert_eq!(current, MessageState::Pending, "{step}: undeliverable after delivery");
        }
        // `expiryPastReceiverPrefix`, `expiredPastPrefix`, `expiryLosesToEvidence`.
        if would_accept(&self.home, ONE, message::DropMessage { reason: DropReason::Expired }, &world, now, tally) {
            let deadline = self.deadline.unwrap_or_else(|| panic!("{step}: expired without a deadline"));
            assert!(now > moment(deadline), "{step}: expired before the deadline");
            let bound = match self.feed {
                Feed::Race => true,
                Feed::Inbox => number(&state["bound"]) != 0 || current.receiver().is_some(),
            };
            if bound {
                assert!(stream.watermark() > deadline, "{step}: expired before the consumer's stream passed the deadline");
            }
            if self.feed == Feed::Race {
                let evidence = match self.expectation {
                    "Reply" => Some("reply"),
                    "Outcome" => Some("outcome"),
                    _ => None,
                };
                let in_time = stream.rows.iter().any(|row| Some(row.kind.as_str()) == evidence && row.at <= deadline);
                assert!(!in_time, "{step}: expiry beat evidence written before the deadline");
            }
        }
        // `receiverTurned`: a receipt exists only after the consumer's turn.
        if self.feed == Feed::Inbox {
            let turned: BTreeSet<i64> = set(&state["turned"]).iter().map(number).collect();
            for who in [1, 2] {
                if would_accept(&self.home, ONE, deliver(who), &world, now, tally) {
                    assert!(turned.contains(&who), "{step}: delivers to session {who}, which had no turn");
                }
            }
        }
    }
}

fn inbox_delivery(trace: &Trace, tally: &mut Tally) {
    let mut replay = MessageReplay::new(trace, Feed::Inbox);
    for step in trace.steps() {
        replay.step(&step, tally);
    }
}

fn message_race(trace: &Trace, tally: &mut Tally) {
    let mut replay = MessageReplay::new(trace, Feed::Race);
    for step in trace.steps() {
        replay.step(&step, tally);
    }
}

// ---------------------------------------------------------------------------

type Scenario = fn(&Trace, &mut Tally);

/// Every generated trace replays through the engine: each machine operation
/// the model took is accepted by Rust and each world step applies nothing;
/// after every step every Rust store, rendered from its resources' state
/// values (with spec and attempts where the model's state carries them),
/// equals the model's store, and the harness's placement evidence equals the
/// scenario's `placedOn` and keeps `queuePriority`; ended resources refuse
/// every edge and record and change nothing; every edge the Rust gates would
/// accept from a reached state keeps the scenario's safety properties; and
/// the scenarios' negative picks are refused. Every scenario must supply traces.
#[test]
#[ignore = "generate traces with specs/primitives/check.sh"]
fn replay_quint_itf_traces() {
    let directory =
        std::env::var("QUINT_TRACE_DIR").expect("QUINT_TRACE_DIR names a directory of ITF traces from specs/primitives/check.sh");
    let scenarios: [(&str, Scenario); 6] = [
        ("convoy_deliver", convoy_deliver),
        ("ensure_bind", ensure_bind),
        ("inbox_delivery", inbox_delivery),
        ("message_race", message_race),
        ("machine_edges", machine_edges),
        ("admission_placement", admission_placement),
    ];
    let mut tallies = [Tally::default(); 6];
    let mut paths: Vec<_> = fs::read_dir(Path::new(&directory))
        .unwrap_or_else(|error| panic!("read {directory}: {error}"))
        .map(|entry| entry.expect("entry").path())
        .collect();
    paths.sort();
    let started = Instant::now();
    for path in paths {
        let file = path.file_name().expect("file name").to_string_lossy().into_owned();
        if !file.ends_with(".itf.json") {
            continue;
        }
        let index = scenarios
            .iter()
            .position(|(prefix, _)| file.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('-')))
            .unwrap_or_else(|| panic!("{file} names no scenario"));
        let trace: Value = serde_json::from_slice(&fs::read(&path).expect("read trace")).unwrap_or_else(|error| panic!("{file}: {error}"));
        let trace = Trace { file: &file, states: list(&trace["states"]) };
        (scenarios[index].1)(&trace, &mut tallies[index]);
        tallies[index].traces += 1;
    }
    let mut total = Tally::default();
    for ((name, _), tally) in scenarios.iter().zip(&tallies) {
        eprintln!("{name:>20}: {tally}");
        total.add(tally);
    }
    eprintln!("{:>20}: {total} in {:.1?}", "replayed", started.elapsed());
    for ((name, _), tally) in scenarios.iter().zip(&tallies) {
        assert!(tally.traces > 0, "{name} supplied no traces in {directory}");
    }
}
