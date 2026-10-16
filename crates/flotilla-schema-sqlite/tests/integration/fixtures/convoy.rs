//! The Convoy-shaped fixture, transcribed from the #3208 §4.1 sketch (itself written from
//! `flotilla_resources::convoy`): frozen workflow and placement snapshots, a repositories child,
//! an inputs map of a data enum, and deliverables as `records` whose evidence is `records` too:
//! rows that change only through a declared edge (#3320 ruling 2), so a whole-value write only
//! appends deliverables, and evidence to new or kept deliverables.
//!
//! Modelling decisions the storage list forces:
//! - The workflow snapshot is a serde document with nested maps and lists, stored once per
//!   distinct content in `objects`; the convoy row holds its hash.
//! - An input value that is a list (`InputValue::List`) is a child table of the inputs table,
//!   with rows only while that variant is active.
//! - Evidence metadata is a child map under evidence under deliverables: three levels of keys.
//! - The convoy and deliverable machine states are `state` columns. One edge log holds the events
//!   of both machines (#3208 §4.1 `convoy_edge`): an [`EdgeEvent`] header labelled by the convoy's
//!   state, and a document whose arguments name a deliverable by position. Every edge writes the
//!   convoy's state value, unchanged by a deliverable edge.

use std::collections::BTreeMap;

use flotilla_schema::{Edge, EdgeError, EdgeEvent, Frozen, Records, Ref, State, Stored, Timestamp};
use hegel::{generators as gs, TestCase};
use serde::{Deserialize, Serialize};

use super::{
    appended, code_enum, event, key, list, name, payload, position, records, reference, replaced, stamp, text, timestamp, Fixture, Payload,
    Pick, SHAPES,
};
use crate::fixtures::AgentSession;

/// A minimal target kind for `Convoy::project`.
#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Project", code = 4)]
pub struct Project {
    pub title: String,
}

/// The #3208 §4.1 index: a project's open convoys.
#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Convoy", code = 5, index(name = "open_by_project", eq(project), when(state in [Open], project is_some)))]
pub struct Convoy {
    pub project: Option<Ref<Project>>,
    #[store(intern)]
    pub workflow_ref: String,
    #[store(frozen)]
    pub workflow_snapshot: Frozen<WorkflowSnapshot>,
    #[store(frozen)]
    pub placement_snapshot: Option<Frozen<PlacementSnapshot>>,
    #[store(intern)]
    pub dispatching_principal: PrincipalRef,
    pub instruction: Option<String>,
    #[store(child)]
    pub repositories: Vec<ConvoyRepository>,
    #[store(child)]
    pub inputs: BTreeMap<String, InputValue>,
    #[store(records)]
    pub deliverables: Records<Deliverable>,
    #[store(history)]
    pub edges: Records<ConvoyEdgeEvent>,
    #[store(state)]
    pub state: State<ConvoyState>,
}

/// The writes a generated edge adds after its event.
type Changes = dyn FnOnce(Edge<Convoy>) -> Result<Edge<Convoy>, EdgeError>;

/// One edge event of the convoy or one of its deliverables.
pub type ConvoyEdgeEvent = EdgeEvent<ConvoyState, ConvoyEdgeName, Payload<ConvoyEdge>>;

/// The edge column of the convoy's edge log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum ConvoyEdgeName {
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

/// The #3208 §4.1 edges' arguments, convoy and deliverable machines in one log; `Promise` appends
/// a deliverable, `Submit` appends evidence under one, `Retract` moves one's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConvoyEdge {
    Deliver,
    Abandon { by: String, reason: String },
    Promise { deliverable: u32 },
    Submit { deliverable: u32 },
    Retract { deliverable: u32, reason: String },
}

