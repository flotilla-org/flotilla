//! The catalog projection: query result-set rows in, metadata patches out.
//!
//! Pure functions — no I/O, no clock. The connector loop feeds it the
//! fleet-merged `convoys` and `sessions` rows and sends what comes back.
//!
//! Truthfulness rules (design §7 on flotilla-org/flotilla#667):
//! - every path is published honestly; the PM's resolver decides
//!   latent-vs-live by whether a materialised member carries the identity —
//!   the connector never says "latent";
//! - entities without a daemon-resolvable attach list without a recipe
//!   (truthfully unmaterialisable) rather than with one that would fail;
//! - failed things carry their message — never a masked "initializing…".

use std::collections::BTreeMap;

use flotilla_protocol::result_set::{ConvoyPhase, ConvoyRow, SessionPhase, SessionRow, VesselRow, WorkPhase};

use crate::{
    keys::{
        ARCHIPELAGO_ORDINAL, CATALOG_TTL_MS, KEY_CONVOY_MESSAGE, KEY_CONVOY_PHASE, KEY_CONVOY_WORKFLOW, KEY_CREW_ROLES, KEY_FACTORY_ID,
        KEY_MATERIALIZE_RECIPE, KEY_MATERIALIZE_TARGET, KEY_PROJECT_NAME, KEY_SCOPE, KEY_SESSION, KEY_STATUS_ATTENTION, KEY_STATUS_STATE,
        KEY_SUMMARY_TEXT, KEY_VESSEL_HOST, KEY_WORK_PHASE, SEGMENT_CONVOY, SEGMENT_PROJECT, SEGMENT_VESSEL, SOURCE_CONNECTOR,
    },
    recipe::RecipeMint,
    wire::{GroupPath, GroupSegment, MetadataIdentity, MetadataPatch, MetadataTarget, MetadataValue, MetadataValueUpdate},
};

/// The rows the catalog is projected from.
pub struct CatalogInput<'a> {
    pub convoys: &'a [ConvoyRow],
    pub sessions: &'a [SessionRow],
}

/// Frozen badge vocabulary (design §6, proposed for the Leg-1 freeze).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeState {
    Idle,
    Waiting,
    Active,
    Done,
    Failed,
}

impl BadgeState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Waiting => "waiting",
            Self::Active => "active",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// Badge for a convoy. `initializing` is already `Pending` on rows — both
/// read as waiting, never a masked "initializing…".
pub fn convoy_badge(phase: ConvoyPhase) -> (BadgeState, bool) {
    match phase {
        ConvoyPhase::Pending => (BadgeState::Waiting, false),
        ConvoyPhase::Active => (BadgeState::Active, false),
        ConvoyPhase::Completed => (BadgeState::Done, false),
        ConvoyPhase::Failed => (BadgeState::Failed, true),
        ConvoyPhase::Cancelled => (BadgeState::Idle, false),
    }
}

/// Badge for the work aboard a vessel. `Ready` demands attention: the crew
/// could start and hasn't.
pub fn work_badge(phase: WorkPhase) -> (BadgeState, bool) {
    match phase {
        WorkPhase::Pending => (BadgeState::Idle, false),
        WorkPhase::Ready => (BadgeState::Waiting, true),
        WorkPhase::Launching | WorkPhase::Running => (BadgeState::Active, false),
        WorkPhase::Complete => (BadgeState::Done, false),
        WorkPhase::Failed => (BadgeState::Failed, true),
        WorkPhase::Cancelled => (BadgeState::Idle, false),
    }
}

pub fn session_badge(phase: SessionPhase) -> (BadgeState, bool) {
    match phase {
        SessionPhase::Starting => (BadgeState::Waiting, false),
        SessionPhase::Running => (BadgeState::Active, false),
        SessionPhase::Stopped => (BadgeState::Idle, false),
        SessionPhase::Failed => (BadgeState::Failed, true),
    }
}

/// Everything the connector currently asserts, as resolved facts per target.
/// Two catalogs diff to the patches that move a PM from one to the other.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    facts: BTreeMap<MetadataTarget, BTreeMap<String, MetadataValueUpdate>>,
}

impl Catalog {
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }

    /// Full re-assertion — one patch per target, refreshing every TTL.
    pub fn reassert_patches(&self) -> Vec<MetadataPatch> {
        self.facts.iter().map(|(target, facts)| patch(target.clone(), facts.clone(), vec![])).collect()
    }

    /// The patches that move a PM holding `previous` to this catalog:
    /// changed/added keys are set, vanished keys and targets are explicitly
    /// unset (snappier than waiting for the TTL backstop).
    pub fn diff_patches(&self, previous: &Catalog) -> Vec<MetadataPatch> {
        let mut patches = Vec::new();
        for (target, facts) in &self.facts {
            let prior = previous.facts.get(target);
            let set: BTreeMap<String, MetadataValueUpdate> = facts
                .iter()
                .filter(|(key, update)| prior.and_then(|prior| prior.get(*key)) != Some(*update))
                .map(|(key, update)| (key.clone(), update.clone()))
                .collect();
            let unset: Vec<String> =
                prior.map(|prior| prior.keys().filter(|key| !facts.contains_key(*key)).cloned().collect()).unwrap_or_default();
            if !set.is_empty() || !unset.is_empty() {
                patches.push(patch(target.clone(), set, unset));
            }
        }
        for (target, facts) in &previous.facts {
            if !self.facts.contains_key(target) {
                patches.push(patch(target.clone(), BTreeMap::new(), facts.keys().cloned().collect()));
            }
        }
        patches
    }

    fn assert_facts(&mut self, target: MetadataTarget, facts: Vec<(&str, MetadataValue)>, ordinal: Option<i64>) {
        let entry = self.facts.entry(target).or_default();
        for (key, value) in facts {
            let mut update = MetadataValueUpdate::new(value, Some(CATALOG_TTL_MS));
            update.ordinal = ordinal;
            entry.insert(key.to_owned(), update);
        }
    }
}

