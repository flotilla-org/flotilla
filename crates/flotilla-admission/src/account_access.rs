//! Account access at admission (#3907): requirements resolve through grants into authorized
//! alternatives per requester, a registered plan is checked per candidate environment kind, and
//! one credential is selected per requester and stored as its binding.
//!
//! The contract is #3879 (matching), #3881 (plans), #3882 (bindings; selection amended by #3884
//! to a seeded uniform draw) and #3910's owner rulings.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_agent::{
    account_access::{environment_kind, AccountProvisioners},
    adapter::CapabilityTable,
};
use flotilla_protocol::{PlacementDecision, PlacementTargetHost};
use flotilla_resources::{
    account_access::{
        binding_name, requester_address, requester_parts, AccessName, AccountBinding, AccountBindingSpec, AccountProvision, BindingState,
        CredentialName, EpochEntry, GrantName, GrantScope, Selection,
    },
    Convoy, ConvoyRepositorySpec, CredentialGrant, CredentialSpec, CrewSource, Host as ResourceHost, InputMeta, PlacementPolicy,
    PlacementPolicySpec, RandomSource, Repository, RepositoryKey, RepositoryTrust, ResourceError, ResourceObject, WorkflowTemplateSpec,
};
use flotilla_schema::{Records, State, Timestamp};
use flotilla_store::{ResourceBackend, StoreQuery};

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

/// The candidate environment kind a placement policy resolves to, in the vocabulary plans are
/// resolved against.
pub fn placement_environment_kind(spec: &PlacementPolicySpec, host: Option<&ResourceObject<ResourceHost>>) -> &'static str {
    if spec.cloud_sandbox_per_vessel.is_some() {
        environment_kind::CLOUD_SANDBOX
    } else if spec.docker_per_vessel.is_some() {
        environment_kind::DOCKER
    } else if host.is_some_and(is_agentless_ssh_host) {
        environment_kind::AGENTLESS_SSH
    } else {
        environment_kind::HOST_DIRECT
    }
}

