//! Account access at admission (#3907): requirements resolve through grants into authorized
//! alternatives per requester, a registered plan is checked per candidate environment kind, and
//! one credential is selected per requester and stored as its binding.
//!
//! The contract is #3879 (matching), #3881 (plans), #3882 (bindings; selection amended by #3884
//! to a seeded uniform draw) and #3910's owner rulings.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_agent::{
    account_access::{AccountProvisioners, EnvironmentKind},
    adapter::CapabilityTable,
};
use flotilla_protocol::{PlacementDecision, PlacementTargetHost};
use flotilla_resources::{
    account_access::{
        binding_name, requester_address, requester_parts, AccessName, AccountBindingSpec, AccountRecords, BindingRecord, BindingState,
        CredentialName, EpochEntry, GrantName, GrantScope, PendingBinding, Selection,
    },
    Convoy, ConvoyRepositorySpec, CredentialGrant, CredentialSpec, CrewSource, Host as ResourceHost, PlacementPolicy, PlacementPolicySpec,
    Repository, RepositoryKey, RepositoryTrust, ResourceError, ResourceObject, WorkflowTemplateSpec,
};
use flotilla_schema::{Records, State, Timestamp};
use flotilla_store::{ResourceBackend, StoreQuery};

use crate::random::RandomSource;
use crate::support::{authoritative_placement_host, is_agentless_ssh_host, placement_target_host};

/// The requester an alternative set belongs to: one agent crew of one vessel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requester {
    pub vessel: String,
    pub role: String,
    /// The item that declared the requirement.
    pub item: String,
    pub access: AccessName,
}

/// One authorized candidate credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The account the declaration names.
    pub account: Option<String>,
    /// Every grant authorizing it for this requester, with the scope it permits.
    pub grants: BTreeMap<GrantName, GrantScope>,
    /// The credential's current authorization epoch.
    pub epoch: u32,
}

/// Authorized alternatives for one requester's requirement, with the candidates known to be
/// unusable at the placement under consideration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequesterAlternatives {
    pub requester: Requester,
    pub candidates: BTreeMap<CredentialName, Candidate>,
    pub exclusions: BTreeMap<CredentialName, String>,
}

impl RequesterAlternatives {
    /// Candidates less exclusions, in the sorted order a draw indexes.
    pub fn usable(&self) -> Vec<&CredentialName> {
        self.candidates.keys().filter(|name| !self.exclusions.contains_key(*name)).collect()
    }
}

/// The scope a grant permits for the Claude subscription family: the whole account. Core
/// records it and never computes containment over it (#3881).
const ACCOUNT_SCOPE: &str = "account";

/// Who pinned a binding chosen from `convoy start --account`.
pub const CONVOY_START_PIN: &str = "convoy start --account";

/// The candidate environment kind a placement policy resolves to on its target host.
pub fn placement_environment_kind(spec: &PlacementPolicySpec, host: Option<&ResourceObject<ResourceHost>>) -> EnvironmentKind {
    EnvironmentKind::of_policy(spec, host.is_some_and(is_agentless_ssh_host))
}

/// The reason a candidate environment kind is refused: some requester's access has no plan
/// there (#3881). The kind drops out of placement with this reason.
pub fn plan_refusal(workflow: &WorkflowTemplateSpec, kind: EnvironmentKind) -> Result<Option<String>, String> {
    let provisioners = AccountProvisioners::seeded();
    for requester in requesters(workflow)? {
        let provisioner = provisioners
            .for_access(&requester.access)
            .ok_or_else(|| format!("no registered implementation supplies `{}` for role `{}`", requester.access, requester.role))?;
        if let Err(reason) = provisioner.plan(kind) {
            return Ok(Some(format!("role `{}` needs `{}`: {reason}", requester.role, requester.access)));
        }
    }
    Ok(None)
}