fn patch(target: MetadataTarget, set: BTreeMap<String, MetadataValueUpdate>, unset: Vec<String>) -> MetadataPatch {
    MetadataPatch { target, source_id: SOURCE_CONNECTOR.to_owned(), set, unset }
}

/// Project the fleet-merged rows into the catalog the PM should hold.
pub fn project_catalog(input: &CatalogInput<'_>, mint: &dyn RecipeMint) -> Catalog {
    let mut catalog = Catalog::default();
    for convoy in input.convoys {
        project_convoy(&mut catalog, convoy, mint);
    }
    for session in input.sessions {
        project_session(&mut catalog, session, mint);
    }
    catalog
}

/// The project-level segment: `project_ref` when set (the project→convoy
/// spine's primary key), repo as fallback — under the *same* segment key the
/// git-watcher publishes, so both producers' groups collide into one project
/// cluster. Neither ⇒ `None`: the entity is truthfully unparented at
/// archipelago level.
pub fn project_segment(project_ref: Option<&str>, repo: Option<&str>) -> Option<GroupSegment> {
    let value = project_ref.or(repo)?;
    let label = value.rsplit('/').next().filter(|short| !short.is_empty() && *short != value);
    let segment = GroupSegment::text(SEGMENT_PROJECT, value);
    Some(match label {
        Some(label) => segment.with_label(label),
        None => segment,
    })
}

/// The GroupPath of a convoy — shared spine construction so every producer
/// lands on the same group node.
pub fn convoy_group_path(project: Option<GroupSegment>, namespace: &str, convoy: &str) -> GroupPath {
    let mut path = Vec::new();
    if let Some(segment) = project {
        path.push(segment);
    }
    path.push(GroupSegment::text(SEGMENT_CONVOY, format!("{namespace}/{convoy}")).with_label(convoy.to_owned()));
    GroupPath(path)
}

/// The GroupPath of a convoy-hosted vessel — shared by the catalog and the
/// actuator's tab stamp so both producers land on the same group node.
pub fn vessel_group_path(project: Option<GroupSegment>, namespace: &str, convoy: &str, vessel: &str) -> GroupPath {
    let mut path = convoy_group_path(project, namespace, convoy);
    path.0.push(GroupSegment::text(SEGMENT_VESSEL, vessel.to_owned()));
    path
}

/// `factory.id` for a convoy-hosted vessel — the dedupe key shared by every
/// producer that materialises this node.
pub fn vessel_factory_id(namespace: &str, convoy: &str, vessel: &str) -> String {
    format!("flotilla:convoys/{namespace}/{convoy}/{vessel}")
}

fn assert_project_group(catalog: &mut Catalog, segment: &GroupSegment) {
    let MetadataValue::Text(value) = &segment.value else {
        return;
    };
    let name = segment.label.clone().unwrap_or_else(|| value.clone());
    catalog.assert_facts(
        MetadataTarget::Group(GroupPath(vec![segment.clone()])),
        vec![(KEY_PROJECT_NAME, MetadataValue::text(name)), (KEY_FACTORY_ID, MetadataValue::text(format!("flotilla:projects/{value}")))],
        None,
    );
}

fn project_convoy(catalog: &mut Catalog, convoy: &ConvoyRow, mint: &dyn RecipeMint) {
    let namespace = &convoy.resource.namespace;
    let project = project_segment(convoy.project_ref.as_deref(), convoy.repo.as_ref().map(|repo| repo.0.as_str()));
    if let Some(segment) = &project {
        assert_project_group(catalog, segment);
    }
    let ordinal = project.is_none().then_some(ARCHIPELAGO_ORDINAL);

    let convoy_path = convoy_group_path(project.clone(), namespace, &convoy.name);
    let (state, attention) = convoy_badge(convoy.phase);
    let done = convoy.vessels.iter().filter(|vessel| vessel.phase == WorkPhase::Complete).count();
    let mut facts = vec![
        (KEY_CONVOY_PHASE, MetadataValue::text(convoy.phase.as_str())),
        (KEY_CONVOY_WORKFLOW, MetadataValue::text(convoy.workflow_ref.clone())),
        (KEY_STATUS_STATE, MetadataValue::text(state.as_str())),
        (KEY_FACTORY_ID, MetadataValue::text(format!("flotilla:convoys/{namespace}/{}", convoy.name))),
    ];
    if let Some(message) = &convoy.message {
        facts.push((KEY_CONVOY_MESSAGE, MetadataValue::text(message.clone())));
    }
    if attention {
        facts.push((KEY_STATUS_ATTENTION, MetadataValue::Bool(true)));
    }
    if !convoy.vessels.is_empty() {
        facts.push((KEY_SUMMARY_TEXT, MetadataValue::text(format!("{done}/{} vessels done", convoy.vessels.len()))));
    }
    catalog.assert_facts(MetadataTarget::Group(convoy_path), facts, ordinal);

    for vessel in &convoy.vessels {
        project_vessel(catalog, convoy, vessel, project.clone(), ordinal, mint);
    }
}

