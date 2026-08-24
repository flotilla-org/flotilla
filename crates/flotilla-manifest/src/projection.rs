//! Fleet rows projected as flat presentation entities.
//!
//! This is deliberately not a hierarchy builder. Every catalog patch targets
//! one canonical entity and carries only flat facts. Presentation managers
//! derive paths from those facts using their selected grouping template.

use std::collections::BTreeMap;

use flotilla_protocol::{
    result_set::{
        AwarenessCounts, AwarenessEntry, AwarenessKind, AwarenessNode, AwarenessPhase, AwarenessState, ConvoyPhase, ConvoyRow,
        IndependentRow, SessionPhase, VesselRow, WorkPhase,
    },
    ViewAddress, AWARENESS_REL_FOR_CONVOY,
};

use crate::{
    entity::{self, EntityRef},
    keys::{
        ARCHIPELAGO_ORDINAL, CATALOG_TTL_MS, KEY_CHANGE_REQUEST_NUMBER, KEY_CHECKOUT_BRANCH, KEY_CHECKOUT_PATH, KEY_CONVOY,
        KEY_CONVOY_MESSAGE, KEY_CONVOY_NAME, KEY_CONVOY_PHASE, KEY_CONVOY_WORKFLOW, KEY_COUNT_CHECKOUTS, KEY_COUNT_CONVOYS,
        KEY_COUNT_INDEPENDENTS, KEY_COUNT_ISSUES, KEY_COUNT_TOTAL, KEY_COUNT_VESSELS, KEY_CREW_ROLES, KEY_DISPLAY_LABEL,
        KEY_DISPLAY_LABEL_MEDIUM, KEY_DISPLAY_LABEL_SHORT, KEY_ENTITY_ID, KEY_ENTITY_KIND, KEY_INDEPENDENT_HOST, KEY_PRIMARY_ACTION_KEY,
        KEY_PRIMARY_ACTION_LABEL, KEY_PRIMARY_ACTION_RECIPE, KEY_PRIMARY_ACTION_TARGET, KEY_PRIMARY_ACTION_VEHICLE, KEY_PROJECT_NAME,
        KEY_REPO_NAME, KEY_SESSION, KEY_SOURCE, KEY_STATUS_ATTENTION, KEY_STATUS_STATE, KEY_SUMMARY_TEXT, KEY_VESSEL, KEY_VESSEL_HOST,
        KEY_VESSEL_NAME, KEY_WORK_PHASE, SEGMENT_CHECKOUT, SEGMENT_ISSUE, SEGMENT_PROJECT, SEGMENT_REPO, SOURCE_CONNECTOR, SOURCE_FLOTILLA,
    },
    recipe::{Recipe, RecipeMint},
    wire::{MetadataPatch, MetadataTarget, MetadataValue, MetadataValueUpdate},
};

/// The rows the catalog is projected from.
pub struct CatalogInput<'a> {
    pub awareness: Option<&'a [AwarenessNode]>,
    pub convoys: &'a [ConvoyRow],
    pub independents: &'a [IndependentRow],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Badge {
    pub state: BadgeState,
    pub attention: bool,
}

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

pub fn convoy_badge(phase: ConvoyPhase, initializing: bool) -> Badge {
    if initializing {
        return Badge { state: BadgeState::Waiting, attention: false };
    }
    match phase {
        ConvoyPhase::Pending => Badge { state: BadgeState::Waiting, attention: false },
        ConvoyPhase::Active => Badge { state: BadgeState::Active, attention: false },
        ConvoyPhase::Interrupted => Badge { state: BadgeState::Waiting, attention: true },
        ConvoyPhase::Landed => Badge { state: BadgeState::Done, attention: false },
        ConvoyPhase::Anchored | ConvoyPhase::Landing => Badge { state: BadgeState::Active, attention: false },
        ConvoyPhase::Failed => Badge { state: BadgeState::Failed, attention: true },
        ConvoyPhase::Cancelled | ConvoyPhase::Abandoned => Badge { state: BadgeState::Idle, attention: false },
    }
}

