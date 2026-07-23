//! The catalog projection: query result-set rows in, metadata patches out.
//!
//! Pure functions — no I/O, no clock. The connector loop feeds it the
//! fleet-merged `convoys` and `independents` rows and sends what comes back.
//!
//! Truthfulness rules (design §7 on flotilla-org/flotilla#667):
//! - every path is published honestly; the PM's resolver decides
//!   latent-vs-live by whether a materialised member carries the identity —
//!   the connector never says "latent";
//! - entities without a daemon-resolvable attach list without a recipe
//!   (truthfully unmaterialisable) rather than with one that would fail;
//! - failed things carry their message — never a masked "initializing…".

use std::collections::BTreeMap;

use flotilla_protocol::{
    result_set::{
        AwarenessEntry, AwarenessKind, AwarenessNode, AwarenessPhase, AwarenessState, ConvoyPhase, ConvoyRow, IndependentRow, SessionPhase,
        VesselRow, WorkPhase,
    },
    ViewAddress,
};

use crate::{
    keys::{
        ARCHIPELAGO_ORDINAL, CATALOG_TTL_MS, KEY_CONVOY_MESSAGE, KEY_CONVOY_PHASE, KEY_CONVOY_WORKFLOW, KEY_CREW_ROLES, KEY_FACTORY_ID,
        KEY_MATERIALIZE_RECIPE, KEY_MATERIALIZE_TARGET, KEY_PROJECT_NAME, KEY_SCOPE, KEY_SESSION, KEY_STATUS_ATTENTION, KEY_STATUS_STATE,
        KEY_SUMMARY_TEXT, KEY_VESSEL_HOST, KEY_WORK_PHASE, SEGMENT_CONVOY, SEGMENT_ISSUE, SEGMENT_PROJECT, SEGMENT_VESSEL,
        SOURCE_CONNECTOR,
    },
    recipe::RecipeMint,
    wire::{GroupPath, GroupSegment, MetadataIdentity, MetadataPatch, MetadataTarget, MetadataValue, MetadataValueUpdate},
};

/// The rows the catalog is projected from.
pub struct CatalogInput<'a> {
    pub awareness: Option<&'a [AwarenessNode]>,
    pub convoys: &'a [ConvoyRow],
    pub independents: &'a [IndependentRow],
}

/// A normalized badge: the frozen `status.state` vocabulary plus whether
/// the entity demands attention (design §6, proposed for the Leg-1 freeze).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Badge {
    pub state: BadgeState,
    pub attention: bool,
}

/// Frozen `status.state` vocabulary.
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

/// Badge for a convoy. A convoy still awaiting its workflow snapshot reads
/// as waiting whatever its phase — truthfully not yet running, never a
/// masked "initializing…".
pub fn convoy_badge(phase: ConvoyPhase, initializing: bool) -> Badge {
    if initializing {
        return Badge { state: BadgeState::Waiting, attention: false };
    }
    match phase {
        ConvoyPhase::Pending => Badge { state: BadgeState::Waiting, attention: false },
        ConvoyPhase::Active => Badge { state: BadgeState::Active, attention: false },
        ConvoyPhase::Completed => Badge { state: BadgeState::Done, attention: false },
        ConvoyPhase::Failed => Badge { state: BadgeState::Failed, attention: true },
        ConvoyPhase::Cancelled => Badge { state: BadgeState::Idle, attention: false },
        ConvoyPhase::Abandoned => Badge { state: BadgeState::Idle, attention: false },
    }
}

/// Badge for the work aboard a vessel. `Ready` demands attention: the crew
/// could start and hasn't.
pub fn work_badge(phase: WorkPhase) -> Badge {
    match phase {
        WorkPhase::Pending => Badge { state: BadgeState::Idle, attention: false },
        WorkPhase::Ready => Badge { state: BadgeState::Waiting, attention: true },
        WorkPhase::Launching | WorkPhase::Running => Badge { state: BadgeState::Active, attention: false },
        WorkPhase::Complete => Badge { state: BadgeState::Done, attention: false },
        WorkPhase::Failed => Badge { state: BadgeState::Failed, attention: true },
        WorkPhase::Cancelled => Badge { state: BadgeState::Idle, attention: false },
        WorkPhase::Abandoned => Badge { state: BadgeState::Idle, attention: false },
    }
}

