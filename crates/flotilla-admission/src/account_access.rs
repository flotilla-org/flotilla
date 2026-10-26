//! Account access at admission (#3907): requirements resolve through grants into authorized
//! alternatives per requester, a registered plan is checked per candidate environment kind, and
//! one credential is selected per requester and stored as its binding.
//!
//! The contract is #3879 (matching), #3881 (plans), #3882 (bindings; selection amended by #3884
//! to a seeded uniform draw) and #3910's owner rulings.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_address::{Address, Kind as AddressKind};
use flotilla_agent::{
    account_access::{environment_kind, AccountProvisioners},
    adapter::CapabilityTable,
};
use flotilla_protocol::{PlacementDecision, PlacementTargetHost};
use flotilla_resources::{
    account_access::{
        AccessName, AccountBinding, AccountBindingSpec, BindingState, CredentialName, EpochEntry, GrantName, GrantScope, Selection,
    },
    ConvoyRepositorySpec, CredentialGrant, CredentialSpec, CrewSource, Host as ResourceHost, InputMeta, PlacementPolicy,
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
        .items;
    let specs = backend
        .query_declared::<CredentialSpec>(
            namespace,
            &StoreQuery::names(grants.iter().flat_map(|grant| grant.object.spec.credentials.iter().cloned()), true),
        )
        .await
        .map_err(|error| format!("list credential specs: {error}"))?
        .items
        .into_iter()
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
        if candidates.is_empty() {
            return Err(format!(
                "agent adapter `{}` for role `{}` requires `{}`, but no CredentialGrant authorizes a credential that supplies it",
                requester.item, requester.role, requester.access
            ));
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
                    alternative.exclusions.insert(name.clone(), format!("not held by host `{host_label}`"));
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
pub fn select(alternatives: &RequesterAlternatives, pin: Option<&str>, random: &dyn RandomSource) -> Result<Selected, String> {
    let requester = &alternatives.requester;
    if let Some(pin) = pin {
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
        return Ok(Selected { credential, selection: Selection::Pin { pinned_by: CONVOY_START_PIN.to_string() } });
    }
    let usable = alternatives.usable();
    if usable.is_empty() {
        return Err(format!("role `{}` has no usable credential for `{}`", requester.role, requester.access));
    }
    let bound = u32::try_from(usable.len()).map_err(|_| "too many candidates for a draw".to_string())?;
    let index = random.below(bound);
    Ok(Selected { credential: usable[index as usize].clone(), selection: Selection::Draw { index } })
}

/// The binding's resource name: one per (requester, requirement), so a lookup needs no query.
pub fn binding_name(convoy: &str, role: &str, access: &AccessName) -> String {
    format!("{convoy}.{role}.{}", access.0.replace(':', "-"))
}

/// The requester's role address (`project:<p>/convoy:<c>/role:<r>`).
pub fn requester_address(project: Option<&str>, convoy: &str, role: &str) -> Result<Address, String> {
    let convoy = match project {
        Some(project) => Address::project(project).and_then(|address| address.child(AddressKind::Convoy, convoy)),
        None => Address::convoy(convoy),
    };
    convoy.and_then(|address| address.child(AddressKind::Role, role)).map_err(|error| format!("requester address: {error:?}"))
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
        let selected = select(alternative, pins.get(&alternative.requester.role).map(String::as_str), random)?;
        pending.push(binding_record(project_ref, convoy, alternative, selected, now)?);
    }
    Ok(pending)
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
                    .update(&InputMeta::builder().name(binding.name.clone()).build(), &existing.metadata.resource_version, &binding.spec)
                    .await
                    .map_err(|error| format!("reopen account binding `{}`: {error}", binding.name))?;
            }
            Err(ResourceError::NotFound { .. }) => {
                bindings
                    .create(&InputMeta::builder().name(binding.name.clone()).build(), &binding.spec)
                    .await
                    .map_err(|error| format!("write account binding `{}`: {error}", binding.name))?;
            }
            Err(error) => return Err(format!("read account binding `{}`: {error}", binding.name)),
        }
    }
    Ok(())
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
            exclusions: excluded.iter().map(|name| (CredentialName((*name).into()), "not held by host `b1`".into())).collect(),
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
            select(&set, Some("box-a"), &random).expect("pin"),
            Selected { credential: CredentialName("box-a".into()), selection: Selection::Pin { pinned_by: CONVOY_START_PIN.into() } }
        );
        assert_eq!(
            select(&set, Some("claude-max"), &random).expect_err("unauthorized"),
            "account pin `claude-max` for role `coder` is refused: no CredentialGrant authorizes it for `claude:subscription`"
        );
        assert_eq!(
            select(&set, Some("box-b"), &random).expect_err("excluded"),
            "account pin `box-b` for role `coder` is refused: not held by host `b1`"
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