/// The reason a candidate environment kind is refused: some requester's access has no plan
/// there (#3881). The kind drops out of placement with this reason.
pub fn plan_refusal(workflow: &WorkflowTemplateSpec, kind: &str) -> Result<Option<String>, String> {
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
    kind: &str,
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

/// A binding chosen at admission, written once its convoy exists.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingBinding {
    pub name: String,
    pub spec: AccountBindingSpec,
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

/// A binding's metadata: labelled with its requester's convoy, so explain and the capability
/// card read a convoy's bindings by index.
pub fn binding_meta(name: &str, spec: &AccountBindingSpec) -> InputMeta {
    let labels = requester_parts(&spec.requester)
        .map(|(convoy, _)| std::collections::BTreeMap::from([(flotilla_resources::CONVOY_LABEL.to_string(), convoy)]))
        .unwrap_or_default();
    InputMeta::builder().name(name.to_string()).labels(labels).build()
}

/// Write chosen bindings. A requester that already holds an open binding keeps it: bindings are
/// sticky and only rebind or an authorization change moves them (#3882).
pub async fn write_bindings(backend: &ResourceBackend, namespace: &str, pending: Vec<PendingBinding>) -> Result<(), String> {
    let bindings = backend.clone().using::<AccountBinding>(namespace);
    for binding in pending {
        match bindings.get(&binding.name).await {
            Ok(existing) if existing.spec.state.value.is_open() => continue,
            Ok(existing) => {
                bindings
                    .update(&binding_meta(&binding.name, &binding.spec), &existing.metadata.resource_version, &binding.spec)
                    .await
                    .map_err(|error| format!("reopen account binding `{}`: {error}", binding.name))?;
            }
            Err(ResourceError::NotFound { .. }) => {
                bindings
                    .create(&binding_meta(&binding.name, &binding.spec), &binding.spec)
                    .await
                    .map_err(|error| format!("write account binding `{}`: {error}", binding.name))?;
            }
            Err(error) => return Err(format!("read account binding `{}`: {error}", binding.name)),
        }
    }
    Ok(())
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
async fn delivered_anywhere(backend: &ResourceBackend, namespace: &str, binding: &str) -> Result<bool, String> {
    let provisions = backend
        .query_declared::<AccountProvision>(
            namespace,
            &StoreQuery {
                fields: BTreeMap::from([("spec.binding.name".to_string(), binding.to_string())]),
                include_replicas: true,
                ..Default::default()
            },
        )
        .await
        .map_err(|error| format!("list provisions of `{binding}`: {error}"))?;
    Ok(provisions.items.iter().any(|provision| provision.object.spec.delivered.is_some()))
}

/// The home pass over this daemon's own bindings (#3882 lifecycle): a requester's teardown
/// closes its binding; an authorization change revokes it or updates its recorded grants; a
/// replacement login for the same account appends an epoch. A convoy admitted before bindings
/// existed gets one, pinned to the credential its snapshot carried when exactly one did.
pub async fn reconcile_home_bindings(
    backend: &ResourceBackend,
    namespace: &str,
    random: &dyn RandomSource,
    now: Timestamp,
) -> Result<Vec<(String, BindingChange)>, String> {
    let bindings = backend.clone().using::<AccountBinding>(namespace);
    let convoys = backend.clone().using::<Convoy>(namespace).list().await.map_err(|error| format!("list convoys: {error}"))?.items;
    let mut changes = Vec::new();
    let mut bound = BTreeSet::new();
    for binding in bindings.list().await.map_err(|error| format!("list account bindings: {error}"))?.items {
        let name = binding.metadata.name.clone();
        bound.insert(name.clone());
        if !binding.spec.state.value.is_open() {
            continue;
        }
        let Some((convoy_name, role)) = requester_parts(&binding.spec.requester) else { continue };
        let convoy = convoys.iter().find(|convoy| convoy.metadata.name == convoy_name);
        let live = convoy.filter(|convoy| !convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()));
        let Some(convoy) = live else {
            let reason = "requester teardown".to_string();
            close(&bindings, &binding, BindingState::Closed { reason: reason.clone(), delivered_until: None }, now).await?;
            changes.push((name, BindingChange::Closed(reason)));
            continue;
        };
        let Some(workflow) = admitted_workflow(backend, namespace, convoy).await? else { continue };
        let alternatives =
            current_alternatives(backend, namespace, convoy.spec.project_ref.as_deref(), &convoy.spec.repositories, &workflow).await?;
        let candidate = alternatives
            .iter()
            .find(|alternative| alternative.requester.role == role && alternative.requester.access == binding.spec.requirement)
            .and_then(|alternative| alternative.candidates.get(&binding.spec.credential));
        let Some(candidate) = candidate else {
            let delivered_until = delivered_anywhere(backend, namespace, &name).await?.then_some(now);
            let state = BindingState::Revoked { reason: NO_AUTHORIZING_GRANT.to_string(), delivered_until };
            close(&bindings, &binding, state, now).await?;
            changes.push((name, BindingChange::Revoked(NO_AUTHORIZING_GRANT.to_string())));
            continue;
        };
        let mut spec = binding.spec.clone();
        let mut change = None;
        if spec.grants != candidate.grants {
            spec.grants = candidate.grants.clone();
            change = Some(BindingChange::GrantsUpdated);
        }
        let current_epoch = spec.epochs.last().and_then(|row| row.record.epoch.parse::<u32>().ok()).unwrap_or(0);
        if candidate.epoch > current_epoch {
            if candidate.account != spec.account {
                let reason = format!(
                    "replacement refused: credential `{}` now declares account `{}`, not the bound `{}`",
                    spec.credential,
                    candidate.account.as_deref().unwrap_or("none"),
                    spec.account.as_deref().unwrap_or("none")
                );
                let delivered_until = delivered_anywhere(backend, namespace, &name).await?.then_some(now);
                close(&bindings, &binding, BindingState::Revoked { reason: reason.clone(), delivered_until }, now).await?;
                changes.push((name, BindingChange::Revoked(reason)));
                continue;
            }
            let next = spec.epochs.last().map_or(1, |row| row.version + 1);
            spec.epochs
                .append(next, EpochEntry { epoch: candidate.epoch.to_string(), observed_at: now })
                .map_err(|error| format!("epoch lineage: {error:?}"))?;
            change = Some(BindingChange::EpochAppended(candidate.epoch.to_string()));
        }
        if let Some(change) = change {
            bindings
                .update(&binding_meta(&name, &spec), &binding.metadata.resource_version, &spec)
                .await
                .map_err(|error| format!("update account binding `{name}`: {error}"))?;
            changes.push((name, change));
        }
    }
    // Convoys admitted before bindings existed.
    for convoy in convoys.iter().filter(|convoy| !convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal())) {
        let Some(workflow) = admitted_workflow(backend, namespace, convoy).await? else { continue };
        let project = convoy.spec.project_ref.as_deref();
        let alternatives = current_alternatives(backend, namespace, project, &convoy.spec.repositories, &workflow).await?;
        let mut pending = Vec::new();
        for alternative in alternatives.iter().filter(|alternative| !alternative.candidates.is_empty()) {
            let requester = &alternative.requester;
            let name = binding_name(&convoy.metadata.name, &requester.role, &requester.access);
            if bound.contains(&name) {
                continue;
            }
            let vessel = workflow.vessels.iter().find(|vessel| vessel.name == requester.vessel);
            let carried = vessel
                .map(|vessel| {
                    vessel.credential_refs.iter().filter(|name| alternative.candidates.contains_key(&CredentialName((*name).clone())))
                })
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            let pin = match carried.as_slice() {
                [single] => Some((single.as_str(), SNAPSHOT_PIN)),
                _ => None,
            };
            let selected = select(alternative, pin, random)?;
            pending.push(binding_record(project, &convoy.metadata.name, alternative, selected, now)?);
            changes.push((name, BindingChange::Opened));
        }
        write_bindings(backend, namespace, pending).await?;
    }
    Ok(changes)
}

