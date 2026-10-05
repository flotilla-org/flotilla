//! Fleet rows projected as flat presentation entities.
//!
//! This is deliberately not a hierarchy builder. Every catalog patch targets
//! one canonical entity and carries only flat facts. Presentation managers
//! derive paths from those facts using their selected grouping template.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_protocol::{
    result_set::{
        AwarenessCounts, AwarenessEntry, AwarenessKind, AwarenessNode, AwarenessPhase, AwarenessState, CleatEndpoint, ConvoyPhase,
        ConvoyRow, IndependentRow, ProjectRepositoriesRow, SessionPhase, StandingRoleHold, StandingRoleRow, SurfaceState, Timestamp,
        VesselRow, WorkPhase,
    },
    HostName, ReferenceContext, ViewAddress, AWARENESS_REL_FOR_CONVOY,
};
use flotilla_resources::{
    ChangeRequest, ChangeRequestStatus, Forge, Issue, Observation, ObservedChangeRequestState, ObservedChecks, ObservedMergeability,
    ObservedReviewDecision, ResourceObject,
};

use crate::{
    entity::{self, EntityRef},
    keys::{
        ARCHIPELAGO_ORDINAL, CATALOG_TTL_MS, KEY_CHECKOUT_BRANCH, KEY_CHECKOUT_PATH, KEY_CONVOY, KEY_CONVOY_MESSAGE, KEY_CONVOY_NAME,
        KEY_CONVOY_PHASE, KEY_CONVOY_STANDING, KEY_CONVOY_SUPERSEDED, KEY_CONVOY_WORKFLOW, KEY_COUNT_CHECKOUTS, KEY_COUNT_CONVOYS,
        KEY_COUNT_INDEPENDENTS, KEY_COUNT_ISSUES, KEY_COUNT_TOTAL, KEY_COUNT_VESSELS, KEY_CREW_ROLES, KEY_DISPLAY_LABEL,
        KEY_DISPLAY_LABEL_MEDIUM, KEY_DISPLAY_LABEL_SHORT, KEY_ENTITY_ID, KEY_ENTITY_KIND, KEY_INDEPENDENT_HOST, KEY_MEMBERSHIP_PROJECT,
        KEY_MEMBERSHIP_REPOSITORY_KEY, KEY_MEMBERSHIP_REPOSITORY_SLUG, KEY_MEMBERSHIP_SUBPATH, KEY_PRIMARY_ACTION_KEY,
        KEY_PRIMARY_ACTION_LABEL, KEY_PRIMARY_ACTION_RECIPE, KEY_PRIMARY_ACTION_TARGET, KEY_PRIMARY_ACTION_VEHICLE,
        KEY_PRIMARY_DIRECT_DAEMON, KEY_PRIMARY_DIRECT_HOST, KEY_PRIMARY_DIRECT_REASON, KEY_PRIMARY_DIRECT_RUNTIME_ROOT,
        KEY_PRIMARY_DIRECT_SESSION, KEY_PRIMARY_DIRECT_TRANSPORT, KEY_PROJECT_NAME, KEY_PROJECT_REPOSITORY_COUNT, KEY_REPO_NAME, KEY_ROLE,
        KEY_ROLE_HOLD, KEY_ROLE_NAME, KEY_ROLE_PRESENTS_AS, KEY_SESSION, KEY_SOURCE, KEY_STATUS_ATTENTION, KEY_STATUS_STATE,
        KEY_SUMMARY_TEXT, KEY_SURFACE_RUNG, KEY_SURFACE_STATE, KEY_VESSEL, KEY_VESSEL_HOST, KEY_VESSEL_NAME, KEY_WORKSPACE_PRIMARY_STATE,
        KEY_WORKSPACE_PRIMARY_TARGET, KEY_WORK_PHASE, SEGMENT_CHECKOUT, SEGMENT_ISSUE, SEGMENT_PROJECT, SEGMENT_REPO, SOURCE_CONNECTOR,
        SOURCE_FLOTILLA,
    },
    recipe::{DirectTransport, Recipe, RecipeMint},
    wire::{MetadataPatch, MetadataTarget, MetadataValue, MetadataValueUpdate},
};

/// The rows the catalog is projected from.
pub struct CatalogInput<'a> {
    pub subjects: Option<&'a SubjectCatalogInput>,
    pub awareness: Option<&'a [AwarenessNode]>,
    pub convoys: &'a [ConvoyRow],
    pub independents: &'a [IndependentRow],
    pub standing_roles: &'a [StandingRoleRow],
    pub project_repositories: &'a [ProjectRepositoriesRow],
}

mod subjects;
pub use subjects::change_request_facts;
use subjects::{project_role_attempts, project_subjects};

