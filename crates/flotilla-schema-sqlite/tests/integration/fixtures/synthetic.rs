//! Synthetic kinds (#3373 ruling 4): small, clearly synthetic shapes for the derive features no
//! engine kind uses yet, so the derive stays covered through SQLite and every unused feature is
//! visible. No engine machine is a referral or a dossier. Each feature, where it lives, and its
//! expected first real user:
//!
//! | Feature | Where | Expected first real user |
//! |---|---|---|
//! | `frozen` objects, optional, and set by an edge that carries them | `Dossier::workflow`, `Dossier::placement` | the convoy's frozen workflow snapshot |
//! | `records` nested in `records` (a records row's state set by `set_in`, rows appended under it by `append_in`, a flattened `Option` and a child map in a nested row) | `Dossier::deliverables` and their `evidence` | convoy deliverables and their evidence, from promise submissions |
//! | a map of a data enum whose variant holds a child table | `Dossier::inputs` | a convoy's workflow inputs |
//! | the seven-variant reference enum, in a child `Vec` | `Referral::references` | message references, until ADR 0066 makes a reference an address plus a revision |
//! | an optional data enum an edge sets | `Referral::subject` | a message's subject reference |
//! | references to kinds (`Ref<K>`, whose writes create identity rows) and untyped `Identity` columns | `Referral::session`, `Referral::in_reply_to`, `Reference::Commit` | references that the address layer keeps as references |
//! | an interned newtype | `Referral::sender` | agent names |
//! | an optional flattened struct holding a data enum | `Referral::condition` | a message's delivery condition |
//! | `DateTime<Utc>` (floored to microseconds), `u64` and `u16` range checks, an `order` index on a spec column | `Referral::sent_at`, `seq`, `ordinal`, `by_session` | resource specs keeping chrono times and sequence numbers |
//! | code-enum states, written whole by `Edge::new`, and edges setting a root column | `Referral::phase`, `Dossier::state`, `Referral::subject` | sub-machines without data, such as a deliverable's state |
//!
//! Frozen data inside a data enum variant lives in the lifecycle test kind (`TerminalSession`'s
//! agent brief). Edge events are log entries, not rows (#3392 G8), so no kind here holds one.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use flotilla_schema::{Edge, EdgeError, EdgeEvent, Frozen, Identity, Records, Ref, State, Stored, Timestamp};
use hegel::{generators as gs, TestCase};
use serde::{Deserialize, Serialize};

use super::{
    appended, code_enum, event, floor, identity, list, name, payload, position, records, reference, replaced, seq, stamp, stamp_flat,
    terminal::map, text, time, timestamp, AgentSessionRow, Fixture, Payload, Pick, SHAPES,
};

/// A synthetic message-like kind: references, addressing and plain spec columns.
#[derive(Debug, Clone, PartialEq, Stored)]
#[store(
    kind = "Referral",
    code = 10,
    index(name = "by_session", eq(session), order(seq)),
    index(name = "by_reply", eq(in_reply_to), when(in_reply_to is_some))
)]
pub struct Referral {
    #[store(intern)]
    pub sender: AgentName,
    pub session: Ref<AgentSessionRow>,
    pub in_reply_to: Option<Ref<Referral>>,
    #[store(child)]
    pub references: Vec<Reference>,
    pub subject: Option<LeafAddress>,
    #[store(flatten)]
    pub condition: Option<Leaf>,
    pub sent_at: Option<DateTime<Utc>>,
    pub seq: u64,
    pub ordinal: u16,
    #[store(state)]
    pub phase: State<Phase>,
}

/// A session name: a newtype of `String`, interned where it is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct AgentName(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum Phase {
    #[store(code = 1)]
    Draft,
    #[store(code = 2)]
    Sent,
    #[store(code = 3)]
    Read,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum ReferralEdge {
    #[store(code = 1)]
    Send,
    #[store(code = 2)]
    Read,
    #[store(code = 3)]
    Amend,
}