async fn close(
    bindings: &flotilla_store::TypedResolver<AccountBinding>,
    binding: &ResourceObject<AccountBinding>,
    state: BindingState,
    now: Timestamp,
) -> Result<(), String> {
    let mut spec = binding.spec.clone();
    spec.state = State::new(state, now);
    bindings
        .update(&binding_meta(&binding.metadata.name, &spec), &binding.metadata.resource_version, &spec)
        .await
        .map(|_| ())
        .map_err(|error| format!("update account binding `{}`: {error}", binding.metadata.name))
}

/// An explicit rebind (#3882 lifecycle): the old binding is closed and kept under a retired
/// name for attribution, and a new one opens by pin or draw under the requester's name. Its
/// places see the new opening, remove the old copies and start a fresh harness session.
pub async fn rebind(
    backend: &ResourceBackend,
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
    let bindings = backend.clone().using::<AccountBinding>(namespace);
    match bindings.get(&opened.name).await {
        Ok(existing) => {
            let delivered_until = delivered_anywhere(backend, namespace, &opened.name).await?.then_some(now);
            let mut retired = existing.spec.clone();
            if retired.state.value.is_open() {
                retired.state = State::new(BindingState::Closed { reason: "rebind".to_string(), delivered_until }, now);
            }
            let retired_name = format!("{}.closed-{}", opened.name, existing.spec.opened_at.micros());
            bindings
                .create(&binding_meta(&retired_name, &retired), &retired)
                .await
                .map_err(|error| format!("retire account binding as `{retired_name}`: {error}"))?;
            bindings
                .update(&binding_meta(&opened.name, &opened.spec), &existing.metadata.resource_version, &opened.spec)
                .await
                .map_err(|error| format!("open account binding `{}`: {error}", opened.name))?;
        }
        Err(ResourceError::NotFound { .. }) => {
            bindings
                .create(&binding_meta(&opened.name, &opened.spec), &opened.spec)
                .await
                .map_err(|error| format!("open account binding `{}`: {error}", opened.name))?;
        }
        Err(error) => return Err(format!("read account binding `{}`: {error}", opened.name)),
    }
    Ok(opened.spec)
}

#[cfg(test)]
mod tests {
    use flotilla_resources::SeededRandom;

    use super::*;

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

    // Behaviour (#3882): an authorized pin wins over the draw; an unauthorized or excluded pin
    // refuses with the reason.
    #[test]
    fn a_pin_wins_only_when_authorized_and_usable() {
        let set = alternatives(&["box-a", "box-b"], &["box-b"]);
        let random = SeededRandom::new(1);
        assert_eq!(
            select(&set, Some(("box-a", CONVOY_START_PIN)), &random).expect("pin"),
            Selected { credential: CredentialName("box-a".into()), selection: Selection::Pin { pinned_by: CONVOY_START_PIN.into() } }
        );
        assert_eq!(
            select(&set, Some(("claude-max", CONVOY_START_PIN)), &random).expect_err("unauthorized"),
            "account pin `claude-max` for role `coder` is refused: no CredentialGrant authorizes it for `claude:subscription`"
        );
        assert_eq!(
            select(&set, Some(("box-b", CONVOY_START_PIN)), &random).expect_err("excluded"),
            "account pin `box-b` for role `coder` is refused: host `b1` does not hold it"
        );
    }

    #[test]
    fn binding_names_and_requester_addresses_are_per_role() {
        assert_eq!(binding_name("c1", "coder", &AccessName("claude:subscription".into())), "c1.coder.claude-subscription");
        assert_eq!(
            requester_address(Some("flotilla"), "c1", "coder").expect("address").to_string(),
            "project:flotilla/convoy:c1/role:coder"
        );
    }
}