pub fn work_badge(phase: WorkPhase) -> Badge {
    match phase {
        WorkPhase::Pending => Badge { state: BadgeState::Idle, attention: false },
        WorkPhase::Ready => Badge { state: BadgeState::Waiting, attention: true },
        WorkPhase::Launching | WorkPhase::Running => Badge { state: BadgeState::Active, attention: false },
        WorkPhase::Interrupted => Badge { state: BadgeState::Waiting, attention: true },
        WorkPhase::Complete => Badge { state: BadgeState::Done, attention: false },
        WorkPhase::Failed => Badge { state: BadgeState::Failed, attention: true },
        WorkPhase::Cancelled | WorkPhase::Abandoned => Badge { state: BadgeState::Idle, attention: false },
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    facts: BTreeMap<MetadataTarget, BTreeMap<String, MetadataValueUpdate>>,
}

impl Catalog {
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }

    pub fn reassert_patches(&self) -> Vec<MetadataPatch> {
        self.facts.iter().map(|(target, facts)| patch(target.clone(), facts.clone(), vec![])).collect()
    }

    pub fn diff_patches(&self, previous: &Catalog) -> Vec<MetadataPatch> {
        let mut patches = Vec::new();
        for (target, facts) in &self.facts {
            let prior = previous.facts.get(target);
            let set = facts
                .iter()
                .filter(|(key, update)| prior.and_then(|prior| prior.get(*key)) != Some(*update))
                .map(|(key, update)| (key.clone(), update.clone()))
                .collect::<BTreeMap<_, _>>();
            let unset: Vec<String> =
                prior.map(|prior| prior.keys().filter(|key| !facts.contains_key(*key)).cloned().collect()).unwrap_or_default();
            if !set.is_empty() || !unset.is_empty() {
                patches.push(patch(target.clone(), set, unset));
            }
        }
        for (target, facts) in &previous.facts {
            if !self.facts.contains_key(target) {
                patches.push(patch(
                    target.clone(),
                    BTreeMap::new(),
                    facts.keys().filter(|key| key.as_str() != KEY_SOURCE).cloned().collect(),
                ));
            }
        }
        patches
    }

    fn assert_entity(&mut self, entity: EntityRef, facts: Vec<(&str, MetadataValue)>, ordinal: Option<i64>) {
        let target = MetadataTarget::Entity(entity.clone());
        let entry = self.facts.entry(target).or_default();
        let base = [
            (KEY_ENTITY_KIND, MetadataValue::text(entity.kind.clone())),
            (KEY_ENTITY_ID, MetadataValue::text(entity.id)),
            (KEY_SOURCE, MetadataValue::text(SOURCE_FLOTILLA)),
        ];
        for (key, value) in base.into_iter().chain(facts) {
            let mut update = MetadataValueUpdate::new(value, Some(CATALOG_TTL_MS));
            update.ordinal = ordinal;
            entry.insert(key.to_owned(), update);
        }
    }
}

fn patch(target: MetadataTarget, mut set: BTreeMap<String, MetadataValueUpdate>, unset: Vec<String>) -> MetadataPatch {
    set.entry(KEY_SOURCE.to_owned())
        .or_insert_with(|| MetadataValueUpdate::new(MetadataValue::text(SOURCE_FLOTILLA), Some(CATALOG_TTL_MS)));
    MetadataPatch { target, source_id: SOURCE_CONNECTOR.to_owned(), set, unset }
}