impl ConvoyEdge {
    pub fn name(&self) -> ConvoyEdgeName {
        match self {
            ConvoyEdge::Deliver => ConvoyEdgeName::Deliver,
            ConvoyEdge::Abandon { .. } => ConvoyEdgeName::Abandon,
            ConvoyEdge::Promise { .. } => ConvoyEdgeName::Promise,
            ConvoyEdge::Submit { .. } => ConvoyEdgeName::Submit,
            ConvoyEdge::Retract { .. } => ConvoyEdgeName::Retract,
        }
    }
}

/// A frozen document: any serde type. Nested maps and lists stand for the workflow's step graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowSnapshot {
    pub name: String,
    pub revision: String,
    pub steps: BTreeMap<String, Vec<String>>,
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementSnapshot {
    pub host: String,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub struct PrincipalRef(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub struct ConvoyRepository {
    pub url: String,
    #[store(intern)]
    pub repo_key: String,
    pub source_ref: String,
    pub target_ref: String,
    pub workspace_slug: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
pub enum InputValue {
    #[store(code = 1)]
    Text { value: String },
    #[store(code = 2)]
    Number { value: i64 },
    #[store(code = 3)]
    Flag { value: bool },
    #[store(code = 4)]
    List {
        #[store(child)]
        items: Vec<String>,
    },
    #[store(code = 5)]
    Unset,
}

#[derive(Debug, Clone, PartialEq, Stored)]
pub struct Deliverable {
    #[store(intern)]
    pub label: String,
    pub kind: PromiseKind,
    #[store(intern)]
    pub role: String,
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
    #[store(code = 3)]
    Report,
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
    #[store(code = 3)]
    Agent { session: Ref<AgentSession> },
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
    pub why: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
pub enum ConvoyState {
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

/// One of two workflow documents (by `n`), so equal snapshots are common.
pub fn workflow(n: usize) -> Frozen<WorkflowSnapshot> {
    let steps = (0..=n).map(|step| (format!("step-{step}"), (0..step).map(|dep| format!("step-{dep}")).collect())).collect();
    Frozen::new(WorkflowSnapshot {
        name: "review".into(),
        revision: format!("r{n}"),
        steps,
        roles: vec!["coder".into(), "reviewer".into()],
    })
    .expect("freeze")
}

fn placement(tc: &TestCase) -> Frozen<PlacementSnapshot> {
    let labels = [("zone".to_owned(), name(tc))].into();
    Frozen::new(PlacementSnapshot { host: name(tc), labels }).expect("freeze")
}

/// Drawn keys collide (and so dedupe); covering keys are distinct, so every covering element and
/// its variant is kept.
pub fn map<V>(tc: &TestCase, pick: Pick, max: usize, mut value: impl FnMut(Pick) -> V) -> BTreeMap<String, V> {
    (0..pick.count(tc, max))
        .map(|n| {
            let key = match pick {
                Pick::Draw => key(tc),
                Pick::Cover(_) => format!("k{n}"),
            };
            (key, value(pick.nth(n)))
        })
        .collect()
}

impl InputValue {
    /// Every variant in turn when covering, the list variant with each size.
    fn generate(tc: &TestCase, pick: Pick) -> InputValue {
        match pick.variant(tc, 5, |index| index) {
            0 => InputValue::Text { value: text(tc) },
            1 => InputValue::Number { value: tc.draw(gs::integers::<i64>()) },
            2 => InputValue::Flag { value: tc.draw(gs::booleans()) },
            3 => InputValue::List { items: list(tc, pick.nth(1), 2, |_| text(tc)) },
            _ => InputValue::Unset,
        }
    }
}

impl Evidence {
    fn generate(tc: &TestCase, pick: Pick) -> Evidence {
        Evidence {
            reference: text(tc),
            submitted_at: timestamp(tc),
            verdict: pick.some(tc, 8).then(|| Verdict {
                accepted: tc.draw(gs::booleans()),
                by: name(tc),
                at: timestamp(tc),
                why: text(tc),
            }),
            metadata: map(tc, pick, 2, |_| text(tc)),
        }
    }
}

/// Every edge in turn when covering, between any labels.
fn edge_event(tc: &TestCase, pick: Pick) -> ConvoyEdgeEvent {
    use ConvoyState::*;
    let deliverable = || tc.draw(gs::integers::<u32>());
    let args = match pick.variant(tc, 5, |index| index) {
        0 => ConvoyEdge::Deliver,
        1 => ConvoyEdge::Abandon { by: name(tc), reason: text(tc) },
        2 => ConvoyEdge::Promise { deliverable: deliverable() },
        3 => ConvoyEdge::Submit { deliverable: deliverable() },
        _ => ConvoyEdge::Retract { deliverable: deliverable(), reason: text(tc) },
    };
    let labels = [Open, Delivered, Abandoned];
    event(tc, args.name(), code_enum(tc, pick, &labels), code_enum(tc, pick.nth(1), &labels), payload(tc, pick, args))
}

impl Deliverable {
    fn generate(tc: &TestCase, pick: Pick) -> Deliverable {
        Deliverable { evidence: records(tc, pick, 2, |pick| Evidence::generate(tc, pick)), ..Deliverable::spec(tc, pick) }
    }

    /// The deliverable's own fields, with no evidence.
    fn spec(tc: &TestCase, pick: Pick) -> Deliverable {
        use DeliverableState::*;
        use PromiseKind::*;
        Deliverable {
            label: name(tc),
            kind: code_enum(tc, pick, &[ChangeRequest, Artifact, Report]),
            role: name(tc),
            source: match pick.variant(tc, 3, |index| index) {
                0 => PromiseSource::Workflow { step: name(tc) },
                1 => PromiseSource::Dispatcher,
                _ => PromiseSource::Agent { session: reference(tc) },
            },
            evidence: Records::new(),
            state: State::new(code_enum(tc, pick, &[Open, Retracted]), timestamp(tc)),
        }
    }

    fn stamped(&self, before: Option<&Deliverable>, version: u64) -> Deliverable {
        let evidence = stamp(&self.evidence, before.map(|before| &before.evidence), version, |evidence, _| evidence.clone());
        Deliverable { evidence, ..self.clone() }
    }
}

/// Every variant and option of the convoy, its inputs, deliverables and evidence in each round of
/// [`SHAPES`]; five rounds give every code enum every code.
pub const COVERING: usize = SHAPES * 5;

impl Fixture for Convoy {
    const COVERING: usize = COVERING;

    fn generate(tc: &TestCase, pick: Pick) -> Convoy {
        use ConvoyState::*;
        Convoy {
            project: pick.some(tc, 8).then(|| reference(tc)),
            workflow_ref: name(tc),
            workflow_snapshot: workflow(pick.variant(tc, 2, |index| index)),
            placement_snapshot: pick.some(tc, 9).then(|| placement(tc)),
            dispatching_principal: PrincipalRef(name(tc)),
            instruction: pick.some(tc, 7).then(|| text(tc)),
            repositories: list(tc, pick, 2, |_| ConvoyRepository {
                url: text(tc),
                repo_key: name(tc),
                source_ref: text(tc),
                target_ref: text(tc),
                workspace_slug: text(tc),
            }),
            inputs: map(tc, pick, 3, |pick| InputValue::generate(tc, pick)),
            deliverables: records(tc, pick, 2, |pick| Deliverable::generate(tc, pick)),
            edges: records(tc, pick, 2, |pick| edge_event(tc, pick)),
            state: State::new(code_enum(tc, pick, &[Open, Delivered, Abandoned]), timestamp(tc)),
        }
    }

    /// Spec and child tables drawn afresh; state, deliverables and edge events kept.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> Convoy {
        Convoy { deliverables: self.deliverables.clone(), edges: self.edges.clone(), state: self.state, ..Convoy::generate(tc, pick) }
    }

    /// A convoy edge (deliver or abandon) moving the convoy's state, or a deliverable edge, which
    /// writes the convoy's state unchanged: promising a deliverable (with its initial evidence:
    /// records nested in an appended record), submitting evidence under an existing one, or
    /// retracting one (its state, on its records row). Deliverable edges on a convoy without
    /// deliverables promise one.
    fn edge(&self, tc: &TestCase, pick: Pick) -> (Edge<Convoy>, Convoy) {
        use ConvoyState::*;
        let mut choice = pick.variant(tc, 5, |index| index);
        if self.deliverables.is_empty() && choice >= 3 {
            choice = 2;
        }
        let mut next = self.clone();
        let (args, changes): (ConvoyEdge, Box<Changes>) = match choice {
            0 | 1 => {
                let edge = if choice == 0 { ConvoyEdge::Deliver } else { ConvoyEdge::Abandon { by: name(tc), reason: text(tc) } };
                next.state = State::new(if choice == 0 { Delivered } else { Abandoned }, timestamp(tc));
                (edge, Box::new(Ok))
            }
            2 => {
                let deliverable = Deliverable::generate(tc, pick.nth(1));
                let position = u32::try_from(self.deliverables.len()).expect("small fixture");
                next.deliverables = appended(tc, &self.deliverables, deliverable.clone());
                (ConvoyEdge::Promise { deliverable: position }, Box::new(move |edge| edge.append(Convoy::DELIVERABLES, &deliverable)))
            }
            3 => {
                let at = position(tc, pick.nth(1), &self.deliverables);
                let evidence = Evidence::generate(tc, pick.nth(2));
                next.deliverables = replaced(&self.deliverables, at, |deliverable| Deliverable {
                    evidence: appended(tc, &deliverable.evidence, evidence.clone()),
                    ..deliverable.clone()
                });
                let edge = ConvoyEdge::Submit { deliverable: at.0 };
                (edge, Box::new(move |edge| edge.append_in(Convoy::DELIVERABLES, at, Deliverable::EVIDENCE, &evidence)))
            }
            _ => {
                let at = position(tc, pick.nth(1), &self.deliverables);
                let state = State::new(DeliverableState::Retracted, timestamp(tc));
                next.deliverables = replaced(&self.deliverables, at, |deliverable| Deliverable { state, ..deliverable.clone() });
                let edge = ConvoyEdge::Retract { deliverable: at.0, reason: text(tc) };
                (edge, Box::new(move |edge| edge.set_in(Convoy::DELIVERABLES, at, Deliverable::STATE, &state)))
            }
        };
        let event = event(tc, args.name(), self.state.value, next.state.value, payload(tc, pick, args));
        let edge = Edge::new(Convoy::STATE, &next.state, Convoy::EDGES, &event).and_then(changes).expect("edge");
        next.edges = appended(tc, &self.edges, event);
        (edge, next)
    }

    fn stamped(&self, before: Option<&Convoy>, version: u64) -> Convoy {
        let deliverables = stamp(&self.deliverables, before.map(|before| &before.deliverables), version, |deliverable, before| {
            deliverable.stamped(before, version)
        });
        let edges = stamp(&self.edges, before.map(|before| &before.edges), version, |event, _| event.clone());
        Convoy { deliverables, edges, ..self.clone() }
    }
}

impl Convoy {
    /// A fixed convoy for example tests: the given workflow, no inputs, no deliverables.
    pub fn sample(workflow_n: usize) -> Convoy {
        Convoy {
            project: None,
            workflow_ref: "review".into(),
            workflow_snapshot: workflow(workflow_n),
            placement_snapshot: None,
            dispatching_principal: PrincipalRef("alice".into()),
            instruction: None,
            repositories: Vec::new(),
            inputs: BTreeMap::new(),
            deliverables: Records::new(),
            edges: Records::new(),
            state: State::new(ConvoyState::Open, Timestamp(0)),
        }
    }
}
