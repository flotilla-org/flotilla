//! The Convoy-shaped fixture, transcribed from the #3208 §4.1 sketch (itself written from
//! `flotilla_resources::convoy`): frozen workflow and placement snapshots, a repositories child,
//! an inputs map of a data enum, and deliverables as `records` whose evidence is `records` too.
//!
//! Modelling decisions the storage list forces:
//! - The workflow snapshot is a serde document with nested maps and lists, stored once per
//!   distinct content in `objects`; the convoy row holds its hash.
//! - An input value that is a list (`InputValue::List`) is a child table of the inputs table,
//!   with rows only while that variant is active.
//! - Evidence metadata is a child map under evidence under deliverables: three levels of keys.
//! - The convoy and deliverable machine states are `state` columns; the edges themselves are the
//!   engine stack's work and are not part of this fixture.

use std::collections::BTreeMap;

use flotilla_schema::{Frozen, Records, Ref, State, Stored, Timestamp};
use hegel::{generators as gs, TestCase};
use serde::{Deserialize, Serialize};

use super::{code_enum, evolve_records, key, list, name, reference, text, timestamp, Fixture, Pick, SHAPES};
use crate::fixtures::AgentSession;

/// A minimal target kind for `Convoy::project`.
#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Project", code = 4)]
pub struct Project {
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Convoy", code = 5)]
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
    #[store(state)]
    pub state: State<ConvoyState>,
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

impl Deliverable {
    fn generate(tc: &TestCase, pick: Pick) -> Deliverable {
        Deliverable { evidence: list(tc, pick, 2, |pick| Evidence::generate(tc, pick)).into(), ..Deliverable::spec(tc, pick) }
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

    /// Replaced in place: fields drawn afresh, evidence kept append-only (records in records).
    fn evolve(&self, tc: &TestCase, pick: Pick) -> Deliverable {
        let replace = |_: &Evidence, pick: Pick| Evidence::generate(tc, pick);
        Deliverable {
            evidence: evolve_records(tc, pick, &self.evidence, Some(&replace), |pick| Evidence::generate(tc, pick)),
            ..Deliverable::spec(tc, pick)
        }
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
            deliverables: list(tc, pick, 2, |pick| Deliverable::generate(tc, pick)).into(),
            state: State::new(code_enum(tc, pick, &[Open, Delivered, Abandoned]), timestamp(tc)),
        }
    }

    /// Spec and child tables drawn afresh; deliverables replaced in place (their evidence evolving
    /// append-only) or appended.
    fn evolve(&self, tc: &TestCase, pick: Pick) -> Convoy {
        let spec = Convoy::generate(tc, pick);
        let replace = |deliverable: &Deliverable, pick: Pick| deliverable.evolve(tc, pick);
        Convoy {
            deliverables: evolve_records(tc, pick, &self.deliverables, Some(&replace), |pick| Deliverable::generate(tc, pick)),
            ..spec
        }
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
            state: State::new(ConvoyState::Open, Timestamp(0)),
        }
    }
}