pub fn project_catalog(input: &CatalogInput<'_>, mint: &dyn RecipeMint) -> Catalog {
    let mut catalog = Catalog::default();
    if let Some(nodes) = input.awareness {
        for node in nodes {
            project_awareness_node(&mut catalog, node, input.convoys, mint);
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

fn project_awareness_node(catalog: &mut Catalog, node: &AwarenessNode, convoys: &[ConvoyRow], mint: &dyn RecipeMint) {
    let parent = awareness_parent_facts(node, convoys);
    if let Some((entity, mut facts)) = awareness_node_entity(node, convoys) {
        facts.extend(status_and_counts(node.state, &node.counts, node.entries.len()));
        if let Some(recipe) = awareness_project_recipe(node, mint) {
            facts.extend(action_facts(&entity, &recipe, "workspace"));
        }
        catalog.assert_entity(entity, facts, None);
    }
    for entry in &node.entries {
        project_awareness_entry(catalog, &parent, entry, convoys, mint);
    }
}

fn awareness_parent_facts(node: &AwarenessNode, convoys: &[ConvoyRow]) -> Vec<(&'static str, MetadataValue)> {
    match node.kind {
        AwarenessKind::Project => {
            let Some((project, _)) = awareness_node_entity(node, convoys) else {
                return vec![];
            };
            vec![
                (SEGMENT_PROJECT, MetadataValue::text(project.id)),
                (KEY_PROJECT_NAME, MetadataValue::text(node.label.clone())),
                (KEY_DISPLAY_LABEL, MetadataValue::text(node.label.clone())),
            ]
        }
        AwarenessKind::Convoy => {
            let value = node.id.strip_prefix("convoy/").unwrap_or(&node.id);
            let Some((namespace, name)) = value.split_once('/') else {
                return vec![];
            };
            let origin = find_convoy(convoys, namespace, name)
                .map(|row| entity::resource_origin(&row.resource))
                .unwrap_or_else(|| "fleet".to_owned());
            let convoy = entity::convoy(namespace, name, &origin);
            vec![(KEY_CONVOY, MetadataValue::text(convoy.id)), (KEY_CONVOY_NAME, MetadataValue::text(node.label.clone()))]
        }
        AwarenessKind::Fleet | AwarenessKind::Vessel | AwarenessKind::Issue | AwarenessKind::Independent | AwarenessKind::Checkout => {
            vec![]
        }
    }
}

fn awareness_node_entity(node: &AwarenessNode, convoys: &[ConvoyRow]) -> Option<(EntityRef, Vec<(&'static str, MetadataValue)>)> {
    match node.kind {
        AwarenessKind::Project => {
            let (namespace, name) = node.scope.as_ref().map(|scope| (scope.namespace.as_str(), scope.name.as_str())).or_else(|| {
                let mut parts = node.id.strip_prefix("project/")?.split('/');
                Some((parts.next()?, parts.next()?))
            })?;
            let entity = entity::project(namespace, name, "fleet");
            let mut facts = vec![
                (SEGMENT_PROJECT, MetadataValue::text(entity.id.clone())),
                (KEY_PROJECT_NAME, MetadataValue::text(node.label.clone())),
                (KEY_DISPLAY_LABEL, MetadataValue::text(node.label.clone())),
            ];
            facts.extend(label_tier_facts(&node.label));
            Some((entity, facts))
        }
        AwarenessKind::Convoy => {
            let value = node.id.strip_prefix("convoy/").unwrap_or(&node.id);
            let (namespace, name) = value.split_once('/')?;
            let row = find_convoy(convoys, namespace, name);
            let origin = row.map(|row| entity::resource_origin(&row.resource)).unwrap_or_else(|| "fleet".to_owned());
            let entity = entity::convoy(namespace, name, &origin);
            let semantic_label = row.map(|row| row.name.as_str()).unwrap_or(&node.label);
            let mut facts = vec![
                (KEY_CONVOY, MetadataValue::text(entity.id.clone())),
                (KEY_CONVOY_NAME, MetadataValue::text(semantic_label)),
                (KEY_DISPLAY_LABEL, MetadataValue::text(node.label.clone())),
            ];
            facts.extend(label_tier_facts(semantic_label));
            Some((entity, facts))
        }
        _ => None,
    }
}

fn project_awareness_entry(
    catalog: &mut Catalog,
    parent: &[(&'static str, MetadataValue)],
    entry: &AwarenessEntry,
    convoys: &[ConvoyRow],
    mint: &dyn RecipeMint,
) {
    let repo = entry.annotations.get(SEGMENT_REPO).cloned();
    if let Some(repo) = &repo {
        assert_repo_entity(catalog, repo, parent);
    }
    let Some((entity, mut own_facts)) = awareness_entry_entity(entry, convoys) else {
        return;
    };
    let mut facts = parent.to_vec();
    if let Some(repo) = repo {
        facts.push((SEGMENT_REPO, MetadataValue::text(repo.clone())));
        facts.push((KEY_REPO_NAME, MetadataValue::text(repo_label(&repo))));
    }
    facts.append(&mut own_facts);
    facts.push((KEY_STATUS_STATE, MetadataValue::text(awareness_state(entry.state))));
    facts.push((KEY_SUMMARY_TEXT, MetadataValue::text(entry.label.clone())));
    if let Some(AwarenessPhase::Work(phase)) = entry.phase {
        facts.push((KEY_WORK_PHASE, MetadataValue::text(phase.as_str())));
    }
    if let Some(host) = entry.annotations.get(KEY_VESSEL_HOST) {
        facts.push((KEY_VESSEL_HOST, MetadataValue::text(host.clone())));
    }
    if matches!(entry.state, AwarenessState::Waiting | AwarenessState::Failed) {
        facts.push((KEY_STATUS_ATTENTION, MetadataValue::Bool(true)));
    }
    if let Some((recipe, target)) = awareness_entry_recipe(entry, convoys, mint) {
        facts.extend(action_facts(&target, &recipe, "workspace"));
    }
    catalog.assert_entity(entity, facts, None);
}

fn awareness_entry_entity(entry: &AwarenessEntry, convoys: &[ConvoyRow]) -> Option<(EntityRef, Vec<(&'static str, MetadataValue)>)> {
    let label = entry.label.clone();
    let (entity, facts) = match entry.kind {
        AwarenessKind::Convoy => {
            let value = entry.id.strip_prefix("convoy/").unwrap_or(&entry.id);
            let (namespace, name) = value.split_once('/')?;
            let row = find_convoy(convoys, namespace, name);
            let origin = row.map(|row| entity::resource_origin(&row.resource)).unwrap_or_else(|| "fleet".to_owned());
            let entity = entity::convoy(namespace, name, &origin);
            let semantic_label = entry.annotations.get(KEY_CONVOY_NAME).map(String::as_str).unwrap_or(name);
            let mut facts =
                vec![(KEY_CONVOY, MetadataValue::text(entity.id.clone())), (KEY_CONVOY_NAME, MetadataValue::text(semantic_label))];
            facts.extend(label_tier_facts(semantic_label));
            if let Some(number) = entry.annotations.get(KEY_CHANGE_REQUEST_NUMBER) {
                facts.push((KEY_CHANGE_REQUEST_NUMBER, MetadataValue::text(number.clone())));
            }
            (entity, facts)
        }
        AwarenessKind::Vessel => {
            let value = entry.id.strip_prefix("vessel/").unwrap_or(&entry.id);
            let mut parts = value.split('/');
            let (namespace, convoy_name, vessel_name) = (parts.next()?, parts.next()?, parts.next()?);
            let row = find_convoy(convoys, namespace, convoy_name);
            let origin = row.map(|row| entity::resource_origin(&row.resource)).unwrap_or_else(|| "fleet".to_owned());
            let convoy = entity::convoy(namespace, convoy_name, &origin);
            let vessel_origin = find_vessel(convoys, namespace, convoy_name, vessel_name)
                .map(|vessel| vessel.host.to_string())
                .unwrap_or_else(|| origin.clone());
            let entity = entity::vessel(namespace, convoy_name, vessel_name, &vessel_origin);
            let mut facts = vec![
                (KEY_CONVOY, MetadataValue::text(convoy.id)),
                (KEY_CONVOY_NAME, MetadataValue::text(convoy_name)),
                (KEY_VESSEL, MetadataValue::text(entity.id.clone())),
                (KEY_VESSEL_NAME, MetadataValue::text(label.clone())),
            ];
            facts.extend(label_tier_facts(&label));
            (entity, facts)
        }
        AwarenessKind::Issue => {
            let entity = entry.issue_refs.first().map(entity::issue).unwrap_or_else(|| EntityRef::new("issue", entry.id.clone()));
            (entity.clone(), vec![(SEGMENT_ISSUE, MetadataValue::text(entity.id.clone()))])
        }
        AwarenessKind::Independent => {
            let value = entry.id.rsplit('/').next().unwrap_or(&entry.id);
            let session_ref = entry
                .refs
                .first()
                .map(|reference| {
                    format!(
                        "{}/{}/{}",
                        reference.host.as_ref().map(ToString::to_string).unwrap_or_else(|| "fleet".to_owned()),
                        reference.namespace,
                        reference.name
                    )
                })
                .unwrap_or_else(|| entry.id.clone());
            let entity = entity::session(&session_ref);
            let mut facts = vec![(KEY_SESSION, MetadataValue::text(entity.id.clone()))];
            facts.extend(label_tier_facts(value));
            (entity, facts)
        }
        AwarenessKind::Checkout => {
            let entity = entity::checkout(&entry.id);
            let mut facts = vec![(SEGMENT_CHECKOUT, MetadataValue::text(entity.id.clone()))];
            if let Some(branch) = entry.annotations.get(KEY_CHECKOUT_BRANCH) {
                facts.push((KEY_CHECKOUT_BRANCH, MetadataValue::text(branch.clone())));
            }
            if let Some(path) = entry.annotations.get(KEY_CHECKOUT_PATH) {
                facts.push((KEY_CHECKOUT_PATH, MetadataValue::text(path.clone())));
            }
            (entity, facts)
        }
        AwarenessKind::Fleet | AwarenessKind::Project => return None,
    };
    let mut facts = facts;
    facts.push((KEY_DISPLAY_LABEL, MetadataValue::text(label)));
    Some((entity, facts))
}

fn awareness_project_recipe(node: &AwarenessNode, mint: &dyn RecipeMint) -> Option<Recipe> {
    if !matches!(node.kind, AwarenessKind::Project) {
        return None;
    }
    let address = node.id.parse().ok()?;
    let ViewAddress::Project { .. } = &address else {
        return None;
    };
    mint.scoped_view(&address)
}

fn awareness_entry_recipe(entry: &AwarenessEntry, convoys: &[ConvoyRow], mint: &dyn RecipeMint) -> Option<(Recipe, EntityRef)> {
    if matches!(entry.kind, AwarenessKind::Issue) {
        return None;
    }
    if matches!(entry.kind, AwarenessKind::Checkout) {
        if entry.links.iter().any(|link| link.rel == AWARENESS_REL_FOR_CONVOY) {
            return None;
        }
        let path = entry.annotations.get(KEY_CHECKOUT_PATH)?;
        let host = entry.refs.iter().find_map(|reference| reference.host.as_ref())?;
        let target = entity::checkout(&entry.id);
        return mint.checkout_terminal(path, host).map(|recipe| (recipe, target));
    }
    match entry.id.parse().ok()? {
        ViewAddress::Project { namespace, name } => {
            let target = entity::project(&namespace, &name, "fleet");
            mint.scoped_view(&ViewAddress::Project { namespace, name }).map(|recipe| (recipe, target))
        }
        ViewAddress::Vessel { namespace, convoy, vessel } => find_vessel(convoys, &namespace, &convoy, &vessel).and_then(|vessel_row| {
            let target = entity::vessel(&namespace, &convoy, &vessel, vessel_row.host.as_str());
            vessel_row
                .materialize
                .as_deref()
                .and_then(|attach_ref| mint.attach(attach_ref, &vessel_row.host))
                .map(|recipe| (recipe, target))
        }),
        ViewAddress::Convoy { namespace, name } => {
            let convoy = find_convoy(convoys, &namespace, &name)?;
            let [vessel] = convoy.vessels.as_slice() else {
                return None;
            };
            let target = entity::vessel(&namespace, &name, &vessel.name, vessel.host.as_str());
            vessel.materialize.as_deref().and_then(|attach_ref| mint.attach(attach_ref, &vessel.host)).map(|recipe| (recipe, target))
        }
        _ => None,
    }
}

fn find_convoy<'a>(convoys: &'a [ConvoyRow], namespace: &str, name: &str) -> Option<&'a ConvoyRow> {
    convoys.iter().find(|convoy| convoy.resource.namespace == namespace && convoy.name == name)
}

fn find_vessel<'a>(convoys: &'a [ConvoyRow], namespace: &str, convoy_name: &str, vessel_name: &str) -> Option<&'a VesselRow> {
    find_convoy(convoys, namespace, convoy_name)?.vessels.iter().find(|vessel| vessel.name == vessel_name)
}

fn awareness_state(state: AwarenessState) -> &'static str {
    match state {
        AwarenessState::Unknown | AwarenessState::Idle | AwarenessState::Pending | AwarenessState::Cancelled => "idle",
        AwarenessState::Waiting => "waiting",
        AwarenessState::Active => "active",
        AwarenessState::Done => "done",
        AwarenessState::Failed => "failed",
    }
}

fn status_and_counts(state: AwarenessState, counts: &AwarenessCounts, visible: usize) -> Vec<(&'static str, MetadataValue)> {
    vec![
        (KEY_STATUS_STATE, MetadataValue::text(awareness_state(state))),
        (KEY_SUMMARY_TEXT, MetadataValue::text(summary_text(counts, visible))),
        (KEY_COUNT_TOTAL, MetadataValue::Integer(counts.total as i64)),
        (KEY_COUNT_ISSUES, MetadataValue::Integer(counts.issues as i64)),
        (KEY_COUNT_CONVOYS, MetadataValue::Integer(counts.convoys as i64)),
        (KEY_COUNT_VESSELS, MetadataValue::Integer(counts.vessels as i64)),
        (KEY_COUNT_CHECKOUTS, MetadataValue::Integer(counts.checkouts as i64)),
        (KEY_COUNT_INDEPENDENTS, MetadataValue::Integer(counts.independents as i64)),
    ]
}

fn summary_text(counts: &AwarenessCounts, visible: usize) -> String {
    let mut summary =
        format!("{} entries · {} issues · {} vessels · {} checkouts", counts.total, counts.issues, counts.vessels, counts.checkouts);
    let omitted = counts.total.saturating_sub(visible);
    if omitted > 0 {
        summary.push_str(&format!(" · +{omitted} more"));
    }
    summary
}

fn project_convoy(catalog: &mut Catalog, convoy: &ConvoyRow, mint: &dyn RecipeMint) {
    let namespace = &convoy.resource.namespace;
    let origin = entity::resource_origin(&convoy.resource);
    let project = project_fact(convoy.project_ref.as_deref(), &origin);
    let repo = convoy.repo.as_ref().map(|repo| repo.0.clone());
    if let Some((entity, facts)) = &project {
        catalog.assert_entity(entity.clone(), facts.clone(), None);
    }
    if let Some(repo) = &repo {
        assert_repo_entity(catalog, repo, &project_facts(&project));
    }
    let convoy_entity = entity::convoy(namespace, &convoy.name, &origin);
    let ordinal = (project.is_none() && repo.is_none()).then_some(ARCHIPELAGO_ORDINAL);
    let badge = convoy_badge(convoy.phase, convoy.initializing);
    let done = convoy.vessels.iter().filter(|vessel| vessel.phase == WorkPhase::Complete).count();
    let mut facts = project_facts(&project);
    if let Some(repo) = &repo {
        facts.push((SEGMENT_REPO, MetadataValue::text(repo.clone())));
        facts.push((KEY_REPO_NAME, MetadataValue::text(repo_label(repo))));
    }
    facts.extend([
        (KEY_CONVOY, MetadataValue::text(convoy_entity.id.clone())),
        (KEY_CONVOY_NAME, MetadataValue::text(convoy.name.clone())),
        (KEY_DISPLAY_LABEL, MetadataValue::text(convoy.name.clone())),
        (KEY_CONVOY_PHASE, MetadataValue::text(convoy.phase.as_str())),
        (KEY_CONVOY_WORKFLOW, MetadataValue::text(convoy.workflow_ref.clone())),
        (KEY_STATUS_STATE, MetadataValue::text(badge.state.as_str())),
    ]);
    facts.extend(label_tier_facts(&convoy.name));
    if let Some(change_request) = &convoy.change_request {
        facts.push((KEY_CHANGE_REQUEST_NUMBER, MetadataValue::text(change_request.id.clone())));
    }
    if let Some(message) = &convoy.message {
        facts.push((KEY_CONVOY_MESSAGE, MetadataValue::text(message.clone())));
    }
    if badge.attention {
        facts.push((KEY_STATUS_ATTENTION, MetadataValue::Bool(true)));
    }
    if !convoy.vessels.is_empty() {
        facts.push((KEY_SUMMARY_TEXT, MetadataValue::text(format!("{done}/{} vessels done", convoy.vessels.len()))));
    }
    if let [vessel] = convoy.vessels.as_slice() {
        if let Some(recipe) = vessel.materialize.as_deref().and_then(|attach_ref| mint.attach(attach_ref, &vessel.host)) {
            let target = entity::vessel(namespace, &convoy.name, &vessel.name, vessel.host.as_str());
            facts.extend(action_facts(&target, &recipe, "workspace"));
        }
    }
    catalog.assert_entity(convoy_entity, facts, ordinal);
    for vessel in &convoy.vessels {
        project_vessel(catalog, convoy, vessel, &project, repo.as_deref(), mint);
    }
}

fn project_vessel(
    catalog: &mut Catalog,
    convoy: &ConvoyRow,
    vessel: &VesselRow,
    project: &Option<(EntityRef, Vec<(&'static str, MetadataValue)>)>,
    repo: Option<&str>,
    mint: &dyn RecipeMint,
) {
    let entity = entity::vessel(&convoy.resource.namespace, &convoy.name, &vessel.name, vessel.host.as_str());
    let origin = entity::resource_origin(&convoy.resource);
    let convoy_entity = entity::convoy(&convoy.resource.namespace, &convoy.name, &origin);
    let ordinal = (project.is_none() && repo.is_none()).then_some(ARCHIPELAGO_ORDINAL);
    let badge = work_badge(vessel.phase);
    let mut facts = project_facts(project);
    if let Some(repo) = repo {
        facts.push((SEGMENT_REPO, MetadataValue::text(repo)));
        facts.push((KEY_REPO_NAME, MetadataValue::text(repo_label(repo))));
    }
    facts.extend([
        (KEY_CONVOY, MetadataValue::text(convoy_entity.id)),
        (KEY_CONVOY_NAME, MetadataValue::text(convoy.name.clone())),
        (KEY_VESSEL, MetadataValue::text(entity.id.clone())),
        (KEY_VESSEL_NAME, MetadataValue::text(vessel.name.clone())),
        (KEY_DISPLAY_LABEL, MetadataValue::text(vessel.name.clone())),
        (KEY_WORK_PHASE, MetadataValue::text(vessel.phase.as_str())),
        (KEY_VESSEL_HOST, MetadataValue::text(vessel.host.to_string())),
        (KEY_STATUS_STATE, MetadataValue::text(badge.state.as_str())),
    ]);
    facts.extend(label_tier_facts(&vessel.name));
    if !vessel.crew.is_empty() {
        facts.push((KEY_CREW_ROLES, MetadataValue::StringList(vessel.crew.iter().map(|member| member.role.clone()).collect())));
    }
    if badge.attention {
        facts.push((KEY_STATUS_ATTENTION, MetadataValue::Bool(true)));
    }
    if let Some(message) = &vessel.message {
        facts.push((KEY_SUMMARY_TEXT, MetadataValue::text(message.clone())));
    }
    if let Some(recipe) = vessel.materialize.as_deref().and_then(|attach_ref| mint.attach(attach_ref, &vessel.host)) {
        facts.extend(action_facts(&entity, &recipe, "workspace"));
    }
    catalog.assert_entity(entity, facts, ordinal);
}

fn project_independent(catalog: &mut Catalog, independent: &IndependentRow, mint: &dyn RecipeMint) {
    let namespace = &independent.resource.namespace;
    let session_ref = format!("{}/{namespace}/{}", independent.host, independent.name);
    let entity = entity::session(&session_ref);
    let repo = independent.repo_fact.as_ref().map(|repo| repo.0.as_str());
    if let Some(repo) = repo {
        assert_repo_entity(catalog, repo, &[]);
    }
    let ordinal = repo.is_none().then_some(ARCHIPELAGO_ORDINAL);
    let badge = session_badge(independent.phase);
    let mut facts = vec![
        (KEY_SESSION, MetadataValue::text(entity.id.clone())),
        (KEY_DISPLAY_LABEL, MetadataValue::text(independent.name.clone())),
        (KEY_STATUS_STATE, MetadataValue::text(badge.state.as_str())),
        (KEY_INDEPENDENT_HOST, MetadataValue::text(independent.host.to_string())),
    ];
    facts.extend(label_tier_facts(&independent.name));
    if let Some(repo) = repo {
        facts.push((SEGMENT_REPO, MetadataValue::text(repo)));
        facts.push((KEY_REPO_NAME, MetadataValue::text(repo_label(repo))));
    }
    if badge.attention {
        facts.push((KEY_STATUS_ATTENTION, MetadataValue::Bool(true)));
    }
    if let Some(recipe) = independent.attach.as_deref().and_then(|attach_ref| mint.attach(attach_ref, &independent.host)) {
        facts.extend(action_facts(&entity, &recipe, "pane"));
    }
    catalog.assert_entity(entity, facts, ordinal);
}

fn project_fact(project_ref: Option<&str>, origin: &str) -> Option<(EntityRef, Vec<(&'static str, MetadataValue)>)> {
    let project_ref = project_ref?;
    let mut parts = project_ref.split('/');
    let (namespace, name) = match (parts.next(), parts.next(), parts.next()) {
        (Some("project"), Some(namespace), Some(name)) => (namespace, name),
        _ => ("flotilla", project_ref.rsplit('/').next()?),
    };
    let entity = entity::project(namespace, name, origin);
    let mut facts = vec![
        (SEGMENT_PROJECT, MetadataValue::text(entity.id.clone())),
        (KEY_PROJECT_NAME, MetadataValue::text(name)),
        (KEY_DISPLAY_LABEL, MetadataValue::text(name)),
    ];
    facts.extend(label_tier_facts(name));
    Some((entity, facts))
}

fn project_facts(project: &Option<(EntityRef, Vec<(&'static str, MetadataValue)>)>) -> Vec<(&'static str, MetadataValue)> {
    project
        .as_ref()
        .map(|(_, facts)| {
            facts
                .iter()
                .filter(|(key, _)| !matches!(*key, KEY_DISPLAY_LABEL | KEY_DISPLAY_LABEL_MEDIUM | KEY_DISPLAY_LABEL_SHORT))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Derives stable width tiers from the semantic components of a role/name.
/// The medium tier retains the final (usually noun) component while reducing
/// its qualifiers to initials; the short tier is the component acronym.
fn label_tier_facts(label: &str) -> Vec<(&'static str, MetadataValue)> {
    let components: Vec<&str> = label.split(|character: char| !character.is_alphanumeric()).filter(|part| !part.is_empty()).collect();
    let Some(last) = components.last() else {
        return vec![];
    };
    let short: String = components.iter().filter_map(|part| part.chars().next()).collect();
    let medium = if components.len() == 1 {
        (*last).to_owned()
    } else {
        let qualifiers: String = components[..components.len() - 1].iter().filter_map(|part| part.chars().next()).collect();
        format!("{qualifiers}-{last}")
    };
    vec![(KEY_DISPLAY_LABEL_MEDIUM, MetadataValue::text(medium)), (KEY_DISPLAY_LABEL_SHORT, MetadataValue::text(short))]
}

fn assert_repo_entity(catalog: &mut Catalog, repo: &str, parent: &[(&'static str, MetadataValue)]) {
    let mut facts = parent.to_vec();
    facts.extend([
        (SEGMENT_REPO, MetadataValue::text(repo)),
        (KEY_REPO_NAME, MetadataValue::text(repo_label(repo))),
        (KEY_DISPLAY_LABEL, MetadataValue::text(repo_label(repo))),
    ]);
    catalog.assert_entity(entity::repo(repo), facts, None);
}

fn repo_label(value: &str) -> String {
    value
        .strip_suffix("/.git")
        .or_else(|| value.strip_suffix(".git"))
        .unwrap_or(value)
        .rsplit('/')
        .next()
        .filter(|short| !short.is_empty())
        .unwrap_or(value)
        .to_owned()
}

fn action_facts(target: &EntityRef, recipe: &Recipe, vehicle: &'static str) -> Vec<(&'static str, MetadataValue)> {
    vec![
        (KEY_PRIMARY_ACTION_KEY, MetadataValue::text("materialize")),
        (KEY_PRIMARY_ACTION_LABEL, MetadataValue::text("Open")),
        (KEY_PRIMARY_ACTION_VEHICLE, MetadataValue::text(vehicle)),
        (KEY_PRIMARY_ACTION_TARGET, MetadataValue::text(target.action_target())),
        (KEY_PRIMARY_ACTION_RECIPE, MetadataValue::text(recipe.command())),
    ]
}

#[cfg(test)]
mod tests;