/// Replicated observation records and the fleet reference context. The clock is
/// supplied by the connector so expiry is deterministic in replay and tests.
/// With no clock, only live subjects are eligible; landed subjects are omitted.
#[derive(Default, bon::Builder)]
pub struct SubjectCatalogInput {
    pub change_requests: Vec<ResourceObject<ChangeRequest>>,
    pub issues: Vec<ResourceObject<Issue>>,
    pub forges: Vec<ResourceObject<Forge>>,
    pub references: ReferenceContext,
    pub now: Option<Timestamp>,
}

/// Resolve a subject service to a Forge: exact ID, installation URL, then
/// host alias. Overlapping declarations at one priority use Forge ID and
/// namespace as a deterministic tie-break, independent of input order.
pub fn subject_forge<'a>(forges: &'a [ResourceObject<Forge>], service: &str) -> Option<&'a ResourceObject<Forge>> {
    forges
        .iter()
        .filter_map(|forge| {
            let rank = if forge.spec.forge_id == service {
                0
            } else if forge.spec.owns_issue_service(service) {
                1
            } else if forge.spec.matches_host(service) {
                2
            } else {
                return None;
            };
            Some((rank, forge))
        })
        .min_by(|(a, left), (b, right)| {
            (a, &left.spec.forge_id, &left.metadata.namespace).cmp(&(b, &right.spec.forge_id, &right.metadata.namespace))
        })
        .map(|(_, forge)| forge)
}

/// Derived presentation state for a change request. The raw observations remain
/// available to presentation managers for more detailed displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeRequestReadiness {
    ReadyToMerge,
    AwaitingReviewResponse,
    CiFailing,
    Conflicting,
    Draft,
    MergedNotLanded,
    Closed,
}

impl ChangeRequestReadiness {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadyToMerge => "ready_to_merge",
            Self::AwaitingReviewResponse => "awaiting_review_response",
            Self::CiFailing => "ci_failing",
            Self::Conflicting => "conflicting",
            Self::Draft => "draft",
            Self::MergedNotLanded => "merged_not_landed",
            Self::Closed => "closed",
        }
    }
}

/// Derive the single readiness fact published on a change-request entity.
///
/// Precedence is closed or merged, draft, conflict, failed checks, then
/// waiting for review or incomplete evidence. `ready_to_merge` requires every
/// positive observation to be known. Pending checks and unknown observations
/// use the waiting state because the wire vocabulary has no pending/unknown
/// readiness variant; consumers can distinguish them through the raw fields.
/// A merged change request whose linked convoy has landed maps to `closed`:
/// there is no separate landed value. `landed` means a linked convoy has
/// reached its terminal landed phase.
pub fn change_request_readiness(status: &ChangeRequestStatus, landed: bool) -> ChangeRequestReadiness {
    evaluate_change_request_readiness(status, landed).readiness
}

/// A derived readiness value and the oldest observation it depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeRequestReadinessEvaluation {
    pub readiness: ChangeRequestReadiness,
    pub observed_at: Timestamp,
}

/// Evaluate readiness and its evidence time from the same typed input set.
pub fn evaluate_change_request_readiness(status: &ChangeRequestStatus, landed: bool) -> ChangeRequestReadinessEvaluation {
    let inputs = ReadinessInputs::builder()
        .state(&status.state)
        .checks(&status.checks)
        .mergeable(&status.mergeable)
        .review_decision(&status.review_decision)
        .actionable_at_head(&status.review.actionable_at_head)
        .build();
    ChangeRequestReadinessEvaluation { readiness: inputs.readiness(landed), observed_at: inputs.observed_at() }
}

// Readiness logic only sees this input set. Adding a policy input requires
// updating the exhaustive timestamp destructuring too, enforced by Rust.
#[derive(bon::Builder)]
struct ReadinessInputs<'a> {
    state: &'a Observation<ObservedChangeRequestState>,
    checks: &'a Observation<ObservedChecks>,
    mergeable: &'a Observation<ObservedMergeability>,
    review_decision: &'a Observation<ObservedReviewDecision>,
    actionable_at_head: &'a Observation<bool>,
}