fn project_vessel(
    catalog: &mut Catalog,
    convoy: &ConvoyRow,
    vessel: &VesselRow,
    project: Option<GroupSegment>,
    ordinal: Option<i64>,
    mint: &dyn RecipeMint,
) {
    let namespace = &convoy.resource.namespace;
    let path = vessel_group_path(project, namespace, &convoy.name, &vessel.name);
    let (state, attention) = work_badge(vessel.phase);
    let mut facts = vec![
        (KEY_WORK_PHASE, MetadataValue::text(vessel.phase.as_str())),
        (KEY_VESSEL_HOST, MetadataValue::text(vessel.host.to_string())),
        (KEY_STATUS_STATE, MetadataValue::text(state.as_str())),
        (KEY_FACTORY_ID, MetadataValue::text(vessel_factory_id(namespace, &convoy.name, &vessel.name))),
    ];
    if !vessel.crew.is_empty() {
        let roles = vessel.crew.iter().map(|member| member.role.clone()).collect();
        facts.push((KEY_CREW_ROLES, MetadataValue::StringList(roles)));
    }
    if attention {
        facts.push((KEY_STATUS_ATTENTION, MetadataValue::Bool(true)));
    }
    if let Some(message) = &vessel.message {
        facts.push((KEY_SUMMARY_TEXT, MetadataValue::text(message.clone())));
    }
    // Vessels materialise at workspace granularity (ADR 0012); the recipe
    // exists iff the daemon can resolve the attach — a capability fact.
    if let Some(recipe) = vessel.attach.as_deref().and_then(|attach_ref| mint.attach(attach_ref)) {
        facts.push((KEY_MATERIALIZE_TARGET, MetadataValue::text("workspace")));
        facts.push((KEY_MATERIALIZE_RECIPE, MetadataValue::text(recipe.command())));
    }
    catalog.assert_facts(MetadataTarget::Group(path), facts, ordinal);
}

fn project_session(catalog: &mut Catalog, session: &SessionRow, mint: &dyn RecipeMint) {
    let namespace = &session.resource.namespace;
    let project = project_segment(None, session.repo.as_ref().map(|repo| repo.0.as_str()));
    let mut path = Vec::new();
    if let Some(segment) = &project {
        assert_project_group(catalog, segment);
        path.push(segment.clone());
    }
    // Free-floating vessels with no project prefix are archipelago-level:
    // grouped and ordered before everything else by default (design §4;
    // ordering semantics are gap §9.6 for the Leg-1 freeze).
    let ordinal = project.is_none().then_some(ARCHIPELAGO_ORDINAL);

    path.push(GroupSegment::text(SEGMENT_VESSEL, session.name.clone()));
    let group_path = GroupPath(path);
    let (state, attention) = session_badge(session.phase);
    let mut facts = vec![
        (KEY_STATUS_STATE, MetadataValue::text(state.as_str())),
        (KEY_VESSEL_HOST, MetadataValue::text(session.host.to_string())),
        (KEY_FACTORY_ID, MetadataValue::text(format!("flotilla:sessions/{namespace}/{}", session.name))),
    ];
    if attention {
        facts.push((KEY_STATUS_ATTENTION, MetadataValue::Bool(true)));
    }
    if let Some(recipe) = session.attach.as_deref().and_then(|attach_ref| mint.attach(attach_ref)) {
        facts.push((KEY_MATERIALIZE_TARGET, MetadataValue::text("pane")));
        facts.push((KEY_MATERIALIZE_RECIPE, MetadataValue::text(recipe.command())));
    }
    catalog.assert_facts(MetadataTarget::Group(group_path.clone()), facts, ordinal);

    // The identity half of the pane → identity → group join: `flotilla
    // attach` stamps this identity on the pane; the scope fact published
    // here resolves the pane into its group.
    let identity_value = format!("{}/{namespace}/{}", session.host, session.name);
    catalog.assert_facts(
        MetadataTarget::Identity(MetadataIdentity { key: KEY_SESSION.to_owned(), value: MetadataValue::text(identity_value) }),
        vec![(KEY_SCOPE, group_path.to_scope_value()), (KEY_STATUS_STATE, MetadataValue::text(state.as_str()))],
        None,
    );
}

#[cfg(test)]
mod tests;