/// The seven-variant reference enum, transcribed from `flotilla_protocol::MessageReference`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum Reference {
    #[store(code = 1)]
    ChangeRequest {
        #[store(intern)]
        service: String,
        scope: String,
        number: u64,
        revision: String,
    },
    #[store(code = 2)]
    Commit { repository: Identity, revision: String },
    #[store(code = 3)]
    Ref { repository: Identity, name: String, revision: String },
    #[store(code = 4)]
    Artifact { resource: Identity, revision: String },
    #[store(code = 5)]
    Issue {
        #[store(intern)]
        service: String,
        scope: String,
        number: u64,
        revision: String,
    },
    #[store(code = 6)]
    Comment {
        #[store(intern)]
        service: String,
        scope: String,
        id: String,
        revision: String,
    },
    #[store(code = 7)]
    ControlRecord { resource: Identity, revision: String },
}

/// A condition on an addressed value: a flattened struct holding a data enum.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub struct Leaf {
    pub address: LeafAddress,
    pub operator: Operator,
    pub literal: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub enum LeafAddress {
    #[store(code = 1)]
    Convoy { name: String },
    #[store(code = 2)]
    ChangeRequest {
        #[store(intern)]
        service: String,
        number: u64,
    },
    #[store(code = 3)]
    Usage { provider: String, account: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum Operator {
    #[store(code = 1)]
    Equal,
    #[store(code = 2)]
    LessThan,
    #[store(code = 3)]
    GreaterThan,
}

impl Reference {
    pub fn generate(tc: &TestCase, pick: Pick) -> Reference {
        match pick.variant(tc, 7, |index| index) {
            0 => Reference::ChangeRequest { service: name(tc), scope: text(tc), number: seq(tc), revision: text(tc) },
            1 => Reference::Commit { repository: identity(tc), revision: text(tc) },
            2 => Reference::Ref { repository: identity(tc), name: text(tc), revision: text(tc) },
            3 => Reference::Artifact { resource: identity(tc), revision: text(tc) },
            4 => Reference::Issue { service: name(tc), scope: text(tc), number: seq(tc), revision: text(tc) },
            5 => Reference::Comment { service: name(tc), scope: text(tc), id: text(tc), revision: text(tc) },
            _ => Reference::ControlRecord { resource: identity(tc), revision: text(tc) },
        }
    }
}

impl LeafAddress {
    fn generate(tc: &TestCase, pick: Pick) -> LeafAddress {
        match pick.variant(tc, 3, |index| index) {
            0 => LeafAddress::Convoy { name: text(tc) },
            1 => LeafAddress::ChangeRequest { service: name(tc), number: seq(tc) },
            _ => LeafAddress::Usage { provider: text(tc), account: text(tc) },
        }
    }
}

impl Leaf {
    fn generate(tc: &TestCase, pick: Pick) -> Leaf {
        let operator = code_enum(tc, pick, &[Operator::Equal, Operator::LessThan, Operator::GreaterThan]);
        Leaf { address: LeafAddress::generate(tc, pick), operator, literal: text(tc) }
    }
}

const PHASES: [Phase; 3] = [Phase::Draft, Phase::Sent, Phase::Read];
const REFERRAL_EDGES: [ReferralEdge; 3] = [ReferralEdge::Send, ReferralEdge::Read, ReferralEdge::Amend];

impl Referral {
    /// Spans every field's boundaries: colliding interned text, references to missing targets at
    /// two homes, every reference and leaf variant (covering indexes `0..7` reach them under a
    /// present option), absent options above, times with sub-microsecond digits either side of the
    /// epoch, and the whole storable `u64` and the whole `u16` range.
    pub fn generate(tc: &TestCase, pick: Pick) -> Referral {
        Referral {
            sender: AgentName(name(tc)),
            session: reference(tc),
            in_reply_to: pick.some(tc, 8).then(|| reference(tc)),
            references: list(tc, pick, 3, |pick| Reference::generate(tc, pick)),
            subject: pick.some(tc, 7).then(|| LeafAddress::generate(tc, pick)),
            condition: pick.some(tc, 7).then(|| Leaf::generate(tc, pick)),
            sent_at: pick.some(tc, 8).then(|| time(tc)),
            seq: seq(tc),
            ordinal: tc.draw(gs::integers::<u16>()),
            phase: State::new(code_enum(tc, pick, &PHASES), timestamp(tc)),
        }
    }

    /// A fixed referral for example tests: no options, no history, a draft.
    pub fn sample() -> Referral {
        Referral {
            sender: AgentName("alice".into()),
            session: Ref::new("session", super::LOCAL),
            in_reply_to: None,
            references: Vec::new(),
            subject: None,
            condition: None,
            sent_at: None,
            seq: 0,
            ordinal: 0,
            phase: State::new(Phase::Draft, Timestamp(0)),
        }
    }
}

impl Fixture for Referral {
    /// Every reference variant in the child table and the subject, every leaf variant, and every
    /// code at every position across five rounds.
    const COVERING: usize = SHAPES * 5;

    fn generate(tc: &TestCase, pick: Pick) -> Referral {
        Referral::generate(tc, pick)
    }

    /// The spec drawn afresh; the phase kept.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> Referral {
        Referral { phase: self.phase, ..Referral::generate(tc, pick) }
    }

    /// The phase written whole (a code-enum state: any phase, as the event says), and sometimes the
    /// subject set (a multi-column data enum, absent or present).
    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<Referral>, Referral)> {
        let phase = State::new(code_enum(tc, pick.nth(1), &PHASES), timestamp(tc));
        let event = event(tc, code_enum(tc, pick, &REFERRAL_EDGES), self.phase.value, phase.value, payload(tc, pick, text(tc)));
        let mut edge = Edge::new(Referral::PHASE, &phase, &event).expect("edge");
        let mut next = Referral { phase, ..self.clone() };
        if pick.some(tc, 6) {
            let subject = pick.nth(2).some(tc, 8).then(|| LeafAddress::generate(tc, pick.nth(3)));
            edge = edge.set(Referral::SUBJECT, &subject).expect("subject");
            next.subject = subject;
        }
        Some((edge, next))
    }

    fn stored(&self) -> Referral {
        Referral { sent_at: self.sent_at.map(floor), ..self.clone() }
    }

    fn stamped(&self, _before: Option<&Referral>, _version: u64) -> Referral {
        self.clone()
    }
}