impl ReadinessInputs<'_> {
    fn observed_at(&self) -> Timestamp {
        let Self { state, checks, mergeable, review_decision, actionable_at_head } = self;
        [state.observed_at, checks.observed_at, mergeable.observed_at, review_decision.observed_at, actionable_at_head.observed_at]
            .into_iter()
            .min()
            .expect("five readiness inputs")
    }

    fn readiness(&self, landed: bool) -> ChangeRequestReadiness {
        use ChangeRequestReadiness as Readiness;

        match self.state.value {
            Some(ObservedChangeRequestState::Closed) => return Readiness::Closed,
            Some(ObservedChangeRequestState::Merged) => return if landed { Readiness::Closed } else { Readiness::MergedNotLanded },
            Some(ObservedChangeRequestState::Draft) => return Readiness::Draft,
            _ => {}
        }

        if self.mergeable.value == Some(ObservedMergeability::Conflicting) {
            return Readiness::Conflicting;
        }
        if self.checks.value == Some(ObservedChecks::Fail) {
            return Readiness::CiFailing;
        }
        if self.state.value != Some(ObservedChangeRequestState::Open)
            || self.checks.value != Some(ObservedChecks::Pass)
            || self.mergeable.value != Some(ObservedMergeability::Mergeable)
            || self.actionable_at_head.value != Some(false)
            || !matches!(self.review_decision.value, Some(ObservedReviewDecision::Approved | ObservedReviewDecision::None))
        {
            return Readiness::AwaitingReviewResponse;
        }
        Readiness::ReadyToMerge
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Badge {
    pub state: BadgeState,
    pub attention: bool,
}

fn surface_facts(state: SurfaceState) -> Vec<(&'static str, MetadataValue)> {
    let mut facts = vec![(KEY_SURFACE_STATE, MetadataValue::text(state.as_str()))];
    if let Some(rung) = state.rung() {
        facts.push((KEY_SURFACE_RUNG, MetadataValue::text(rung.as_str())));
    }
    facts
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
        WorkPhase::Stalled => Badge { state: BadgeState::Waiting, attention: false },
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
    uncovered_services: BTreeSet<String>,
    facts: BTreeMap<MetadataTarget, BTreeMap<String, MetadataValueUpdate>>,
}

impl Catalog {
    /// Warn once per service until a rebuild no longer contains the gap.
    /// The returned set belongs to the caller's connector lifecycle.
    pub fn warn_new_uncovered_services(&self, previous: &BTreeSet<String>) -> BTreeSet<String> {
        for service in self.uncovered_services.difference(previous) {
            tracing::warn!(service, "subject service has no covering Forge");
        }
        self.uncovered_services.clone()
    }

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

/// Project once and emit diagnostics for callers without a connector lifecycle.
pub fn project_catalog(input: &CatalogInput<'_>, mint: &dyn RecipeMint) -> Catalog {
    let catalog = project_catalog_without_warnings(input, mint);
    catalog.warn_new_uncovered_services(&BTreeSet::new());
    catalog
}

/// Projection for connectors that own diagnostic suppression across rebuilds.
pub fn project_catalog_without_warnings(input: &CatalogInput<'_>, mint: &dyn RecipeMint) -> Catalog {
    let mut catalog = Catalog::default();
    if let Some(nodes) = input.awareness {
        for node in nodes {
            project_awareness_node(&mut catalog, node, input.convoys, mint);
        }
        project_readiness(&mut catalog, input.convoys);
        mark_superseded_convoys(&mut catalog, input.convoys);
        project_standing_roles(&mut catalog, input.standing_roles, input.convoys, mint);
        project_repository_memberships(&mut catalog, input.project_repositories);
        if let Some(subjects) = input.subjects {
            project_subjects(&mut catalog, input, subjects);
        }
        return catalog;
    }
    for convoy in input.convoys {
        project_convoy(&mut catalog, convoy, mint);
    }
    project_readiness(&mut catalog, input.convoys);
    for independent in input.independents {
        project_independent(&mut catalog, independent, mint);
    }
    mark_superseded_convoys(&mut catalog, input.convoys);
    project_standing_roles(&mut catalog, input.standing_roles, input.convoys, mint);
    project_repository_memberships(&mut catalog, input.project_repositories);
    if let Some(subjects) = input.subjects {
        project_subjects(&mut catalog, input, subjects);
    }
    catalog
}

/// Definitions publish even when no convoy or awareness entry exists.
fn project_repository_memberships(catalog: &mut Catalog, projects: &[ProjectRepositoriesRow]) {
    for project in projects {
        let project_entity = entity::project(&project.resource.namespace, &project.resource.name, "fleet");
        catalog.assert_entity(
            project_entity.clone(),
            vec![
                (SEGMENT_PROJECT, MetadataValue::text(project_entity.id.clone())),
                (KEY_PROJECT_NAME, MetadataValue::text(&project.display_name)),
                (KEY_DISPLAY_LABEL, MetadataValue::text(&project.display_name)),
                (KEY_PROJECT_REPOSITORY_COUNT, MetadataValue::Integer(project.repositories.len() as i64)),
            ],
            None,
        );
        for membership in &project.repositories {
            let relation = entity::project_repository(
                &project.resource.namespace,
                &project.resource.name,
                &membership.key.0,
                membership.subpath.as_deref(),
            );
            let mut facts = vec![
                (KEY_MEMBERSHIP_PROJECT, MetadataValue::text(&project_entity.id)),
                (KEY_MEMBERSHIP_REPOSITORY_KEY, MetadataValue::text(&membership.key.0)),
                (SEGMENT_PROJECT, MetadataValue::text(&project_entity.id)),
                (KEY_DISPLAY_LABEL, MetadataValue::text(membership.slug.as_deref().unwrap_or(&membership.key.0))),
            ];
            if let Some(slug) = &membership.slug {
                facts.push((KEY_MEMBERSHIP_REPOSITORY_SLUG, MetadataValue::text(slug)));
                facts.push((SEGMENT_REPO, MetadataValue::text(slug)));
            }
            if let Some(subpath) = &membership.subpath {
                facts.push((KEY_MEMBERSHIP_SUBPATH, MetadataValue::text(subpath)));
            }
            catalog.assert_entity(relation, facts, None);
        }
    }
}

/// Publish one stable entity per declared standing role and mark the attempts
/// its declaration admitted. Attempts are related by `ensured_from`, never by
/// convoy or role names: a task convoy sharing a role name is not standing.
fn project_standing_roles(catalog: &mut Catalog, roles: &[StandingRoleRow], convoys: &[ConvoyRow], mint: &dyn RecipeMint) {
    let mut role_by_convoy = BTreeMap::new();
    for convoy in convoys {
        if convoy.ensured_from.is_none() {
            continue;
        }
        let declared = roles
            .iter()
            .find(|role| role.resource.namespace == convoy.resource.namespace && Some(&role.resource.name) == convoy.ensured_from.as_ref());
        let convoy_entity = entity::convoy(&convoy.resource.namespace, &convoy.resource.name, &entity::resource_origin(&convoy.resource));
        role_by_convoy.insert(convoy_entity.id, declared.map(role_entity));
    }
    for facts in catalog.facts.values_mut() {
        let Some(MetadataValue::Text(convoy)) = facts.get(KEY_CONVOY).map(|fact| &fact.value) else {
            continue;
        };
        let Some(role) = role_by_convoy.get(convoy).cloned() else {
            continue;
        };
        facts.insert(KEY_CONVOY_STANDING.to_owned(), MetadataValueUpdate::new(MetadataValue::Bool(true), Some(CATALOG_TTL_MS)));
        if let Some(role) = role {
            facts.insert(KEY_ROLE.to_owned(), MetadataValueUpdate::new(MetadataValue::text(role.id), Some(CATALOG_TTL_MS)));
        }
    }
    for role in roles {
        project_standing_role(catalog, role, convoys, mint);
        project_role_attempts(catalog, role, convoys);
    }
}

fn role_project(role: &StandingRoleRow) -> &str {
    role.project_ref.rsplit('/').next().unwrap_or(&role.project_ref)
}

fn role_entity(role: &StandingRoleRow) -> EntityRef {
    entity::role(&role.resource.namespace, role_project(role), &role.role, "fleet")
}

fn standing_attempts<'a>(role: &StandingRoleRow, convoys: &'a [ConvoyRow]) -> Vec<&'a ConvoyRow> {
    let mut attempts = convoys
        .iter()
        .filter(|convoy| convoy.resource.namespace == role.resource.namespace && convoy.ensured_from.as_ref() == Some(&role.resource.name))
        .collect::<Vec<_>>();
    attempts.sort_by(|left, right| (left.generation, &left.resource.name).cmp(&(right.generation, &right.resource.name)));
    attempts
}