/// The requesters in a workflow whose items declare account access.
fn requesters(workflow: &WorkflowTemplateSpec) -> Result<Vec<Requester>, String> {
    let capabilities = CapabilityTable::seeded();
    let mut requesters = Vec::new();
    for vessel in &workflow.vessels {
        for crew in &vessel.crew {
            let CrewSource::Agent { selector, .. } = &crew.source else { continue };
            let requirement = capabilities.resolve_selector(selector)?;
            if let Some(access) = requirement.account_access() {
                requesters.push(Requester {
                    vessel: vessel.name.clone(),
                    role: crew.role.clone(),
                    item: requirement.adapter.clone(),
                    access: access.clone(),
                });
            }
        }
    }
    Ok(requesters)
}

/// Resolve every requester's authorized alternatives (#3879): applicable grants contribute the
/// credentials that a registered implementation says supply the requested access. A credential
/// without an applicable grant is never a candidate; one granted by several grants keeps each.
pub async fn authorized_alternatives(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    repositories: &[ConvoyRepositorySpec],
    workflow: &WorkflowTemplateSpec,
) -> Result<Vec<RequesterAlternatives>, String> {
    let alternatives = current_alternatives(backend, namespace, project_ref, repositories, workflow).await?;
    if let Some(unauthorized) = alternatives.iter().find(|alternative| alternative.candidates.is_empty()) {
        let requester = &unauthorized.requester;
        return Err(format!(
            "agent adapter `{}` for role `{}` requires `{}`, but no CredentialGrant authorizes a credential that supplies it",
            requester.item, requester.role, requester.access
        ));
    }
    Ok(alternatives)
}