/// A synthetic convoy-like kind: frozen documents, inputs, and deliverables whose evidence is
/// records too, changing only through declared edges (#3320 ruling 2).
#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Dossier", code = 11)]
pub struct Dossier {
    #[store(frozen)]
    pub workflow: Frozen<WorkflowSnapshot>,
    #[store(frozen)]
    pub placement: Option<Frozen<PlacementSnapshot>>,
    pub instruction: Option<String>,
    #[store(child)]
    pub inputs: BTreeMap<String, InputValue>,
    #[store(records)]
    pub deliverables: Records<Deliverable>,
    #[store(state)]
    pub state: State<DossierState>,
}

/// One edge event of the dossier or one of its deliverables: its log entry's header and payload.
pub type DossierEvent = EdgeEvent<DossierState, DossierEdgeName, Payload<DossierEdge>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum DossierEdgeName {
    #[store(code = 1)]
    Deliver,
    #[store(code = 2)]
    Abandon,
    #[store(code = 3)]
    Promise,
    #[store(code = 4)]
    Submit,
    #[store(code = 5)]
    Retract,
}

/// The edges' arguments, dossier and deliverable edges in one log; `Promise` appends a
/// deliverable, `Submit` appends evidence under one, `Retract` moves one's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DossierEdge {
    Deliver,
    Abandon { by: String, reason: String },
    Promise { deliverable: u32 },
    Submit { deliverable: u32 },
    Retract { deliverable: u32, reason: String },
}

impl DossierEdge {
    pub fn name(&self) -> DossierEdgeName {
        match self {
            DossierEdge::Deliver => DossierEdgeName::Deliver,
            DossierEdge::Abandon { .. } => DossierEdgeName::Abandon,
            DossierEdge::Promise { .. } => DossierEdgeName::Promise,
            DossierEdge::Submit { .. } => DossierEdgeName::Submit,
            DossierEdge::Retract { .. } => DossierEdgeName::Retract,
        }
    }
}