pub fn session_badge(phase: SessionPhase) -> Badge {
    match phase {
        SessionPhase::Starting => Badge { state: BadgeState::Waiting, attention: false },
        SessionPhase::Running => Badge { state: BadgeState::Active, attention: false },
        SessionPhase::Stopped => Badge { state: BadgeState::Idle, attention: false },
        SessionPhase::Failed => Badge { state: BadgeState::Failed, attention: true },
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
    if let Some(nodes) = input.awareness {
        for node in nodes {
            project_awareness_node(&mut catalog, node, mint);
        }
        return catalog;
    }
    for convoy in input.convoys {
        project_convoy(&mut catalog, convoy, mint);
    }
    for independent in input.independents {
        project_independent(&mut catalog, independent, mint);
    }
    catalog
}

fn project_awareness_node(catalog: &mut Catalog, node: &AwarenessNode, mint: &dyn RecipeMint) {
    let project = GroupSegment::text(SEGMENT_PROJECT, node.id.clone()).with_label(node.label.clone());
    assert_project_group(catalog, &project);
    let project_path = GroupPath(vec![project.clone()]);
    catalog.assert_facts(
        MetadataTarget::Group(project_path),
        vec![
            (KEY_STATUS_STATE, MetadataValue::text(awareness_state(node.state))),
            (KEY_SUMMARY_TEXT, MetadataValue::text(summary_text(&node.counts))),
            (KEY_FACTORY_ID, MetadataValue::text(format!("flotilla:awareness/{}", node.id))),
        ],
        None,
    );

    for entry in &node.entries {
        project_awareness_entry(catalog, &project, entry, mint);
    }
}

fn project_awareness_entry(catalog: &mut Catalog, project: &GroupSegment, entry: &AwarenessEntry, mint: &dyn RecipeMint) {
    let segment_key = match entry.kind {
        AwarenessKind::Convoy => SEGMENT_CONVOY,
        AwarenessKind::Issue => SEGMENT_ISSUE,
        AwarenessKind::Vessel | AwarenessKind::Independent | AwarenessKind::Checkout => SEGMENT_VESSEL,
        AwarenessKind::Fleet | AwarenessKind::Project => return,
    };
    let path = GroupPath(vec![project.clone(), GroupSegment::text(segment_key, entry.id.clone()).with_label(entry.label.clone())]);
    let mut facts = vec![
        (KEY_STATUS_STATE, MetadataValue::text(awareness_state(entry.state))),
        (KEY_SUMMARY_TEXT, MetadataValue::text(entry.label.clone())),
        (KEY_FACTORY_ID, MetadataValue::text(format!("flotilla:awareness/{}", entry.id))),
    ];
    if let Some(AwarenessPhase::Work(phase)) = entry.phase {
        facts.push((KEY_WORK_PHASE, MetadataValue::text(phase.as_str())));
    }
    if let Some(host) = entry.annotations.get(KEY_VESSEL_HOST) {
        facts.push((KEY_VESSEL_HOST, MetadataValue::text(host.clone())));
    }
    if matches!(entry.state, AwarenessState::Waiting | AwarenessState::Failed) {
        facts.push((KEY_STATUS_ATTENTION, MetadataValue::Bool(true)));
    }
    if let Some(recipe) = awareness_view_target(&entry.id).and_then(|target| mint.scoped_view(&target)) {
        facts.push((KEY_MATERIALIZE_TARGET, MetadataValue::text("workspace")));
        facts.push((KEY_MATERIALIZE_RECIPE, MetadataValue::text(recipe.command())));
    }
    catalog.assert_facts(MetadataTarget::Group(path), facts, None);
}

fn awareness_view_target(id: &str) -> Option<ViewAddress> {
    match id.parse().ok()? {
        address @ (ViewAddress::Project { .. } | ViewAddress::Convoy { .. } | ViewAddress::Vessel { .. }) => Some(address),
        _ => None,
    }
}

fn awareness_state(state: AwarenessState) -> &'static str {
    match state {
        AwarenessState::Unknown | AwarenessState::Pending | AwarenessState::Cancelled => "idle",
        AwarenessState::Waiting => "waiting",
        AwarenessState::Active => "active",
        AwarenessState::Done => "done",
        AwarenessState::Failed => "failed",
    }
}