fn project_standing_role(catalog: &mut Catalog, role: &StandingRoleRow, convoys: &[ConvoyRow], mint: &dyn RecipeMint) {
    let entity = role_entity(role);
    let project_name = role_project(role);
    let project = entity::project(&role.resource.namespace, project_name, "fleet");
    let attempts = standing_attempts(role, convoys);
    let latest = attempts.last().copied();
    let live = attempts.iter().rev().copied().find(|convoy| !convoy.phase.is_terminal());

    // The primary target is the live attempt's only vessel. Multi-vessel
    // attempts have no single attach target yet and stay held.
    let attach =
        live.and_then(|convoy| match convoy.vessels.as_slice() {
            [vessel] => vessel.materialize.as_deref().and_then(|attach_ref| mint.attach(attach_ref, &vessel.host)).map(|recipe| {
                (entity::vessel(&convoy.resource.namespace, &convoy.resource.name, &vessel.name, vessel.host.as_str()), recipe)
            }),
            _ => None,
        });

    let badge = match (role.hold, live) {
        (Some(StandingRoleHold::RestartLimit), _) => Badge { state: BadgeState::Failed, attention: true },
        (Some(StandingRoleHold::BackingUnverified), _) => Badge { state: BadgeState::Waiting, attention: true },
        // Presentations may show the role in place of its attempt, so the
        // attempt's own attention (e.g. a vessel waiting for input) surfaces here.
        (None, Some(convoy)) => {
            let badge = readiness_badge(convoy_badge(convoy.phase, convoy.initializing), &convoy.readiness());
            let vessel_attention = convoy.vessels.iter().any(|vessel| vessel.surface_state.needs_attention());
            Badge { attention: convoy.surface_state.needs_attention() || vessel_attention, ..badge }
        }
        // Between generations the ensure loop is expected to admit the next
        // attempt; a superseded failure is not the role's current state.
        (None, _) => Badge { state: BadgeState::Waiting, attention: false },
    };
    let summary = match (role.hold, live) {
        (Some(_), _) | (None, None) => role.last_failure.clone().or_else(|| latest.and_then(|convoy| convoy.message.clone())),
        (None, Some(convoy)) => convoy.message.clone(),
    };

    let mut facts = vec![
        (SEGMENT_PROJECT, MetadataValue::text(project.id)),
        (KEY_PROJECT_NAME, MetadataValue::text(project_name)),
        (KEY_ROLE, MetadataValue::text(entity.id.clone())),
        (KEY_ROLE_NAME, MetadataValue::text(role.role.clone())),
        (KEY_DISPLAY_LABEL, MetadataValue::text(role.role.clone())),
        (KEY_STATUS_STATE, MetadataValue::text(badge.state.as_str())),
    ];
    facts.extend(label_tier_facts(&role.role));
    if let Some(presents_as) = &role.presents_as {
        facts.push((KEY_ROLE_PRESENTS_AS, MetadataValue::text(presents_as.clone())));
    }
    if let Some(hold) = role.hold {
        facts.push((KEY_ROLE_HOLD, MetadataValue::text(hold.as_str())));
    }
    if let Some(convoy) = live {
        facts.push((KEY_CONVOY_PHASE, MetadataValue::text(convoy.phase.as_str())));
        facts.extend(surface_facts(convoy.surface_state));
        facts.extend(readiness_facts(&convoy.readiness(), attach.is_some()));
    }
    if badge.attention {
        facts.push((KEY_STATUS_ATTENTION, MetadataValue::Bool(true)));
    }
    if let Some(summary) = summary {
        facts.push((KEY_SUMMARY_TEXT, MetadataValue::text(summary)));
    }
    match attach {
        Some((target, recipe)) if role.hold.is_none() => {
            facts.extend(action_facts(&entity, &recipe, "workspace"));
            if let Some(vessel) = live.and_then(|convoy| (convoy.vessels.len() == 1).then(|| &convoy.vessels[0])) {
                facts.extend(direct_facts(vessel.cleat_endpoint.as_ref(), &vessel.host, mint));
            }
            facts.push((KEY_WORKSPACE_PRIMARY_STATE, MetadataValue::text("ready")));
            facts.push((KEY_WORKSPACE_PRIMARY_TARGET, MetadataValue::text(target.action_target())));
        }
        // Held retains whatever content a bound workspace already shows.
        _ => facts.push((KEY_WORKSPACE_PRIMARY_STATE, MetadataValue::text("held"))),
    }
    catalog.assert_entity(entity, facts, None);
}