/// A frozen document: any serde type. Nested maps and lists stand for a workflow's step graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowSnapshot {
    pub name: String,
    pub revision: String,
    pub steps: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementSnapshot {
    pub host: String,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub enum InputValue {
    #[store(code = 1)]
    Text { value: String },
    #[store(code = 2)]
    Number { value: i64 },
    #[store(code = 3)]
    List {
        #[store(child)]
        items: Vec<String>,
    },
    #[store(code = 4)]
    Unset,
}

#[derive(Debug, Clone, PartialEq, Stored)]
pub struct Deliverable {
    #[store(intern)]
    pub label: String,
    pub kind: PromiseKind,
    pub source: PromiseSource,
    #[store(records)]
    pub evidence: Records<Evidence>,
    #[store(state)]
    pub state: State<DeliverableState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum PromiseKind {
    #[store(code = 1)]
    ChangeRequest,
    #[store(code = 2)]
    Artifact,
}

#[derive(Debug, Clone, PartialEq, Stored)]
pub enum PromiseSource {
    #[store(code = 1)]
    Workflow {
        #[store(intern)]
        step: String,
    },
    #[store(code = 2)]
    Dispatcher,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub struct Evidence {
    pub reference: String,
    pub submitted_at: Timestamp,
    #[store(flatten)]
    pub verdict: Option<Verdict>,
    #[store(child)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub struct Verdict {
    pub accepted: bool,
    #[store(intern)]
    pub by: String,
    pub at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum DossierState {
    #[store(code = 1)]
    Open,
    #[store(code = 2)]
    Delivered,
    #[store(code = 3)]
    Abandoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum DeliverableState {
    #[store(code = 1)]
    Open,
    #[store(code = 2)]
    Retracted,
}

/// The writes a generated edge adds after its event.
type Changes = dyn FnOnce(Edge<Dossier>) -> Result<Edge<Dossier>, EdgeError>;

/// One of two workflow documents (by `n`), so equal snapshots are common.
pub fn workflow(n: usize) -> Frozen<WorkflowSnapshot> {
    let steps = (0..=n).map(|step| (format!("step-{step}"), (0..step).map(|dep| format!("step-{dep}")).collect())).collect();
    Frozen::new(WorkflowSnapshot { name: "review".into(), revision: format!("r{n}"), steps }).expect("freeze")
}

pub fn placement(tc: &TestCase) -> Frozen<PlacementSnapshot> {
    let labels = [("zone".to_owned(), name(tc))].into();
    Frozen::new(PlacementSnapshot { host: name(tc), labels }).expect("freeze")
}

impl InputValue {
    /// Every variant in turn when covering, the list variant with each size.
    fn generate(tc: &TestCase, pick: Pick) -> InputValue {
        match pick.variant(tc, 4, |index| index) {
            0 => InputValue::Text { value: text(tc) },
            1 => InputValue::Number { value: tc.draw(gs::integers::<i64>()) },
            2 => InputValue::List { items: list(tc, pick.nth(1), 2, |_| text(tc)) },
            _ => InputValue::Unset,
        }
    }
}

impl Evidence {
    pub fn generate(tc: &TestCase, pick: Pick) -> Evidence {
        Evidence {
            reference: text(tc),
            submitted_at: timestamp(tc),
            verdict: pick.some(tc, 8).then(|| Verdict { accepted: tc.draw(gs::booleans()), by: name(tc), at: timestamp(tc) }),
            metadata: map(tc, pick, 2, |_| text(tc)),
        }
    }
}

const DOSSIER_STATES: [DossierState; 3] = [DossierState::Open, DossierState::Delivered, DossierState::Abandoned];

impl Deliverable {
    pub fn generate(tc: &TestCase, pick: Pick) -> Deliverable {
        Deliverable { evidence: records(tc, pick, 2, |pick| Evidence::generate(tc, pick)), ..Deliverable::spec(tc, pick) }
    }

    /// The deliverable's own fields, with no evidence.
    fn spec(tc: &TestCase, pick: Pick) -> Deliverable {
        Deliverable {
            label: name(tc),
            kind: code_enum(tc, pick, &[PromiseKind::ChangeRequest, PromiseKind::Artifact]),
            source: match pick.variant(tc, 2, |index| index) {
                0 => PromiseSource::Workflow { step: name(tc) },
                _ => PromiseSource::Dispatcher,
            },
            evidence: Records::new(),
            state: State::new(code_enum(tc, pick, &[DeliverableState::Open, DeliverableState::Retracted]), timestamp(tc)),
        }
    }

    fn stamped(&self, before: Option<&Deliverable>, version: u64) -> Deliverable {
        Deliverable { evidence: stamp_flat(&self.evidence, before.map(|before| &before.evidence), version), ..self.clone() }
    }
}

impl Fixture for Dossier {
    /// Every variant and option of the dossier, its inputs, deliverables and evidence in each round
    /// of [`SHAPES`]; five rounds give every code enum every code.
    const COVERING: usize = SHAPES * 5;

    fn generate(tc: &TestCase, pick: Pick) -> Dossier {
        Dossier {
            workflow: workflow(pick.variant(tc, 2, |index| index)),
            placement: pick.some(tc, 9).then(|| placement(tc)),
            instruction: pick.some(tc, 7).then(|| text(tc)),
            inputs: map(tc, pick, 3, |pick| InputValue::generate(tc, pick)),
            deliverables: records(tc, pick, 2, |pick| Deliverable::generate(tc, pick)),
            state: State::new(code_enum(tc, pick, &DOSSIER_STATES), timestamp(tc)),
        }
    }

    /// The spec and child tables drawn afresh; the state and deliverables kept.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> Dossier {
        Dossier { deliverables: self.deliverables.clone(), state: self.state, ..Dossier::generate(tc, pick) }
    }

    /// A dossier edge (deliver or abandon) moving its state, or a deliverable edge, which writes
    /// the dossier's state unchanged: promising a deliverable (with its initial evidence: records
    /// nested in an appended record), submitting evidence under an existing one, or retracting one
    /// (its state, on its records row). Deliverable edges on a dossier without deliverables
    /// promise one.
    fn edge(&self, tc: &TestCase, pick: Pick) -> Option<(Edge<Dossier>, Dossier)> {
        use DossierState::*;
        let mut choice = pick.variant(tc, 5, |index| index);
        if self.deliverables.is_empty() && choice >= 3 {
            choice = 2;
        }
        let mut next = self.clone();
        let (args, changes): (DossierEdge, Box<Changes>) = match choice {
            0 | 1 => {
                let edge = if choice == 0 { DossierEdge::Deliver } else { DossierEdge::Abandon { by: name(tc), reason: text(tc) } };
                next.state = State::new(if choice == 0 { Delivered } else { Abandoned }, timestamp(tc));
                (edge, Box::new(Ok))
            }
            2 => {
                let deliverable = Deliverable::generate(tc, pick.nth(1));
                let position = u32::try_from(self.deliverables.len()).expect("small fixture");
                next.deliverables = appended(tc, &self.deliverables, deliverable.clone());
                (DossierEdge::Promise { deliverable: position }, Box::new(move |edge| edge.append(Dossier::DELIVERABLES, &deliverable)))
            }
            3 => {
                let at = position(tc, pick.nth(1), &self.deliverables);
                let evidence = Evidence::generate(tc, pick.nth(2));
                next.deliverables = replaced(&self.deliverables, at, |deliverable| Deliverable {
                    evidence: appended(tc, &deliverable.evidence, evidence.clone()),
                    ..deliverable.clone()
                });
                let edge = DossierEdge::Submit { deliverable: at.0 };
                (edge, Box::new(move |edge| edge.append_in(Dossier::DELIVERABLES, at, Deliverable::EVIDENCE, &evidence)))
            }
            _ => {
                let at = position(tc, pick.nth(1), &self.deliverables);
                let state = State::new(DeliverableState::Retracted, timestamp(tc));
                next.deliverables = replaced(&self.deliverables, at, |deliverable| Deliverable { state, ..deliverable.clone() });
                let edge = DossierEdge::Retract { deliverable: at.0, reason: text(tc) };
                (edge, Box::new(move |edge| edge.set_in(Dossier::DELIVERABLES, at, Deliverable::STATE, &state)))
            }
        };
        let event = event(tc, args.name(), self.state.value, next.state.value, payload(tc, pick, args));
        let edge = Edge::new(Dossier::STATE, &next.state, &event).and_then(changes).expect("edge");
        Some((edge, next))
    }

    fn stamped(&self, before: Option<&Dossier>, version: u64) -> Dossier {
        let deliverables = stamp(&self.deliverables, before.map(|before| &before.deliverables), version, |deliverable, before| {
            deliverable.stamped(before, version)
        });
        Dossier { deliverables, ..self.clone() }
    }
}

impl Dossier {
    /// A fixed dossier for example tests: the given workflow, no inputs, no deliverables.
    pub fn sample(workflow_n: usize) -> Dossier {
        Dossier {
            workflow: workflow(workflow_n),
            placement: None,
            instruction: None,
            inputs: BTreeMap::new(),
            deliverables: Records::new(),
            state: State::new(DossierState::Open, Timestamp(0)),
        }
    }
}