/// Every requester's alternatives as the grants stand now, including requesters no grant
/// authorizes any longer (an empty candidate set).
pub async fn current_alternatives(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    repositories: &[ConvoyRepositorySpec],
    workflow: &WorkflowTemplateSpec,
) -> Result<Vec<RequesterAlternatives>, String> {
    let requesters = requesters(workflow)?;
    if requesters.is_empty() {
        return Ok(Vec::new());
    }
    let provisioners = AccountProvisioners::seeded();
    let grants = backend
        .query_declared::<CredentialGrant>(
            namespace,
            &crate::convoy_admission::credential_grant_query(
                project_ref,
                requesters.iter().map(|requester| requester.role.clone()).collect(),
            ),
        )
        .await
        .map_err(|error| format!("list credential grants: {error}"))?
        .items
        .into_iter()
        // A withdrawn grant (a definition's deletion tombstone) authorizes nothing.
        .filter(|grant| grant.object.metadata.deletion_timestamp.is_none())
        .collect::<Vec<_>>();
    let specs = backend
        .query_declared::<CredentialSpec>(
            namespace,
            &StoreQuery::names(grants.iter().flat_map(|grant| grant.object.spec.credentials.iter().cloned()), true),
        )
        .await
        .map_err(|error| format!("list credential specs: {error}"))?
        .items
        .into_iter()
        .filter(|source| source.object.metadata.deletion_timestamp.is_none())
        .map(|source| (source.object.metadata.name, source.object.spec))
        .collect::<BTreeMap<_, _>>();
    let all_repositories = repositories.iter().map(|repository| repository.repo_ref.clone()).collect::<BTreeSet<_>>();
    let trust = backend
        .query_declared::<Repository>(
            namespace,
            &StoreQuery::names(
                all_repositories
                    .iter()
                    .map(ToString::to_string)
                    .chain(workflow.vessels.iter().flat_map(|vessel| vessel.repository_refs.iter().flatten().map(ToString::to_string))),
                true,
            ),
        )
        .await
        .map_err(|error| format!("list repositories for credential grants: {error}"))?
        .items
        .into_iter()
        .map(|source| {
            (
                RepositoryKey(source.object.metadata.name),
                if source.object.spec.is_fork() { RepositoryTrust::Fork } else { RepositoryTrust::Own },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut resolved = Vec::new();
    for requester in requesters {
        let provisioner = provisioners
            .for_access(&requester.access)
            .ok_or_else(|| format!("no registered implementation supplies `{}` for role `{}`", requester.access, requester.role))?;
        let vessel = workflow.vessels.iter().find(|vessel| vessel.name == requester.vessel).expect("requester's vessel");
        let vessel_repositories =
            vessel.repository_refs.as_ref().map(|refs| refs.iter().cloned().collect()).unwrap_or_else(|| all_repositories.clone());
        let repository_trust = vessel_repositories
            .iter()
            .map(|key| {
                trust
                    .get(key)
                    .copied()
                    .map(|trust| (key.clone(), trust))
                    .ok_or_else(|| format!("repository `{key}` unavailable for credential grant selection"))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let mut candidates = BTreeMap::<CredentialName, Candidate>::new();
        for grant in grants.iter().filter(|grant| grant.object.spec.selector.matches(project_ref, &repository_trust, &requester.role)) {
            for name in &grant.object.spec.credentials {
                let Some(spec) = specs.get(name) else { continue };
                let Some(supplied) = provisioner.supplies(&spec.consumer) else { continue };
                candidates
                    .entry(CredentialName(name.clone()))
                    .or_insert_with(|| Candidate { account: supplied.account, grants: BTreeMap::new(), epoch: spec.authorization_epoch })
                    .grants
                    .insert(GrantName(grant.object.metadata.name.clone()), GrantScope { scope: ACCOUNT_SCOPE.to_string() });
            }
        }
        resolved.push(RequesterAlternatives { requester, candidates, exclusions: BTreeMap::new() });
    }
    Ok(resolved)
}

/// Check alternatives against one placement: the kind must have a plan for every requirement
/// (#3881), and every candidate the placement host does not hold is excluded with its reason.
/// A requester left with no usable candidate refuses the placement.
pub fn check_placement(
    alternatives: &mut [RequesterAlternatives],
    kind: EnvironmentKind,
    host_label: &str,
    held: Option<&BTreeSet<String>>,
) -> Result<(), String> {
    let provisioners = AccountProvisioners::seeded();
    for alternative in alternatives.iter_mut() {
        let provisioner = provisioners.for_access(&alternative.requester.access).expect("resolved alternatives have an implementation");
        provisioner.plan(kind)?;
        if let Some(held) = held {
            for name in alternative.candidates.keys() {
                if !held.contains(&name.0) {
                    alternative.exclusions.insert(name.clone(), format!("host `{host_label}` does not hold it"));
                }
            }
        }
        if alternative.usable().is_empty() {
            let reasons = alternative.exclusions.iter().map(|(name, reason)| format!("`{name}`: {reason}")).collect::<Vec<_>>().join("; ");
            return Err(format!(
                "role `{}` has no usable credential for `{}`: {reasons}",
                alternative.requester.role, alternative.requester.access
            ));
        }
    }
    Ok(())
}

/// Resolve alternatives and check them against a placement policy, as admission does for each
/// candidate placement.
pub async fn validate_account_access(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    repositories: &[ConvoyRepositorySpec],
    workflow: &WorkflowTemplateSpec,
    placement: Option<&ResourceObject<PlacementPolicy>>,
) -> Result<Vec<RequesterAlternatives>, String> {
    let mut alternatives = authorized_alternatives(backend, namespace, project_ref, repositories, workflow).await?;
    if alternatives.is_empty() {
        return Ok(alternatives);
    }
    let Some(placement) = placement else { return Ok(alternatives) };
    let target = placement_target_host(backend, namespace, placement).await?;
    check_at_host(backend, namespace, &mut alternatives, &placement.spec, &target, &placement.metadata.name).await?;
    Ok(alternatives)
}

async fn check_at_host(
    backend: &ResourceBackend,
    namespace: &str,
    alternatives: &mut [RequesterAlternatives],
    spec: &PlacementPolicySpec,
    target: &PlacementTargetHost,
    placement_name: &str,
) -> Result<(), String> {
    let host = authoritative_placement_host(backend, namespace, target, placement_name).await.ok();
    let kind = placement_environment_kind(spec, host.as_ref());
    let held = host.as_ref().and_then(|host| host.status.as_ref()).and_then(|status| status.held_credentials().ok());
    check_placement(alternatives, kind, &target.display_name, held.as_ref())
}

/// A selected credential and how it was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    pub credential: CredentialName,
    pub selection: Selection,
}

/// Select one credential (#3882 as amended by #3884): an authorized, usable pin wins; an
/// unauthorized or unusable pin refuses; otherwise a uniform draw over the usable candidates.
pub fn select(alternatives: &RequesterAlternatives, pin: Option<(&str, &str)>, random: &dyn RandomSource) -> Result<Selected, String> {
    let requester = &alternatives.requester;
    if let Some((pin, pinned_by)) = pin {
        let credential = CredentialName(pin.to_string());
        if !alternatives.candidates.contains_key(&credential) {
            return Err(format!(
                "account pin `{pin}` for role `{}` is refused: no CredentialGrant authorizes it for `{}`",
                requester.role, requester.access
            ));
        }
        if let Some(reason) = alternatives.exclusions.get(&credential) {
            return Err(format!("account pin `{pin}` for role `{}` is refused: {reason}", requester.role));
        }
        return Ok(Selected { credential, selection: Selection::Pin { pinned_by: pinned_by.to_string() } });
    }
    let usable = alternatives.usable();
    if usable.is_empty() {
        return Err(format!("role `{}` has no usable credential for `{}`", requester.role, requester.access));
    }
    let bound = u32::try_from(usable.len()).map_err(|_| "too many candidates for a draw".to_string())?;
    let index = random.below(bound);
    Ok(Selected { credential: usable[index as usize].clone(), selection: Selection::Draw { index } })
}

/// Build the record for a selection.
pub fn binding_record(
    project: Option<&str>,
    convoy: &str,
    alternatives: &RequesterAlternatives,
    selected: Selected,
    now: Timestamp,
) -> Result<PendingBinding, String> {
    let requester = &alternatives.requester;
    let candidate = &alternatives.candidates[&selected.credential];
    let mut epochs = Records::new();
    epochs
        .append(1, EpochEntry { epoch: candidate.epoch.to_string(), observed_at: now })
        .map_err(|error| format!("epoch lineage: {error:?}"))?;
    Ok(PendingBinding {
        name: binding_name(convoy, &requester.role, &requester.access),
        spec: AccountBindingSpec {
            requester: requester_address(project, convoy, &requester.role)?,
            item: requester.item.clone(),
            requirement: requester.access.clone(),
            credential: selected.credential.clone(),
            account: candidate.account.clone(),
            grants: candidate.grants.clone(),
            candidates: alternatives.candidates.keys().cloned().collect(),
            exclusions: alternatives.exclusions.clone(),
            selection: selected.selection,
            epochs,
            opened_at: now,
            state: State::new(BindingState::Open, now),
        },
    })
}

/// Choose bindings for every requester of an admitted workflow at its final placements.
/// `placements` maps each vessel to its placement; a vessel absent from it uses `fallback`.
#[allow(clippy::too_many_arguments)]
pub async fn choose_bindings(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    repositories: &[ConvoyRepositorySpec],
    convoy: &str,
    workflow: &WorkflowTemplateSpec,
    placements: &BTreeMap<String, (PlacementPolicySpec, PlacementDecision)>,
    fallback: Option<(&PlacementPolicySpec, &PlacementDecision)>,
    pins: &BTreeMap<String, String>,
    random: &dyn RandomSource,
    now: Timestamp,
) -> Result<Vec<PendingBinding>, String> {
    let mut alternatives = authorized_alternatives(backend, namespace, project_ref, repositories, workflow).await?;
    let roles = alternatives.iter().map(|alternative| alternative.requester.role.as_str()).collect::<BTreeSet<_>>();
    if let Some(role) = pins.keys().find(|role| !roles.contains(role.as_str())) {
        return Err(format!("account pin names role `{role}`, which has no account requirement in this workflow"));
    }
    let mut pending = Vec::new();
    for alternative in &mut alternatives {
        let placement = placements.get(&alternative.requester.vessel).map(|(spec, decision)| (spec, decision)).or(fallback);
        if let Some((spec, decision)) = placement {
            check_at_host(backend, namespace, std::slice::from_mut(alternative), spec, &decision.target_host, &decision.policy_name)
                .await?;
        }
        let pin = pins.get(&alternative.requester.role).map(|pin| (pin.as_str(), CONVOY_START_PIN));
        let selected = select(alternative, pin, random)?;
        pending.push(binding_record(project_ref, convoy, alternative, selected, now)?);
    }
    Ok(pending)
}

/// Copy the account authority's recorded keys into the frozen per-crew entries.
pub fn apply_crew_home_keys(
    convoy: &str,
    vessels: &mut [flotilla_resources::VesselRequirement],
    keys: &BTreeMap<String, String>,
) -> Result<bool, String> {
    let mut changed = false;
    for crew in vessels.iter_mut().flat_map(|vessel| &mut vessel.crew) {
        let CrewSource::Agent { selector, .. } = &crew.source else { continue };
        let requirement = CapabilityTable::seeded().resolve_selector(selector)?;
        let key = if requirement.account_access().is_some() {
            keys.get(&crew.role).cloned()
        } else {
            Some(flotilla_agent::account_access::unbound_crew_home_key(convoy, &crew.role, &requirement.adapter))
        };
        let before = crew.labels.remove(flotilla_resources::account_access::CREW_HOME_KEY_LABEL);
        if let Some(key) = &key {
            crew.labels.insert(flotilla_resources::account_access::CREW_HOME_KEY_LABEL.into(), key.clone());
        }
        changed |= before != key;
    }
    Ok(changed)
}

pub fn pending_home_keys(bindings: &[PendingBinding]) -> BTreeMap<String, String> {
    bindings
        .iter()
        .filter_map(|binding| {
            let (_, role) = flotilla_resources::account_access::requester_parts(&binding.spec.requester)?;
            let key =
                binding_meta(&binding.name, &binding.spec).labels.get(flotilla_resources::account_access::CREW_HOME_KEY_LABEL)?.clone();
            Some((role, key))
        })
        .collect()
}

/// Recover previous-generation bindings at their authority before publishing caller home keys.
pub async fn recover_home_keys(backend: &ResourceBackend, namespace: &str, convoy: &str) -> Result<BTreeMap<String, String>, String> {
    let bindings = backend.using::<AccountBinding>(namespace);
    let records = bindings
        .list_matching_labels(&BTreeMap::from([(flotilla_resources::CONVOY_LABEL.into(), convoy.into())]))
        .await
        .map_err(|error| error.to_string())?;
    let mut keys = BTreeMap::new();
    for mut binding in records.items {
        if !binding.spec.state.value.is_open() {
            continue;
        }
        if !binding.metadata.labels.contains_key(flotilla_resources::account_access::CREW_HOME_KEY_LABEL) {
            binding = bindings
                .update(&binding_meta(&binding.metadata.name, &binding.spec), &binding.metadata.resource_version, &binding.spec)
                .await
                .map_err(|error| error.to_string())?;
        }
        if let Some((_, role)) = flotilla_resources::account_access::requester_parts(&binding.spec.requester) {
            if let Some(key) = binding.metadata.labels.get(flotilla_resources::account_access::CREW_HOME_KEY_LABEL) {
                keys.insert(role, key.clone());
            }
        }
    }
    Ok(keys)
}

/// The reason a withdrawn authorization records (#3882).
pub const NO_AUTHORIZING_GRANT: &str = "binding revoked: no authorizing grant";

/// Who pinned a binding chosen by `convoy rebind --account`.
pub const REBIND_PIN: &str = "convoy rebind --account";

/// Who pinned a binding adopted from a convoy admitted before bindings existed: its snapshot
/// already carried exactly one subscription credential, and the account stays (#3882).
pub const SNAPSHOT_PIN: &str = "admitted workflow snapshot";

/// What the home pass did to one binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingChange {
    Opened,
    GrantsUpdated,
    EpochAppended(String),
    Revoked(String),
    Closed(String),
}

/// A convoy's admitted workflow, as alternatives are resolved against it: the snapshot in its
/// status, or before the convoy reconciler copies it there, the one admission pinned.
async fn admitted_workflow(
    backend: &ResourceBackend,
    namespace: &str,
    convoy: &ResourceObject<Convoy>,
) -> Result<Option<WorkflowTemplateSpec>, String> {
    if let Some(snapshot) = convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()) {
        return Ok(Some(WorkflowTemplateSpec::builder().vessels(snapshot.vessels.clone()).build()));
    }
    let Some(pinned) = convoy.metadata.annotations.get(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION) else { return Ok(None) };
    match backend.definitions::<flotilla_resources::WorkflowTemplate>(namespace).get(pinned).await {
        Ok(snapshot) => Ok(Some(snapshot.spec)),
        Err(ResourceError::NotFound { .. }) => Ok(None),
        Err(error) => Err(format!("read admitted workflow `{pinned}`: {error}")),
    }
}

/// Whether any place still holds a delivered copy of a binding's credential.
async fn delivered_anywhere(accounts: &dyn AccountRecords, namespace: &str, binding: &str) -> Result<bool, String> {
    let provisions = accounts.binding_provisions(namespace, &[binding.to_string()], false).await?;
    Ok(provisions.iter().any(|provision| provision.spec.delivered.is_some()))
}

/// A requester the home pass found without a binding; it is bound under the admission lock.
pub struct Unbound {
    alternatives: RequesterAlternatives,
    /// The single subscription credential the admitted snapshot carried, which stays bound.
    carried: Option<String>,
}

/// The home pass over one convoy's requesters (#3882 lifecycle): teardown closes their bindings;
/// an authorization change revokes a binding or updates its recorded grants; a replacement login
/// for the same account appends an epoch. Requesters with no binding (a convoy admitted before
/// bindings existed) are returned for [`bind_unbound`].
pub async fn reconcile_convoy_bindings(
    backend: &ResourceBackend,
    accounts: &dyn AccountRecords,
    namespace: &str,
    convoy: &ResourceObject<Convoy>,
    now: Timestamp,
) -> Result<(Vec<(String, BindingChange)>, Vec<Unbound>), String> {
    let bindings = accounts.convoy_bindings(namespace, &convoy.metadata.name, true).await?;
    let mut changes = Vec::new();
    if convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
        for binding in bindings.iter().filter(|binding| binding.spec.state.value.is_open()) {
            let reason = "requester teardown".to_string();
            let delivered_until = delivered_anywhere(accounts, namespace, &binding.name).await?.then_some(now);
            accounts.end(namespace, binding, BindingState::Closed { reason: reason.clone(), delivered_until }, now).await?;
            changes.push((binding.name.clone(), BindingChange::Closed(reason)));
        }
        return Ok((changes, Vec::new()));
    }
    let Some(workflow) = admitted_workflow(backend, namespace, convoy).await? else { return Ok((changes, Vec::new())) };
    let alternatives =
        current_alternatives(backend, namespace, convoy.spec.project_ref.as_deref(), &convoy.spec.repositories, &workflow).await?;
    for binding in bindings.iter().filter(|binding| binding.spec.state.value.is_open()) {
        if let Some(change) = reconcile_open_binding(accounts, namespace, binding, &alternatives, now).await? {
            changes.push((binding.name.clone(), change));
        }
    }
    let bound = bindings
        .iter()
        .filter_map(|binding| requester_parts(&binding.spec.requester).map(|(_, role)| (role, binding.spec.requirement.clone())))
        .collect::<BTreeSet<_>>();
    let unbound = alternatives
        .into_iter()
        .filter(|alternative| !alternative.candidates.is_empty())
        .filter(|alternative| !bound.contains(&(alternative.requester.role.clone(), alternative.requester.access.clone())))
        .map(|alternative| {
            let carried = workflow
                .vessels
                .iter()
                .find(|vessel| vessel.name == alternative.requester.vessel)
                .map(|vessel| {
                    vessel
                        .credential_refs
                        .iter()
                        .filter(|name| alternative.candidates.contains_key(&CredentialName((*name).clone())))
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .and_then(|carried| <[String; 1]>::try_from(carried).ok())
                .map(|[single]| single);
            Unbound { alternatives: alternative, carried }
        })
        .collect();
    Ok((changes, unbound))
}

/// One open binding against its requester's current alternatives: revoked when no grant
/// authorises its credential or a replacement login names another account; its grants updated;
/// a replacement login for the same account appended as the next epoch.
async fn reconcile_open_binding(
    accounts: &dyn AccountRecords,
    namespace: &str,
    binding: &BindingRecord,
    alternatives: &[RequesterAlternatives],
    now: Timestamp,
) -> Result<Option<BindingChange>, String> {
    let Some((_, role)) = requester_parts(&binding.spec.requester) else { return Ok(None) };
    let spec = &binding.spec;
    let candidate = alternatives
        .iter()
        .find(|alternative| alternative.requester.role == role && alternative.requester.access == spec.requirement)
        .and_then(|alternative| alternative.candidates.get(&spec.credential));
    let Some(candidate) = candidate else {
        let delivered_until = delivered_anywhere(accounts, namespace, &binding.name).await?.then_some(now);
        let state = BindingState::Revoked { reason: NO_AUTHORIZING_GRANT.to_string(), delivered_until };
        accounts.end(namespace, binding, state, now).await?;
        return Ok(Some(BindingChange::Revoked(NO_AUTHORIZING_GRANT.to_string())));
    };
    let current_epoch = spec.epochs.last().and_then(|row| row.record.epoch.parse::<u32>().ok()).unwrap_or(0);
    if candidate.epoch > current_epoch && candidate.account != spec.account {
        let reason = format!(
            "replacement refused: credential `{}` now declares account `{}`, not the bound `{}`",
            spec.credential,
            candidate.account.as_deref().unwrap_or("none"),
            spec.account.as_deref().unwrap_or("none")
        );
        let delivered_until = delivered_anywhere(accounts, namespace, &binding.name).await?.then_some(now);
        accounts.end(namespace, binding, BindingState::Revoked { reason: reason.clone(), delivered_until }, now).await?;
        return Ok(Some(BindingChange::Revoked(reason)));
    }
    let mut change = None;
    let mut current = binding.clone();
    if spec.grants != candidate.grants {
        accounts.regrant(namespace, &current, candidate.grants.clone()).await?;
        change = Some(BindingChange::GrantsUpdated);
        // The epoch below is a second write: read the binding at the version the regrant left.
        if candidate.epoch > current_epoch {
            current = reread(accounts, namespace, binding).await?;
        }
    }
    if candidate.epoch > current_epoch {
        accounts.observe_epoch(namespace, &current, EpochEntry { epoch: candidate.epoch.to_string(), observed_at: now }).await?;
        change = Some(BindingChange::EpochAppended(candidate.epoch.to_string()));
    }
    Ok(change)
}

/// `binding` as its convoy's bindings read now.
async fn reread(accounts: &dyn AccountRecords, namespace: &str, binding: &BindingRecord) -> Result<BindingRecord, String> {
    let convoy = requester_parts(&binding.spec.requester).map(|(convoy, _)| convoy).unwrap_or_default();
    accounts
        .convoy_bindings(namespace, &convoy, true)
        .await?
        .into_iter()
        .find(|current| current.name == binding.name)
        .ok_or_else(|| format!("account binding `{}` is gone", binding.name))
}

/// Bind requesters the home pass found without a binding; the caller holds the admission lock.
pub async fn bind_unbound(
    accounts: &dyn AccountRecords,
    namespace: &str,
    convoy: &ResourceObject<Convoy>,
    unbound: Vec<Unbound>,
    random: &dyn RandomSource,
    now: Timestamp,
) -> Result<Vec<(String, BindingChange)>, String> {
    let mut pending = Vec::new();
    for Unbound { alternatives, carried } in unbound {
        let selected = select(&alternatives, carried.as_deref().map(|credential| (credential, SNAPSHOT_PIN)), random)?;
        pending.push(binding_record(convoy.spec.project_ref.as_deref(), &convoy.metadata.name, &alternatives, selected, now)?);
    }
    let changes = pending.iter().map(|binding| (binding.name.clone(), BindingChange::Opened)).collect();
    accounts.open(namespace, pending).await?;
    Ok(changes)
}

/// An explicit rebind (#3882 lifecycle): the old binding is closed and kept for attribution, and
/// a new one opens by pin or draw for the requester. Its places see the new opening, remove the
/// old copies and start a fresh harness session.
#[allow(clippy::too_many_arguments)]
pub async fn rebind(
    backend: &ResourceBackend,
    accounts: &dyn AccountRecords,
    namespace: &str,
    convoy_name: &str,
    role: &str,
    pin: Option<&str>,
    random: &dyn RandomSource,
    now: Timestamp,
) -> Result<AccountBindingSpec, String> {
    let convoy =
        backend.clone().using::<Convoy>(namespace).get(convoy_name).await.map_err(|error| format!("convoy `{convoy_name}`: {error}"))?;
    let workflow = admitted_workflow(backend, namespace, &convoy)
        .await?
        .ok_or_else(|| format!("convoy `{convoy_name}` has no admitted workflow yet"))?;
    let project = convoy.spec.project_ref.as_deref();
    let alternatives = current_alternatives(backend, namespace, project, &convoy.spec.repositories, &workflow).await?;
    let alternative = alternatives
        .iter()
        .find(|alternative| alternative.requester.role == role)
        .ok_or_else(|| format!("role `{role}` of convoy `{convoy_name}` declares no account requirement"))?;
    let selected = select(alternative, pin.map(|pin| (pin, REBIND_PIN)), random)?;
    let opened = binding_record(project, convoy_name, alternative, selected, now)?;
    let delivered_until = match accounts.requester_binding(namespace, convoy_name, role, &alternative.requester.access).await? {
        Some(current) if current.spec.state.value.is_open() => delivered_anywhere(accounts, namespace, &current.name).await?.then_some(now),
        _ => None,
    };
    let spec = opened.spec.clone();
    accounts.rebind(namespace, opened, delivered_until, now).await?;
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::random::SeededRandom;

    fn alternatives(names: &[&str], excluded: &[&str]) -> RequesterAlternatives {
        RequesterAlternatives {
            requester: Requester {
                vessel: "work".into(),
                role: "coder".into(),
                item: "claude-code".into(),
                access: AccessName("claude:subscription".into()),
            },
            candidates: names
                .iter()
                .map(|name| {
                    (
                        CredentialName((*name).into()),
                        Candidate {
                            account: Some(format!("{name}@example.com")),
                            grants: BTreeMap::from([(GrantName("claude-box-accounts".into()), GrantScope { scope: "account".into() })]),
                            epoch: 1,
                        },
                    )
                })
                .collect(),
            exclusions: excluded.iter().map(|name| (CredentialName((*name).into()), "host `b1` does not hold it".into())).collect(),
        }
    }

    // Behaviour (#3884): a fixed seed repeats its draw, the draw ranges over every usable
    // candidate, and an excluded candidate is never drawn.
    #[test]
    fn a_seeded_draw_is_uniform_over_usable_candidates_only() {
        let set = alternatives(&["box-a", "box-b", "box-c"], &["box-b"]);
        let draw = |seed| select(&set, None, &SeededRandom::new(seed)).expect("draw");
        assert_eq!(draw(7), draw(7));
        let drawn = (0..64).map(|seed| draw(seed).credential.0).collect::<BTreeSet<_>>();
        assert_eq!(drawn, BTreeSet::from(["box-a".to_string(), "box-c".to_string()]));
        let Selected { credential, selection: Selection::Draw { index } } = draw(7) else { panic!("a draw") };
        assert_eq!(set.usable()[index as usize], &credential);
    }

    // Behaviour (#3882): a pin of an authorized candidate the placement cannot use refuses with
    // the exclusion's reason; it never falls back to the draw.
    #[test]
    fn an_excluded_pin_refuses_with_the_exclusion() {
        let set = alternatives(&["box-a", "box-b"], &["box-b"]);
        assert_eq!(
            select(&set, Some(("box-b", CONVOY_START_PIN)), &SeededRandom::new(1)).expect_err("excluded"),
            "account pin `box-b` for role `coder` is refused: host `b1` does not hold it"
        );
    }
}