// Lifecycle identity comes from role addressing, never from display labels.
// Keep nonterminal attempts visible even if a newer generation also exists.
fn mark_superseded_convoys(catalog: &mut Catalog, convoys: &[ConvoyRow]) {
    let mut latest = BTreeMap::new();
    for row in convoys {
        if let (Some(project), Some(role)) = (&row.project_ref, &row.address_role) {
            let key = (&row.resource.namespace, project, role);
            latest.entry(key).and_modify(|generation: &mut u64| *generation = (*generation).max(row.generation)).or_insert(row.generation);
        }
    }
    let superseded = convoys
        .iter()
        .filter(|row| {
            row.phase.is_terminal()
                && match (&row.project_ref, &row.address_role) {
                    (Some(project), Some(role)) => {
                        latest.get(&(&row.resource.namespace, project, role)).is_some_and(|generation| *generation > row.generation)
                    }
                    _ => false,
                }
        })
        .map(|row| entity::convoy(&row.resource.namespace, &row.resource.name, &entity::resource_origin(&row.resource)).id)
        .collect::<BTreeSet<_>>();
    for facts in catalog.facts.values_mut() {
        let Some(MetadataValue::Text(convoy)) = facts.get(KEY_CONVOY).map(|fact| &fact.value) else {
            continue;
        };
        facts.insert(
            KEY_CONVOY_SUPERSEDED.to_owned(),
            MetadataValueUpdate::new(MetadataValue::Bool(superseded.contains(convoy)), Some(CATALOG_TTL_MS)),
        );
    }
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
            if let Some(row) = row {
                facts.push((KEY_CONVOY_PHASE, MetadataValue::text(row.phase.as_str())));
            }
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
        assert_repo_entity(catalog, repo);
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
    let surface_state = match entry.id.parse().ok() {
        Some(ViewAddress::Convoy { namespace, name }) => find_convoy(convoys, &namespace, &name).map(|row| row.surface_state),
        Some(ViewAddress::Vessel { namespace, convoy, vessel }) => {
            find_vessel(convoys, &namespace, &convoy, &vessel).map(|row| row.surface_state)
        }
        _ => None,
    };
    if let Some(state) = surface_state {
        facts.extend(surface_facts(state));
    }
    if surface_state.map_or_else(|| matches!(entry.state, AwarenessState::Waiting | AwarenessState::Failed), SurfaceState::needs_attention)
    {
        facts.push((KEY_STATUS_ATTENTION, MetadataValue::Bool(true)));
    }
    if let Some((recipe, target)) = awareness_entry_recipe(entry, convoys, mint) {
        facts.extend(action_facts(&target, &recipe, "workspace"));
        if let Some(vessel) = match entry.id.parse() {
            Ok(ViewAddress::Vessel { namespace, convoy, vessel }) => find_vessel(convoys, &namespace, &convoy, &vessel),
            Ok(ViewAddress::Convoy { namespace, name }) => {
                find_convoy(convoys, &namespace, &name).and_then(|convoy| (convoy.vessels.len() == 1).then(|| &convoy.vessels[0]))
            }
            _ => None,
        } {
            facts.extend(direct_facts(vessel.cleat_endpoint.as_ref(), &vessel.host, mint));
        }
    }
    catalog.assert_entity(entity, facts, None);
}