fn summary_text(counts: &flotilla_protocol::AwarenessCounts) -> String {
    format!("{} entries · {} issues · {} vessels · {} checkouts", counts.total, counts.issues, counts.vessels, counts.checkouts)
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
    let badge = convoy_badge(convoy.phase, convoy.initializing);
    let done = convoy.vessels.iter().filter(|vessel| vessel.phase == WorkPhase::Complete).count();
    let mut facts = vec![
        (KEY_CONVOY_PHASE, MetadataValue::text(convoy.phase.as_str())),
        (KEY_CONVOY_WORKFLOW, MetadataValue::text(convoy.workflow_ref.clone())),
        (KEY_STATUS_STATE, MetadataValue::text(badge.state.as_str())),
        (KEY_FACTORY_ID, MetadataValue::text(format!("flotilla:convoys/{namespace}/{}", convoy.name))),
    ];
    if let Some(message) = &convoy.message {
        facts.push((KEY_CONVOY_MESSAGE, MetadataValue::text(message.clone())));
    }
    if badge.attention {
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
    let badge = work_badge(vessel.phase);
    let mut facts = vec![
        (KEY_WORK_PHASE, MetadataValue::text(vessel.phase.as_str())),
        (KEY_VESSEL_HOST, MetadataValue::text(vessel.host.to_string())),
        (KEY_STATUS_STATE, MetadataValue::text(badge.state.as_str())),
        (KEY_FACTORY_ID, MetadataValue::text(vessel_factory_id(namespace, &convoy.name, &vessel.name))),
    ];
    if !vessel.crew.is_empty() {
        let roles = vessel.crew.iter().map(|member| member.role.clone()).collect();
        facts.push((KEY_CREW_ROLES, MetadataValue::StringList(roles)));
    }
    if badge.attention {
        facts.push((KEY_STATUS_ATTENTION, MetadataValue::Bool(true)));
    }
    if let Some(message) = &vessel.message {
        facts.push((KEY_SUMMARY_TEXT, MetadataValue::text(message.clone())));
    }
    // Vessels materialise at workspace granularity (ADR 0012); the recipe
    // exists iff the daemon can resolve the attach — a capability fact.
    if let Some(recipe) = vessel.materialize.as_deref().and_then(|attach_ref| mint.attach(attach_ref, &vessel.host)) {
        facts.push((KEY_MATERIALIZE_TARGET, MetadataValue::text("workspace")));
        facts.push((KEY_MATERIALIZE_RECIPE, MetadataValue::text(recipe.command())));
    }
    catalog.assert_facts(MetadataTarget::Group(path), facts, ordinal);
}

fn project_independent(catalog: &mut Catalog, independent: &IndependentRow, mint: &dyn RecipeMint) {
    let namespace = &independent.resource.namespace;
    let project = project_segment(None, independent.repo.as_ref().map(|repo| repo.0.as_str()));
    let mut path = Vec::new();
    if let Some(segment) = &project {
        assert_project_group(catalog, segment);
        path.push(segment.clone());
    }
    // Free-floating vessels with no project prefix are archipelago-level:
    // grouped and ordered before everything else by default (design §4;
    // ordering semantics are gap §9.6 for the Leg-1 freeze).
    let ordinal = project.is_none().then_some(ARCHIPELAGO_ORDINAL);

    path.push(GroupSegment::text(SEGMENT_VESSEL, independent.name.clone()));
    let group_path = GroupPath(path);
    let badge = session_badge(independent.phase);
    let mut facts = vec![
        (KEY_STATUS_STATE, MetadataValue::text(badge.state.as_str())),
        (KEY_VESSEL_HOST, MetadataValue::text(independent.host.to_string())),
        (KEY_FACTORY_ID, MetadataValue::text(format!("flotilla:independents/{namespace}/{}", independent.name))),
    ];
    if badge.attention {
        facts.push((KEY_STATUS_ATTENTION, MetadataValue::Bool(true)));
    }
    if let Some(recipe) = independent.attach.as_deref().and_then(|attach_ref| mint.attach(attach_ref, &independent.host)) {
        facts.push((KEY_MATERIALIZE_TARGET, MetadataValue::text("pane")));
        facts.push((KEY_MATERIALIZE_RECIPE, MetadataValue::text(recipe.command())));
    }
    catalog.assert_facts(MetadataTarget::Group(group_path.clone()), facts, ordinal);

    // The identity half of the pane → identity → group join: `flotilla
    // attach` stamps this identity on the pane; the scope fact published
    // here resolves the pane into its group.
    let identity_value = format!("{}/{namespace}/{}", independent.host, independent.name);
    catalog.assert_facts(
        MetadataTarget::Identity(MetadataIdentity { key: KEY_SESSION.to_owned(), value: MetadataValue::text(identity_value) }),
        vec![(KEY_SCOPE, group_path.to_scope_value()), (KEY_STATUS_STATE, MetadataValue::text(badge.state.as_str()))],
        None,
    );
}

#[cfg(test)]
mod tests;