fn awareness_entry_entity(entry: &AwarenessEntry, convoys: &[ConvoyRow]) -> Option<(EntityRef, Vec<(&'static str, MetadataValue)>)> {
    let mut label = entry.label.clone();
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
            if let Some(row) = row {
                facts.push((KEY_CONVOY_PHASE, MetadataValue::text(row.phase.as_str())));
            }
            label = semantic_label.to_owned();
            facts.extend(label_tier_facts(semantic_label));
            if let (None, Some(AwarenessPhase::Convoy(phase))) = (row, &entry.phase) {
                facts.push((KEY_CONVOY_PHASE, MetadataValue::text(phase.as_str())));
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
            let convoy_label = row.map(|row| row.name.as_str()).unwrap_or(convoy_name);
            let vessel_origin = find_vessel(convoys, namespace, convoy_name, vessel_name)
                .map(|vessel| vessel.host.to_string())
                .unwrap_or_else(|| origin.clone());
            let entity = entity::vessel(namespace, convoy_name, vessel_name, &vessel_origin);
            let mut facts = vec![
                (KEY_CONVOY, MetadataValue::text(convoy.id)),
                (KEY_CONVOY_NAME, MetadataValue::text(convoy_label)),
                (KEY_VESSEL, MetadataValue::text(entity.id.clone())),
                (KEY_VESSEL_NAME, MetadataValue::text(label.clone())),
            ];
            if let Some(row) = row {
                facts.push((KEY_CONVOY_PHASE, MetadataValue::text(row.phase.as_str())));
            }
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
    convoys.iter().find(|convoy| convoy.resource.namespace == namespace && convoy.resource.name == name)
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

fn convoy_identity_facts(convoy: &ConvoyRow) -> Vec<(&'static str, MetadataValue)> {
    let target = entity::convoy(&convoy.resource.namespace, &convoy.resource.name, &entity::resource_origin(&convoy.resource));
    let mut facts = vec![
        (KEY_CONVOY, MetadataValue::text(target.id)),
        (KEY_CONVOY_NAME, MetadataValue::text(&convoy.name)),
        (KEY_DISPLAY_LABEL, MetadataValue::text(&convoy.name)),
        (KEY_CONVOY_PHASE, MetadataValue::text(convoy.phase.as_str())),
    ];
    facts.extend(label_tier_facts(&convoy.name));
    facts
}

/// Apply readiness identically to raw-row and awareness catalogs, without
/// creating entities that the chosen catalog view did not include.
fn project_readiness(catalog: &mut Catalog, convoys: &[ConvoyRow]) {
    fn apply(catalog: &mut Catalog, entity: EntityRef, readiness: &flotilla_protocol::result_set::Readiness, attach: bool) {
        let Some(facts) = catalog.facts.get_mut(&MetadataTarget::Entity(entity)) else {
            return;
        };
        if let Some(status) = facts.get_mut(KEY_STATUS_STATE) {
            if status.value == MetadataValue::text("active") {
                let badge = readiness_badge(Badge { state: BadgeState::Active, attention: false }, readiness);
                status.value = MetadataValue::text(badge.state.as_str());
            }
        }
        for (key, value) in readiness_facts(readiness, attach) {
            facts.insert(key.into(), MetadataValueUpdate::new(value, Some(CATALOG_TTL_MS)));
        }
    }
    for convoy in convoys {
        let origin = entity::resource_origin(&convoy.resource);
        apply(
            catalog,
            entity::convoy(&convoy.resource.namespace, &convoy.resource.name, &origin),
            &convoy.readiness(),
            matches!(convoy.vessels.as_slice(), [vessel] if vessel.materialize.is_some()),
        );
        for vessel in &convoy.vessels {
            apply(
                catalog,
                entity::vessel(&convoy.resource.namespace, &convoy.resource.name, &vessel.name, vessel.host.as_str()),
                &vessel.readiness,
                vessel.materialize.is_some(),
            );
        }
    }
}

fn readiness_facts(readiness: &flotilla_protocol::result_set::Readiness, attach_available: bool) -> Vec<(&'static str, MetadataValue)> {
    use crate::keys::{KEY_READINESS_ATTACH_AVAILABLE, KEY_READINESS_BLOCKERS, KEY_READINESS_STATE};
    vec![
        (KEY_READINESS_STATE, MetadataValue::text(readiness.state.as_str())),
        (
            KEY_READINESS_BLOCKERS,
            MetadataValue::StringList(
                readiness.blockers.iter().map(|blocker| serde_json::to_string(blocker).expect("readiness blocker serializes")).collect(),
            ),
        ),
        (KEY_READINESS_ATTACH_AVAILABLE, MetadataValue::Bool(attach_available)),
    ]
}

fn readiness_badge(badge: Badge, readiness: &flotilla_protocol::result_set::Readiness) -> Badge {
    use flotilla_protocol::result_set::ReadinessState;
    if badge.state != BadgeState::Active {
        return badge;
    }
    match readiness.state {
        ReadinessState::Provisioning | ReadinessState::Blocked | ReadinessState::Unknown => {
            Badge { state: BadgeState::Waiting, attention: badge.attention }
        }
        ReadinessState::Failed => Badge { state: BadgeState::Failed, attention: true },
        ReadinessState::Ready => badge,
    }
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
        assert_repo_entity(catalog, repo);
    }
    let convoy_entity = entity::convoy(namespace, &convoy.resource.name, &origin);
    let ordinal = (project.is_none() && repo.is_none()).then_some(ARCHIPELAGO_ORDINAL);
    let badge = convoy_badge(convoy.phase, convoy.initializing);
    let done = convoy.vessels.iter().filter(|vessel| vessel.phase == WorkPhase::Complete).count();
    let mut facts = project_facts(&project);
    if let Some(repo) = &repo {
        facts.push((SEGMENT_REPO, MetadataValue::text(repo.clone())));
        facts.push((KEY_REPO_NAME, MetadataValue::text(repo_label(repo))));
    }
    facts.extend(convoy_identity_facts(convoy));
    facts.extend([
        (KEY_CONVOY_WORKFLOW, MetadataValue::text(convoy.workflow_ref.clone())),
        (KEY_STATUS_STATE, MetadataValue::text(badge.state.as_str())),
    ]);
    facts.extend(surface_facts(convoy.surface_state));
    if let Some(message) = &convoy.message {
        facts.push((KEY_CONVOY_MESSAGE, MetadataValue::text(message.clone())));
    }
    if convoy.surface_state.needs_attention() {
        facts.push((KEY_STATUS_ATTENTION, MetadataValue::Bool(true)));
    }
    if let Some(message) = &convoy.message {
        facts.push((KEY_SUMMARY_TEXT, MetadataValue::text(message.clone())));
    } else if !convoy.vessels.is_empty() {
        facts.push((KEY_SUMMARY_TEXT, MetadataValue::text(format!("{done}/{} vessels done", convoy.vessels.len()))));
    }
    if let [vessel] = convoy.vessels.as_slice() {
        if let Some(recipe) = vessel.materialize.as_deref().and_then(|attach_ref| mint.attach(attach_ref, &vessel.host)) {
            let target = entity::vessel(namespace, &convoy.resource.name, &vessel.name, vessel.host.as_str());
            facts.extend(action_facts(&target, &recipe, "workspace"));
            facts.extend(direct_facts(vessel.cleat_endpoint.as_ref(), &vessel.host, mint));
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
    let entity = entity::vessel(&convoy.resource.namespace, &convoy.resource.name, &vessel.name, vessel.host.as_str());
    let origin = entity::resource_origin(&convoy.resource);
    let convoy_entity = entity::convoy(&convoy.resource.namespace, &convoy.resource.name, &origin);
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
        (KEY_CONVOY_PHASE, MetadataValue::text(convoy.phase.as_str())),
        (KEY_VESSEL, MetadataValue::text(entity.id.clone())),
        (KEY_VESSEL_NAME, MetadataValue::text(vessel.name.clone())),
        (KEY_DISPLAY_LABEL, MetadataValue::text(vessel.name.clone())),
        (KEY_WORK_PHASE, MetadataValue::text(vessel.phase.as_str())),
        (KEY_VESSEL_HOST, MetadataValue::text(vessel.host.to_string())),
        (KEY_STATUS_STATE, MetadataValue::text(badge.state.as_str())),
    ]);
    facts.extend(label_tier_facts(&vessel.name));
    facts.extend(surface_facts(vessel.surface_state));
    if !vessel.crew.is_empty() {
        facts.push((KEY_CREW_ROLES, MetadataValue::StringList(vessel.crew.iter().map(|member| member.role.clone()).collect())));
    }
    if vessel.surface_state.needs_attention() {
        facts.push((KEY_STATUS_ATTENTION, MetadataValue::Bool(true)));
    }
    if let Some(message) = &vessel.message {
        facts.push((KEY_SUMMARY_TEXT, MetadataValue::text(message.clone())));
    }
    if let Some(recipe) = vessel.materialize.as_deref().and_then(|attach_ref| mint.attach(attach_ref, &vessel.host)) {
        facts.extend(action_facts(&entity, &recipe, "workspace"));
        facts.extend(direct_facts(vessel.cleat_endpoint.as_ref(), &vessel.host, mint));
    }
    catalog.assert_entity(entity, facts, ordinal);
}

fn project_independent(catalog: &mut Catalog, independent: &IndependentRow, mint: &dyn RecipeMint) {
    let namespace = &independent.resource.namespace;
    let session_ref = format!("{}/{namespace}/{}", independent.host, independent.name);
    let entity = entity::session(&session_ref);
    let repo = independent.repo_fact.as_ref().map(|repo| repo.0.as_str());
    if let Some(repo) = repo {
        assert_repo_entity(catalog, repo);
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
        facts.extend(direct_facts(independent.cleat_endpoint.as_ref(), &independent.host, mint));
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

fn assert_repo_entity(catalog: &mut Catalog, repo: &str) {
    let facts = vec![
        (SEGMENT_REPO, MetadataValue::text(repo)),
        (KEY_REPO_NAME, MetadataValue::text(repo_label(repo))),
        (KEY_DISPLAY_LABEL, MetadataValue::text(repo_label(repo))),
    ];
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

fn direct_facts(endpoint: Option<&CleatEndpoint>, host: &HostName, mint: &dyn RecipeMint) -> Vec<(&'static str, MetadataValue)> {
    let Some(endpoint) = endpoint else {
        return vec![(KEY_PRIMARY_DIRECT_REASON, MetadataValue::text("no observed Cleat daemon endpoint for this session"))];
    };
    match mint.direct_transport(host) {
        Ok(transport) => {
            let (kind, address) = match transport {
                DirectTransport::Local => ("local", host.as_str().to_owned()),
                DirectTransport::Ssh(destination) => ("ssh", destination),
            };
            vec![
                (KEY_PRIMARY_DIRECT_TRANSPORT, MetadataValue::text(kind)),
                (KEY_PRIMARY_DIRECT_HOST, MetadataValue::text(address)),
                (KEY_PRIMARY_DIRECT_RUNTIME_ROOT, MetadataValue::text(&endpoint.runtime_root)),
                (KEY_PRIMARY_DIRECT_DAEMON, MetadataValue::text(&endpoint.daemon)),
                (KEY_PRIMARY_DIRECT_SESSION, MetadataValue::text(&endpoint.session)),
            ]
        }
        Err(reason) => vec![(KEY_PRIMARY_DIRECT_REASON, MetadataValue::text(reason))],
    }
}

#[cfg(test)]
mod tests;
