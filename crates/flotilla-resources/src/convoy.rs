use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use flotilla_protocol::{
    CommandCaller, IssueRef, IssueState, Leaf, LeafAddress, LeafOperator, PlacementDecision, PrincipalRef, Relationship, Subject,
};
pub use flotilla_protocol::{StallProposedDisposition, StallReason, StallRung, TurnDeliveryRung};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    resource::define_resource,
    status_patch::StatusPatch,
    workflow_template::{ExitDeclaration, SubjectVariable, TurnDeliveryRule, VesselRequirement},
    ReadResourceObject, ReplicationClass, RepositoryKey, Resource, ResourceObject, ResourceProvenance, SettlementClaimEvidence,
    ACTUATOR_HOST_REF_ANNOTATION, CONVOY_LABEL,
};

mod reconcile;

pub use reconcile::{
    evaluate_crew_completion, evaluate_landing_settlement, reconcile, ConvoyEvent, ConvoyReconciler, ConvoyTeardownRuntime,
    CrewCompletionClaim, ReconcileOutcome, SettlementEvaluation, SettlementMode, UnmetSettlementExpectation,
};

define_resource!(Convoy, "convoys", ConvoySpec, ConvoyStatus, ConvoyStatusPatch, replication = ReplicationClass::HomeBoundRuntime);

pub const WORKFLOW_SNAPSHOT_ANNOTATION: &str = "flotilla.work/workflow-snapshot";
pub const PLACEMENT_SNAPSHOT_ANNOTATION: &str = "flotilla.work/placement-snapshot";
pub const VESSEL_PLACEMENTS_ANNOTATION: &str = "flotilla.work/vessel-placements";
pub const ENSURED_FROM_ANNOTATION: &str = "flotilla.work/ensured-from";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VesselPlacementPin {
    pub policy_ref: String,
    pub decision: PlacementDecision,
}

pub fn vessel_placement_pin(convoy: &ResourceObject<Convoy>, vessel: &str) -> Option<VesselPlacementPin> {
    let encoded = convoy.metadata.annotations.get(VESSEL_PLACEMENTS_ANNOTATION)?;
    serde_json::from_str::<BTreeMap<String, VesselPlacementPin>>(encoded).ok()?.remove(vessel)
}

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct ConvoySpec {
    pub workflow_ref: String,
    /// Stable human-facing role within `project_ref`.
    #[builder(default)]
    pub role: String,
    /// Monotonic incarnation number within `{project_ref, role}`.
    #[builder(default)]
    pub generation: u64,
    #[builder(default)]
    pub dispatching_principal_ref: PrincipalRef,
    #[builder(default)]
    pub inputs: BTreeMap<String, InputValue>,
    pub placement_policy: Option<String>,
    #[builder(default)]
    pub repositories: Vec<ConvoyRepositorySpec>,
    pub r#ref: Option<String>,
    /// The [`Project`](crate::Project) whose repository set was snapshotted at admission.
    pub project_ref: Option<String>,
    #[builder(default)]
    pub adopted_checkout_refs: BTreeMap<RepositoryKey, String>,
    #[builder(default)]
    pub subjects: Vec<DeclaredSubject>,
    #[builder(default)]
    pub issues: Vec<ConvoyIssue>,
    /// Change request explicitly bound when the convoy was admitted.
    pub change_request: Option<BoundChangeRequest>,
    pub instruction: Option<String>,
}

impl ConvoySpec {
    /// The repository snapshot when this convoy has exactly one repository.
    ///
    /// A single repository is the only unambiguous source for one `vcs.repo`
    /// grouping fact.
    pub fn sole_repository(&self) -> Option<&ConvoyRepositorySpec> {
        let [repository] = self.repositories.as_slice() else {
            return None;
        };
        Some(repository)
    }
}

/// Build subject parsing and presentation context from the convoy's admitted
/// repository snapshot. Resolved Repository identity wins over URL inference;
/// project aliases and Forge web roots are shared by every consumer.
pub fn convoy_reference_context<'a>(
    repositories: &[ConvoyRepositorySpec],
    project_ref: Option<&str>,
    project: Option<&crate::ProjectSpec>,
    forges: &[crate::ForgeSpec],
    repository_spec: impl Fn(&RepositoryKey) -> Option<&'a crate::RepositorySpec>,
) -> flotilla_protocol::ReferenceContext {
    let repositories = repositories
        .iter()
        .filter_map(|repository| {
            let resolved = repository_spec(&repository.repo_ref);
            let (service, scope) = match resolved.map(crate::RepositorySpec::identity) {
                Some(crate::RepositoryIdentity::Forge { forge_ref, owner, repo_name }) => {
                    (forge_ref.clone(), format!("{owner}/{repo_name}"))
                }
                _ => {
                    let LeafAddress::ChangeRequest { service, scope, .. } =
                        change_request_address_with_forges(&repository.url, "1", forges).ok()?
                    else {
                        return None;
                    };
                    (service, scope)
                }
            };
            let canonical = crate::canonicalize_repo_url(&repository.url).ok()?;
            let web_base = forges
                .iter()
                .find(|forge| forge.forge_id == service)
                .map(|forge| forge.https_url.trim_end_matches('/').to_string())
                .or_else(|| {
                    resolved.and_then(crate::RepositorySpec::forge).map(|forge| forge.service_url.trim_end_matches('/').to_string())
                })
                .or_else(|| canonical.strip_suffix(&format!("/{scope}")).map(str::to_string))?;
            let alias = project
                .and_then(|project| project.repositories.iter().find(|member| member.repo == repository.repo_ref))
                .and_then(|member| member.alias.clone())
                .unwrap_or_else(|| scope.rsplit('/').next().expect("rsplit always yields an item").to_string());
            Some(flotilla_protocol::RepositoryAlias {
                project: project_ref.map(str::to_string),
                alias,
                source: flotilla_protocol::IssueSource { service: service.clone(), scope },
                web_base,
                forge_alias: (service != "github.com").then_some(service),
            })
        })
        .collect();
    flotilla_protocol::ReferenceContext { repositories }
}

/// The presentation and settlement consumers read the same persisted links.
pub fn convoy_subject_rows(
    convoy: &ResourceObject<Convoy>,
    context: &flotilla_protocol::ReferenceContext,
) -> Vec<flotilla_protocol::result_set::ConvoySubjectRow> {
    let mut rows = Vec::new();
    for entry in convoy.spec.declared_subjects().unwrap_or_default() {
        rows.push(flotilla_protocol::result_set::ConvoySubjectRow {
            url: entry.subject.url(context),
            short: entry.subject.short(context),
            repository_key: None,
            subject: entry.subject,
            relationship: entry.relationship,
            declared: true,
        });
    }
    for entry in convoy.status.iter().flat_map(|status| &status.subjects) {
        if !rows.iter().any(|row| row.subject == entry.subject && row.relationship == entry.relationship) {
            rows.push(flotilla_protocol::result_set::ConvoySubjectRow {
                url: entry.subject.url(context),
                short: entry.subject.short(context),
                repository_key: None,
                subject: entry.subject.clone(),
                relationship: entry.relationship,
                declared: false,
            });
        }
    }
    rows
}

/// Different production histories for the same request need an operator's
/// judgement. Distinct requests, including two in one repository, do not.
pub fn subject_relationship_conflicts(convoy: &ResourceObject<Convoy>) -> Vec<Subject> {
    let mut relationships = BTreeMap::<Subject, BTreeSet<Relationship>>::new();
    let declared = convoy.spec.declared_subjects().unwrap_or_default();
    let adopted = declared
        .iter()
        .filter(|entry| entry.relationship == Relationship::Adopts)
        .map(|entry| entry.subject.clone())
        .collect::<BTreeSet<_>>();
    for entry in declared {
        relationships.entry(entry.subject).or_default().insert(entry.relationship);
    }
    for entry in convoy.status.iter().flat_map(|status| &status.subjects) {
        // Working on a declared adopted request also produces work on it,
        // regardless of how that work was discovered (including crew claims).
        if entry.relationship == Relationship::Produces && adopted.contains(&entry.subject) {
            continue;
        }
        relationships.entry(entry.subject.clone()).or_default().insert(entry.relationship);
    }
    relationships
        .into_iter()
        .filter_map(|(subject, relationships)| {
            (subject.kind == flotilla_protocol::SubjectKind::ChangeRequest
                && relationships.contains(&Relationship::Produces)
                && relationships.contains(&Relationship::Adopts))
            .then_some(subject)
        })
        .collect()
}

/// One-generation decoder for the pre-subject `issues` and `change_request`
/// fields. Remove those two read-only fields after the next fleet roll.
#[derive(Serialize, Deserialize)]
struct ConvoySpecRecord {
    workflow_ref: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    generation: u64,
    #[serde(default)]
    dispatching_principal_ref: PrincipalRef,
    #[serde(default)]
    inputs: BTreeMap<String, InputValue>,
    #[serde(default)]
    placement_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    repositories: Vec<ConvoyRepositorySpec>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    r#ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_ref: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    adopted_checkout_refs: BTreeMap<RepositoryKey, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    subjects: Vec<DeclaredSubject>,
    #[serde(default, skip_serializing)]
    issues: Vec<ConvoyIssue>,
    #[serde(default, skip_serializing)]
    change_request: Option<BoundChangeRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instruction: Option<String>,
}

impl ConvoySpec {
    pub fn declared_subjects(&self) -> Result<Vec<DeclaredSubject>, String> {
        let mut subjects = self.subjects.clone();
        for issue in &self.issues {
            let mut canonical = issue.reference.clone();
            canonical.id = "1".to_string();
            let LeafAddress::Issue { service, scope, .. } = issue_address(&canonical)? else {
                unreachable!("issue_address always returns an issue address")
            };
            let subject = Subject {
                kind: flotilla_protocol::SubjectKind::Issue,
                source: flotilla_protocol::IssueSource { service, scope },
                id: issue.reference.id.clone(),
            };
            if !subjects.iter().any(|entry| entry.subject == subject && entry.relationship == Relationship::WorksOn) {
                subjects.push(DeclaredSubject {
                    subject,
                    relationship: Relationship::WorksOn,
                    issue: Some(issue.clone()),
                    change_request: None,
                });
            }
        }
        if let Some(bound) = &self.change_request {
            let repository = self
                .repositories
                .iter()
                .find(|repo| repo.repo_ref == bound.repository_ref)
                .ok_or_else(|| format!("bound change request repository {} is absent from convoy", bound.repository_ref))?;
            let subject =
                Subject::from_leaf(&change_request_address(&repository.url, &bound.id)?).expect("change request address is a subject");
            if !subjects.iter().any(|entry| entry.subject == subject && entry.relationship == Relationship::Adopts) {
                subjects.push(DeclaredSubject {
                    subject,
                    relationship: Relationship::Adopts,
                    issue: None,
                    change_request: Some(bound.clone()),
                });
            }
        }
        Ok(subjects)
    }
}

impl Serialize for ConvoySpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error as _;
        ConvoySpecRecord {
            workflow_ref: self.workflow_ref.clone(),
            role: self.role.clone(),
            generation: self.generation,
            dispatching_principal_ref: self.dispatching_principal_ref.clone(),
            inputs: self.inputs.clone(),
            placement_policy: self.placement_policy.clone(),
            repositories: self.repositories.clone(),
            r#ref: self.r#ref.clone(),
            project_ref: self.project_ref.clone(),
            adopted_checkout_refs: self.adopted_checkout_refs.clone(),
            subjects: self.declared_subjects().map_err(S::Error::custom)?,
            issues: Vec::new(),
            change_request: None,
            instruction: self.instruction.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ConvoySpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let record = ConvoySpecRecord::deserialize(deserializer)?;
        let mut spec = Self {
            workflow_ref: record.workflow_ref,
            role: record.role,
            generation: record.generation,
            dispatching_principal_ref: record.dispatching_principal_ref,
            inputs: record.inputs,
            placement_policy: record.placement_policy,
            repositories: record.repositories,
            r#ref: record.r#ref,
            project_ref: record.project_ref,
            adopted_checkout_refs: record.adopted_checkout_refs,
            subjects: record.subjects,
            issues: record.issues,
            change_request: record.change_request,
            instruction: record.instruction,
        };
        for issue in spec.subjects.iter().filter_map(|entry| entry.issue.as_ref()) {
            if !spec.issues.iter().any(|existing| existing.reference == issue.reference) {
                spec.issues.push(issue.clone());
            }
        }
        if spec.change_request.is_none() {
            spec.change_request = spec.subjects.iter().find_map(|entry| entry.change_request.clone());
        }
        if spec.subjects.is_empty() && (!spec.issues.is_empty() || spec.change_request.is_some()) {
            spec.subjects = spec.declared_subjects().map_err(D::Error::custom)?;
        }
        Ok(spec)
    }
}

/// Checkout object names durably recorded as provisioned for this convoy.
///
/// Placements are copied onto work state before vessels can disappear, so the
/// expected set remains available while federated checkout observations lag.
/// Adopted refs are also declarations on the convoy itself and therefore count
/// as expected even before a placement has repeated them.
pub fn expected_checkout_refs(convoy: &crate::ResourceObject<Convoy>) -> Result<BTreeSet<String>, String> {
    let mut expected = convoy.spec.adopted_checkout_refs.values().cloned().collect::<BTreeSet<_>>();
    let Some(status) = &convoy.status else {
        return Ok(expected);
    };
    for (work_name, work) in &status.work {
        let Some(checkout_refs) = work.placement.as_ref().and_then(|placement| placement.fields.get("checkout_refs")) else {
            continue;
        };
        let checkout_refs = serde_json::from_value::<BTreeMap<RepositoryKey, String>>(checkout_refs.clone()).map_err(|error| {
            format!("convoy {}/{} has invalid checkout_refs in work {work_name}: {error}", convoy.metadata.namespace, convoy.metadata.name)
        })?;
        expected.extend(checkout_refs.into_values());
    }
    Ok(expected)
}

/// Change requests already known through the convoy's expected checkouts.
/// This is a home-side fact derived from local or replicated checkout evidence.
pub fn observed_change_request_subjects(
    convoy: &crate::ResourceObject<Convoy>,
    checkouts: &BTreeMap<String, crate::ResourceObject<crate::Checkout>>,
    forges: &[crate::ForgeSpec],
) -> Result<Vec<Subject>, String> {
    let expected = expected_checkout_refs(convoy)?;
    let adopted = convoy
        .spec
        .declared_subjects()?
        .into_iter()
        .filter(|entry| entry.relationship == Relationship::Adopts)
        .map(|entry| entry.subject)
        .collect::<BTreeSet<_>>();
    let mut subjects = Vec::new();
    for name in expected {
        let Some(checkout) = checkouts.get(&name) else { continue };
        if convoy.spec.r#ref.as_deref() != Some(checkout.spec.branch()) {
            continue;
        }
        let Some(observed) = checkout.status.as_ref().and_then(|status| status.integration.change_request.as_ref()) else {
            continue;
        };
        let repository = convoy
            .spec
            .repositories
            .iter()
            .find(|repository| repository.repo_ref == *checkout.spec.repo_ref())
            .ok_or_else(|| format!("checkout {name} repository {} is absent from convoy", checkout.spec.repo_ref()))?;
        let address = change_request_address_with_forges(&repository.url, &observed.id, forges)?;
        if let Some(subject) = Subject::from_leaf(&address) {
            if !adopted.contains(&subject) && !subjects.contains(&subject) {
                subjects.push(subject);
            }
        }
    }
    Ok(subjects)
}

/// The one sanction for collecting a convoy's managed checkouts.
///
/// Two controllers can remove a checkout: the checkout authority's
/// `OwnerTerminal` cascade and the convoy's own reclaim. Both must derive
/// their authority from this single predicate — the convoy is being deleted,
/// or has reached a phase whose reclaim the substrate sanctions (`Landed`:
/// the world terminal fired; `Abandoned`: an explicit operator override).
/// The teardown gate consumes the same predicate from the other side: an
/// expected checkout that is absent while this sanction holds is evidence of
/// completed reclaim, never missing integration evidence — otherwise the
/// checkout authority's cascade would destroy the only evidence the gate
/// accepts and wedge vessel reclaim forever.
pub fn convoy_sanctions_checkout_reclaim(convoy: &crate::ResourceObject<Convoy>) -> bool {
    convoy.metadata.deletion_timestamp.is_some()
        || convoy.status.as_ref().is_some_and(|status| matches!(status.phase, ConvoyPhase::Landed | ConvoyPhase::Abandoned))
}

/// Explicit operator force on a convoy deletion. Checkout authorities use
/// this durable metadata after replication, including while finalizers run.
pub const FORCE_TEARDOWN_ANNOTATION: &str = "flotilla.work/force-teardown";
pub const CONVOY_TEARDOWN_FINALIZER: &str = "flotilla.work/convoy-teardown";

/// Canonical resource name for a convoy's explicitly bound change request.
pub fn bound_change_request_record_name(convoy: &crate::ResourceObject<Convoy>) -> Result<Option<String>, String> {
    let Some(bound) = &convoy.spec.change_request else { return Ok(None) };
    let repository = convoy
        .spec
        .repositories
        .iter()
        .find(|repository| repository.repo_ref == bound.repository_ref)
        .ok_or_else(|| format!("bound change request repository {} is absent from convoy", bound.repository_ref))?;
    let LeafAddress::ChangeRequest { service, scope, number } = change_request_address(&repository.url, &bound.id)? else {
        unreachable!("change_request_address always returns a change-request address")
    };
    Ok(Some(crate::change_request_record_name(&service, &scope, number)))
}

/// Select the authoritative child observation for each object name.
///
/// The actuator host wins when placement identifies one, followed by a local
/// object and then any replica. Keeping this selection shared ensures lifecycle
/// decisions and their derived wake subscriptions consume identical evidence.
pub fn select_convoy_children<T: Resource + Clone>(
    convoy: &ResourceObject<Convoy>,
    sources: &[ReadResourceObject<T>],
) -> BTreeMap<String, ResourceObject<T>> {
    let target_host_ref = convoy
        .status
        .as_ref()
        .and_then(|status| status.placement_decision.as_ref())
        .map(|decision| decision.target_host.reference.as_str());
    let mut selected = BTreeMap::<String, (u8, ResourceObject<T>)>::new();
    for source in sources {
        if source.object.metadata.labels.get(CONVOY_LABEL) != Some(&convoy.metadata.name) {
            continue;
        }
        let actuator_matches = target_host_ref
            .is_some_and(|target| source.object.metadata.annotations.get(ACTUATOR_HOST_REF_ANNOTATION).is_some_and(|host| host == target));
        let priority = if actuator_matches {
            2
        } else if matches!(source.provenance, ResourceProvenance::Local) {
            1
        } else {
            0
        };
        let name = source.object.metadata.name.clone();
        if selected.get(&name).is_none_or(|(current_priority, _)| priority > *current_priority) {
            selected.insert(name, (priority, source.object.clone()));
        }
    }
    selected.into_iter().map(|(name, (_, object))| (name, object)).collect()
}

/// Hardwired world-terminal leaves armed while a convoy is Landing.
///
/// The convoy's declared and discovered subject set is the sole source of
/// change-request settlement obligations.
pub fn active_change_request_subjects(convoy: &ResourceObject<Convoy>) -> Result<Vec<Subject>, String> {
    let mut subjects = Vec::new();
    let declared = convoy.spec.declared_subjects()?;
    let discovered = convoy.status.iter().flat_map(|status| &status.subjects);
    let superseded = declared
        .iter()
        .filter(|entry| entry.relationship == Relationship::Supersedes)
        .map(|entry| entry.subject.clone())
        .chain(discovered.clone().filter(|entry| entry.relationship == Relationship::Supersedes).map(|entry| entry.subject.clone()))
        .collect::<BTreeSet<_>>();
    for (subject, relationship) in declared
        .iter()
        .map(|entry| (&entry.subject, entry.relationship))
        .chain(discovered.map(|entry| (&entry.subject, entry.relationship)))
    {
        if subject.kind != flotilla_protocol::SubjectKind::ChangeRequest
            || !matches!(relationship, Relationship::Produces | Relationship::Adopts)
            || superseded.contains(subject)
        {
            continue;
        }
        if !subjects.contains(subject) {
            subjects.push(subject.clone());
        }
    }
    Ok(subjects)
}

pub fn expected_change_request_leaves(
    convoy: &crate::ResourceObject<Convoy>,
    _checkouts: &BTreeMap<String, crate::ResourceObject<crate::Checkout>>,
) -> Result<Vec<Leaf>, String> {
    Ok(active_change_request_subjects(convoy)?
        .into_iter()
        .map(|subject| subject.leaf())
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flat_map(|address| {
            ["merged", "closed"].map(|literal| Leaf {
                address: address.clone(),
                field_path: ".state".to_string(),
                operator: LeafOperator::Equal,
                literal: literal.to_string(),
            })
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstantiatedExitEntry {
    pub disposition: String,
    pub template: crate::LeafTemplate,
    pub leaves: Vec<Leaf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstantiatedExit {
    None,
    Claim,
    Table(Vec<InstantiatedExitEntry>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstantiatedTurnDelivery {
    pub source: String,
    pub leaf: Leaf,
    pub rule: TurnDeliveryRule,
}

pub fn instantiate_turn_delivery(
    convoy: &crate::ResourceObject<Convoy>,
    checkouts: &BTreeMap<String, crate::ResourceObject<crate::Checkout>>,
    observed_change_requests: &BTreeMap<String, crate::ResourceObject<crate::ChangeRequest>>,
    forges: &[crate::ForgeSpec],
) -> Result<Vec<InstantiatedTurnDelivery>, String> {
    let Some(snapshot) = convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()) else {
        return Ok(Vec::new());
    };
    let change_requests = bound_change_request_addresses(convoy, checkouts)?;
    let issues = if snapshot.turn_delivery.values().any(|rule| rule.on.subject == SubjectVariable::Issue) {
        convoy.spec.issues.iter().map(|issue| issue_address_with_forges(&issue.reference, forges)).collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    Ok(snapshot
        .turn_delivery
        .iter()
        .flat_map(|(source, rule)| {
            let subjects = match &rule.on.subject {
                SubjectVariable::ChangeRequest => change_requests.clone(),
                SubjectVariable::Issue => issues.clone(),
                SubjectVariable::Artifact { producer, kind, about } => match about {
                    crate::ArtifactSubjectBinding::Convoy => vec![LeafAddress::Artifact {
                        convoy: convoy.metadata.name.clone(),
                        producer: producer.clone(),
                        kind: kind.clone(),
                        subject: convoy.metadata.name.clone(),
                    }],
                    crate::ArtifactSubjectBinding::ChangeRequestHead => change_requests
                        .iter()
                        .filter_map(|address| {
                            let LeafAddress::ChangeRequest { service, scope, number } = address else { return None };
                            let name = crate::change_request_record_name(service, scope, *number);
                            let subject = observed_change_requests.get(&name)?.status.as_ref()?.head_sha.value.clone()?;
                            Some(LeafAddress::Artifact {
                                convoy: convoy.metadata.name.clone(),
                                producer: producer.clone(),
                                kind: kind.clone(),
                                subject,
                            })
                        })
                        .collect(),
                },
            };
            subjects.into_iter().map(move |address| InstantiatedTurnDelivery {
                source: source.clone(),
                leaf: Leaf {
                    address,
                    field_path: rule.on.field_path.clone(),
                    operator: rule.on.operator,
                    literal: rule.on.literal.clone(),
                },
                rule: rule.clone(),
            })
        })
        .collect())
}

pub fn issue_address(reference: &IssueRef) -> Result<LeafAddress, String> {
    issue_address_with_forges(reference, &[])
}

/// The relay service is a declared forge name when one owns the issue source.
/// Without a declaration, encode the source path in the service component so
/// installations sharing a host cannot claim the same subject.
pub fn issue_address_with_forges(reference: &IssueRef, forges: &[crate::ForgeSpec]) -> Result<LeafAddress, String> {
    let source = crate::normalize_issue_source(&reference.source);
    let service_url = source.service.trim_end_matches('/');
    let matches = forges.iter().filter(|forge| forge.owns_issue_service(service_url)).collect::<Vec<_>>();
    let (service, scope) = match matches.as_slice() {
        [forge] => {
            let scope = if forge.kind == crate::ForgeKind::Github { source.scope.to_ascii_lowercase() } else { source.scope.clone() };
            (forge.forge_id.clone(), scope)
        }
        [] => {
            let (scheme, location) = service_url.split_once("://").map_or(("https", service_url), |(scheme, rest)| (scheme, rest));
            let location = location.to_ascii_lowercase().replace('%', "%25").replace('/', "%2f");
            let service = if scheme != "https" {
                format!("{scheme}%3a%2f%2f{location}")
            } else if !location.contains(['.', ':']) && !location.contains("%2f") {
                format!("host%3a{location}")
            } else {
                location
            };
            (service, source.scope.clone())
        }
        _ => return Err(format!("issue source {service_url} matches multiple Forge definitions")),
    };
    let (service, scope) = flotilla_relay_protocol::Subject::normalize_scope(&service, &scope);
    let number = reference.id.parse::<u64>().map_err(|_| format!("issue id `{}` is not a numeric forge number", reference.id))?;
    Ok(LeafAddress::Issue { service, scope, number })
}

/// Instantiate the convoy's pinned exit declaration over every bound subject.
///
/// A table entry is an implicit universal over its subject role. With no bound
/// subjects, a declared table collapses to the same claim exit as `exit: claim`.
/// An absent declaration stays absent, leaving the convoy standing until an
/// operator explicitly reaps it.
pub fn instantiate_exit(
    convoy: &crate::ResourceObject<Convoy>,
    checkouts: &BTreeMap<String, crate::ResourceObject<crate::Checkout>>,
) -> Result<InstantiatedExit, String> {
    let Some(declaration) =
        convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).and_then(|snapshot| snapshot.exit.clone())
    else {
        return Ok(InstantiatedExit::None);
    };
    if matches!(declaration, ExitDeclaration::Claim(_)) {
        return Ok(InstantiatedExit::Claim);
    }

    let subjects = bound_change_request_addresses(convoy, checkouts)?;
    if subjects.is_empty() && expected_checkout_refs(convoy)?.is_empty() {
        return Ok(InstantiatedExit::Claim);
    }
    let ExitDeclaration::Table(entries) = declaration else {
        unreachable!("claim declaration returned above");
    };
    if entries.iter().any(|(_, template)| template.subject == SubjectVariable::Issue) {
        return Err("issue exit leaves are not admitted".to_string());
    }
    Ok(InstantiatedExit::Table(
        entries
            .into_iter()
            .map(|(disposition, template)| {
                let leaves = subjects
                    .iter()
                    .map(|address| Leaf {
                        address: address.clone(),
                        field_path: template.field_path.clone(),
                        operator: template.operator,
                        literal: template.literal.clone(),
                    })
                    .collect();
                InstantiatedExitEntry { disposition, template, leaves }
            })
            .collect(),
    ))
}

fn bound_change_request_addresses(
    convoy: &crate::ResourceObject<Convoy>,
    checkouts: &BTreeMap<String, crate::ResourceObject<crate::Checkout>>,
) -> Result<Vec<LeafAddress>, String> {
    let leaves = expected_change_request_leaves(convoy, checkouts)?;
    Ok(leaves.into_iter().map(|leaf| leaf.address).fold(Vec::new(), |mut addresses, address| {
        if !addresses.contains(&address) {
            addresses.push(address);
        }
        addresses
    }))
}

pub fn change_request_address(repository_url: &str, id: &str) -> Result<LeafAddress, String> {
    let canonical = crate::canonicalize_repo_url(repository_url)?;
    let without_scheme = canonical
        .split_once("://")
        .map(|(_, value)| value)
        .ok_or_else(|| format!("canonical repository remote has no scheme: {canonical}"))?;
    let (service, scope) =
        without_scheme.split_once('/').ok_or_else(|| format!("canonical repository remote has no repository path: {canonical}"))?;
    let number = id.parse::<u64>().map_err(|_| format!("change request id `{id}` is not a numeric forge number"))?;
    Ok(LeafAddress::ChangeRequest { service: service.to_string(), scope: scope.to_string(), number })
}

pub fn change_request_address_with_forges(repository_url: &str, id: &str, forges: &[crate::ForgeSpec]) -> Result<LeafAddress, String> {
    let mut matches = Vec::new();
    for forge in forges {
        if let Some(path) = forge.repository_path(repository_url)? {
            matches.push((forge, path));
        }
    }
    match matches.as_slice() {
        [(forge, (owner, repo))] => {
            let number = id.parse::<u64>().map_err(|_| format!("change request id `{id}` is not a numeric forge number"))?;
            Ok(LeafAddress::ChangeRequest { service: forge.forge_id.clone(), scope: format!("{owner}/{repo}"), number })
        }
        [] => change_request_address(repository_url, id),
        _ => Err(format!("repository {repository_url} matches multiple Forge definitions")),
    }
}

pub fn pinned_workflow_ref(convoy: &crate::ResourceObject<Convoy>) -> &str {
    convoy.metadata.annotations.get(WORKFLOW_SNAPSHOT_ANNOTATION).map(String::as_str).unwrap_or(&convoy.spec.workflow_ref)
}

pub fn pinned_placement_ref(convoy: &crate::ResourceObject<Convoy>) -> Option<&str> {
    convoy.metadata.annotations.get(PLACEMENT_SNAPSHOT_ANNOTATION).map(String::as_str).or(convoy.spec.placement_policy.as_deref())
}

/// Durable source-qualified issue context captured when a convoy is admitted.
/// The reference remains stable if the Project later changes its Issue Source;
/// the snapshot records exactly what the crew was asked to act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoyIssue {
    pub reference: IssueRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_ref: Option<RepositoryKey>,
    pub snapshot: IssueSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredSubject {
    pub subject: Subject,
    pub relationship: Relationship,
    /// Admission snapshot retained for carried-issue briefs and older readers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<ConvoyIssue>,
    /// Admission title and repository binding retained for adopted PR briefs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_request: Option<BoundChangeRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectDiscoverySource {
    Branch,
    Claim,
    Relay,
    Operator,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectDiscovery {
    pub source: SubjectDiscoverySource,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredSubject {
    pub subject: Subject,
    pub relationship: Relationship,
    pub sources: Vec<SubjectDiscovery>,
}

impl ConvoyStatus {
    pub fn produces(&self, subject: &Subject) -> bool {
        self.subjects.iter().any(|entry| &entry.subject == subject && entry.relationship == Relationship::Produces)
    }

    pub fn discover_subject(&mut self, subject: Subject, relationship: Relationship, source: SubjectDiscoverySource, at: DateTime<Utc>) {
        if self.unlinked_subjects.contains(&subject) && source != SubjectDiscoverySource::Operator {
            return;
        }
        if source == SubjectDiscoverySource::Operator {
            self.unlinked_subjects.retain(|unlinked| unlinked != &subject);
        }
        if subject.kind == flotilla_protocol::SubjectKind::ChangeRequest && relationship == Relationship::Produces {
            self.branch_subject_scan_at = Some(at);
            self.branch_subject_scan_error = None;
        }
        if let Some(existing) = self.subjects.iter_mut().find(|entry| entry.subject == subject && entry.relationship == relationship) {
            if let Some(evidence) = existing.sources.iter_mut().find(|evidence| evidence.source == source) {
                evidence.at = at;
            } else {
                existing.sources.push(SubjectDiscovery { source, at });
            }
        } else {
            self.subjects.push(DiscoveredSubject { subject, relationship, sources: vec![SubjectDiscovery { source, at }] });
        }
    }

    pub fn unlink_subject(&mut self, subject: &Subject) {
        self.subjects.retain(|entry| &entry.subject != subject);
        if !self.unlinked_subjects.contains(subject) {
            self.unlinked_subjects.push(subject.clone());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueSnapshot {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub state: IssueState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    pub as_of: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct BoundChangeRequest {
    pub id: String,
    pub repository_ref: RepositoryKey,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ConvoyRepositorySpec {
    pub url: String,
    pub repo_ref: RepositoryKey,
    pub source_ref: String,
    pub target_ref: String,
    pub workspace_slug: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subpaths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputValue {
    // Keep inputs untagged so today's plain strings serialize naturally while leaving room
    // for future structured input sources without changing the field shape.
    String(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ConvoyStatus {
    /// Per-vessel runtime evidence survives backing environment teardown.
    /// ADR 0047: keep this decoder default for one roll.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment_observations: BTreeMap<String, flotilla_protocol::EnvironmentRuntimeObservation>,
    /// Frozen ensure configuration that admitted this generation. Previous
    /// generations have no baseline; an explicit roll establishes one.
    /// Future ConvoyEnsureSpec changes must also decode this embedded stored
    /// snapshot for one generation (ADR 0047), even after the ensure is deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ensure_admission: Option<crate::ConvoyEnsureSpec>,
    pub phase: ConvoyPhase,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subjects: Vec<DiscoveredSubject>,
    /// Discovery completed by a successful search of every admitted repository
    /// or by recording a produced change request from any source. The default
    /// can be removed one fleet roll after this field first ships, per ADR 0047.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_subject_scan_at: Option<DateTime<Utc>>,
    /// Latest branch discovery failure; diagnostic once a subject is known.
    /// The default is a one-roll decoder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_subject_scan_error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unlinked_subjects: Vec<Subject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled: Option<StalledCondition>,
    /// Actor obligation budgets outlive transient attention and visible stalls.
    /// Remove the decoder default one fleet roll after this field lands (ADR 0047).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nudge_obligations: Vec<NudgeObligation>,
    /// Durable evidence of whether this convoy reached provisioning. An absent
    /// value means the evidence predates this field and must be treated as
    /// unknown rather than as `NotStarted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisioning: Option<ConvoyProvisioningState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_decision: Option<PlacementDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_snapshot: Option<WorkflowSnapshot>,
    /// Work aboard each declared vessel, keyed by vessel (requirement) name.
    /// Agent-backed work rolls up from `crew_work`; tool-only work is completed
    /// explicitly by an operator.
    #[serde(default)]
    pub work: BTreeMap<String, WorkState>,
    /// Workflow state for each declared agent crew member, keyed first by
    /// vessel name and then by its unique role. Tool processes are excluded.
    #[serde(default)]
    pub crew_work: BTreeMap<String, BTreeMap<String, CrewWorkState>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_workflow_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_workflows: Option<BTreeMap<String, String>>,
    /// Observed change requests that landed somewhere other than their
    /// repository's declared target. These facts are advisory: the observed
    /// landing still wins and the convoy becomes Landed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_mismatches: Vec<TargetMismatch>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub turn_deliveries: BTreeMap<String, TurnDeliveryStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<ConvoyAttention>,
    /// Mutating lifecycle requests retained for operator explanation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lifecycle_mutations: Vec<LifecycleMutation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeafMaker {
    Observed {
        refresher: String,
        external_party: String,
    },
    Actor {
        vessel: String,
        role: String,
    },
    Supervisor {
        convoy: String,
        vessel: String,
        role: String,
    },
    /// An absent name identifies the resource carrying this condition.
    Controller {
        resource_kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        retry: crate::ControllerRetry,
        #[serde(default)]
        ceiling: crate::RetryCeiling,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallEvidenceSource {
    Screen,
    Hook,
    Observation,
    Session,
    LeafEngine,
    Crew,
}

/// Machine-readable cause for controller-owned waits. Previous-generation
/// stored conditions omit it and remain decodable (ADR 0047).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StallCause {
    Capacity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StallSupervisor {
    pub convoy: String,
    pub vessel: String,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StallNudge {
    pub at: DateTime<Utc>,
    pub row: Leaf,
}

/// Durable supervision accounting for one actor row and its unmet leaves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct NudgeObligation {
    pub maker: LeafMaker,
    pub leaves: Vec<Leaf>,
    #[builder(default)]
    pub history: Vec<StallNudge>,
    #[builder(default)]
    pub progress: BTreeMap<String, String>,
    pub quiet_since: Option<DateTime<Utc>>,
    pub last_hook_at: Option<DateTime<Utc>>,
    pub last_attention_at: Option<DateTime<Utc>>,
    /// Beginning of a continuously observed turn; screen redraws are not tool progress.
    /// Remove the decoder default one fleet roll after this field lands (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_since: Option<DateTime<Utc>>,
    // Stored-data compatibility: defaults may be removed one fleet roll later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_after: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool_activity_at: Option<DateTime<Utc>>,
    pub message_id: Option<String>,
    pub delivered_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StalledCondition {
    pub leaves: Vec<Leaf>,
    pub maker: Option<LeafMaker>,
    pub evidence: String,
    pub source: StallEvidenceSource,
    /// Remove this decoder default one fleet roll after the field is written everywhere (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<StallCause>,
    pub began_at: DateTime<Utc>,
    pub rung: StallRung,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<StallSupervisor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervision_index: Option<usize>,
    #[serde(default)]
    pub supervision_exhausted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<StallReason>,
    /// Remove this decoder default one fleet roll after the field is written everywhere (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_disposition: Option<StallProposedDisposition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nudge_history: Vec<StallNudge>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleMutation {
    pub action: String,
    pub caller: CommandCaller,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CrewCompletionOverride {
    pub principal: PrincipalRef,
    pub forced_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum ConvoyProvisioningState {
    NotStarted,
    Started { started_at: DateTime<Utc> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PendingBrief {
    pub vessel: String,
    pub role: String,
    pub content: String,
    pub queued_at: DateTime<Utc>,
    /// Decodes briefs queued before sender attribution; remove after the next fleet roll.
    #[serde(default)]
    #[builder(default)]
    pub sender: crate::CrewMessageSender,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct TargetMismatch {
    pub repo_ref: RepositoryKey,
    pub change_request_id: String,
    pub declared_target_ref: String,
    pub observed_target_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitDeclaration>,
    #[serde(default, skip_serializing_if = "indexmap::IndexMap::is_empty")]
    pub turn_delivery: indexmap::IndexMap<String, TurnDeliveryRule>,
    #[serde(default, skip_serializing_if = "indexmap::IndexMap::is_empty")]
    pub stall_nudges: indexmap::IndexMap<String, crate::StallNudgePolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervision: Option<Vec<crate::SupervisionTarget>>,
    pub vessels: Vec<VesselRequirement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TurnDeliveryStatus {
    /// Remove this decoder default one fleet roll after delivery failures are stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TurnDeliveryFailure>,
    #[serde(default)]
    pub episodes: Vec<TurnDeliveryEpisode>,
    /// The operator brief waiting for its target crew member's turn boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_brief: Option<PendingBrief>,
    /// The placement host consumes this from the replicated convoy record.
    /// Decodes turn delivery statuses stored before remote supervision; remove
    /// the compatibility default one fleet roll after this field is deployed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_supervisor_turn: Option<PendingSupervisorTurn>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TurnDeliveryFailureKind {
    Permanent,
    Transient,
}

/// A parked permanent failure or a transient retry with a durable deadline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct TurnDeliveryFailure {
    pub reason: String,
    pub failed_at: DateTime<Utc>,
    pub kind: TurnDeliveryFailureKind,
    pub attempts: u32,
    pub retry_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSupervisorTurn {
    pub vessel: String,
    pub role: String,
    pub message: crate::TerminalCrewMessage,
    /// Order assigned when the convoy authority accepts the turn. Older
    /// stored turns decode with zero; remove this default one roll later.
    #[serde(default)]
    pub queued_order: u64,
}

pub const PENDING_BRIEF_DELIVERY_SOURCE: &str = "operator";

impl ConvoyStatus {
    pub fn pending_brief(&self) -> Option<&PendingBrief> {
        self.turn_deliveries.get(PENDING_BRIEF_DELIVERY_SOURCE).and_then(|delivery| delivery.pending_brief.as_ref())
    }
}

fn clear_operator_pending_brief(status: &mut ConvoyStatus) {
    if let Some(delivery) = status.turn_deliveries.get_mut(PENDING_BRIEF_DELIVERY_SOURCE) {
        delivery.pending_brief = None;
    }
    if status.turn_deliveries.get(PENDING_BRIEF_DELIVERY_SOURCE).is_some_and(|delivery| delivery.episodes.is_empty()) {
        status.turn_deliveries.remove(PENDING_BRIEF_DELIVERY_SOURCE);
    }
}

fn clear_nudge_budget(status: &mut ConvoyStatus, vessel: &str, role: &str) {
    for obligation in &mut status.nudge_obligations {
        if matches!(&obligation.maker, LeafMaker::Actor { vessel: actor_vessel, role: actor_role } if actor_vessel == vessel && actor_role == role)
        {
            obligation.history.clear();
            obligation.quiet_since = None;
            obligation.reply_after = None;
        }
    }
}

fn clear_stall_for_crew(status: &mut ConvoyStatus, vessel: &str, role: &str) {
    let field_path = format!(".crew.{role}.phase");
    if status.stalled.as_ref().is_some_and(|stalled| {
        stalled
            .leaves
            .iter()
            .any(|leaf| matches!(&leaf.address, LeafAddress::Work { work, .. } if work == vessel) && leaf.field_path == field_path)
    }) {
        status.stalled = None;
    }
}

fn clear_pending_brief_for(status: &mut ConvoyStatus, vessel: &str, role: &str) {
    if status.pending_brief().is_some_and(|brief| brief.vessel == vessel && brief.role == role) {
        clear_operator_pending_brief(status);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct TurnDeliveryEpisode {
    /// Stored as `head_sha` before issue subjects generalised the revision
    /// (#2135); the alias keeps those convoy records decodable.
    #[serde(alias = "head_sha")]
    pub subject_revision: String,
    pub evidence_at: DateTime<Utc>,
    pub judged_claim_at: DateTime<Utc>,
    pub outcome: TurnDeliveryOutcome,
    /// Decodes episodes stored before sender attribution; remove after the next fleet roll.
    #[serde(default)]
    #[builder(default)]
    pub sender: crate::CrewMessageSender,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TurnDeliveryOutcome {
    /// Accepted into the terminal FIFO; no agent turn has been confirmed yet.
    Queued {
        rung: TurnDeliveryRung,
        queued_at: DateTime<Utc>,
        vessel: String,
        role: String,
        message_id: String,
        blocking_reason: String,
    },
    Delivered {
        rung: TurnDeliveryRung,
        delivered_at: DateTime<Utc>,
    },
    Refused {
        reason: String,
        refused_at: DateTime<Utc>,
        hold_executed: bool,
    },
}

/// One terminal receipt/readiness observation, applied with its convoy peers in one write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedTurnObservation {
    pub source: String,
    pub subject_revision: String,
    pub confirmed: bool,
    pub blocking_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ConvoyAttention {
    pub source: String,
    pub reason: String,
    pub raised_at: DateTime<Utc>,
}

impl TurnDeliveryOutcome {
    /// Allow startup, readiness observation and replica propagation before asking a supervisor.
    pub const QUEUED_BOUND: chrono::Duration = chrono::Duration::minutes(5);
}

impl ConvoyAttention {
    pub const QUEUED_TURN_SOURCE: &'static str = "queued-turn-delivery";
    pub const MISSING_TURN_HOOK_SOURCE: &'static str = "missing-turn-hook";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ConvoyPhase {
    #[default]
    Pending,
    Active,
    Interrupted,
    Anchored,
    Landing,
    Landed,
    Failed,
    Cancelled,
    Abandoned,
}

impl ConvoyPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Landed | Self::Failed | Self::Cancelled | Self::Abandoned)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct WorkState {
    pub phase: WorkPhase,
    /// Authority responsible for settling this work at the roll-up level.
    #[builder(default)]
    #[serde(default, skip_serializing_if = "WorkCompletionAuthority::is_crew_rollup")]
    pub completion_authority: WorkCompletionAuthority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkPhase {
    Pending,
    Ready,
    Launching,
    Running,
    Stalled,
    Interrupted,
    Complete,
    Failed,
    Cancelled,
    Abandoned,
}

impl WorkPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Failed | Self::Cancelled | Self::Abandoned)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WorkCompletionAuthority {
    #[default]
    CrewRollup,
    HumanOverride,
    Principal(PrincipalRef),
    Unattributed,
}

impl WorkCompletionAuthority {
    fn is_crew_rollup(&self) -> bool {
        *self == Self::CrewRollup
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CrewWorkState {
    pub phase: CrewWorkPhase,
    /// A resumed stalled crew is in grace until its new brief is delivered and
    /// a later idle observation ends that turn. The default decodes prior
    /// stored statuses; remove it one fleet roll after this change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_at: Option<DateTime<Utc>>,
    /// ID of the operator brief that opened this turn. Older stored statuses
    /// omit it; remove the compatibility default one roll after deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_brief_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    /// Optional pointer to the PR projection of the claim's decision ledger.
    /// The crew's convoy-bound decision-ledger artifact satisfies the ledger expectation;
    /// this pointer is additional evidence and is absent for PR-less claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_ledger_ref: Option<String>,
    /// Digest of the artifact body accepted with this claim, independent of PR projection.
    /// Decode prior stored claims without it for one generation; remove the default after the next fleet roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_ledger_digest: Option<String>,
    /// Completion claims displaced by a brief delivered at the turn boundary.
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub superseded_claims: Vec<SupersededCrewClaim>,
    /// Operator authority that admitted this claim without a decision ledger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_override: Option<CrewCompletionOverride>,
    /// The completion arrived while the agent process was alive and its
    /// attention had not reached idle.
    #[builder(default)]
    #[serde(default)]
    pub completed_while_crew_active: bool,
    /// Review evidence for this settlement claim. Kept optional while claim
    /// producers migrate onto the evidence-backed protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_evidence: Option<SettlementClaimEvidence>,
    /// Last refused settlement claim. The default decodes previous-generation
    /// statuses and can be removed one fleet roll after this field lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_refusal: Option<CrewCompletionRefusal>,
}

/// Machine-readable remedies, independent of the human-facing expectation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CrewCompletionRefusalCause {
    ConflictingChangeRequest { service: String, scope: String, number: u64 },
    MissingChangeRequestObservation { service: String, scope: String, number: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CrewCompletionRefusal {
    pub expectation: String,
    /// Previous-generation refusals have no typed causes. Remove the default
    /// one fleet roll after deployment (ADR 0047). Always write the new shape.
    #[serde(default)]
    pub causes: Vec<CrewCompletionRefusalCause>,
    pub consecutive_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupersededCrewClaim {
    pub claimed_at: DateTime<Utc>,
    pub message: Option<String>,
    pub disposition: Option<String>,
    pub decision_ledger_ref: Option<String>,
    /// Digest of the artifact body accepted with this claim, independent of PR projection.
    /// Decode prior stored claims without it for one generation; remove the default after the next fleet roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_ledger_digest: Option<String>,
    pub completion_override: Option<CrewCompletionOverride>,
    pub completed_while_crew_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CrewWorkPhase {
    #[default]
    Pending,
    Working,
    Interrupted,
    Stalled,
    Done,
    HandedBack,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlacementStatus {
    #[serde(flatten)]
    pub fields: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyStatusPatch {
    ObserveEnvironment {
        vessel: String,
        observation: flotilla_protocol::EnvironmentRuntimeObservation,
    },
    RecordEnsureAdmission {
        config: crate::ConvoyEnsureSpec,
    },
    DiscoverSubjects {
        subjects: Vec<(Subject, Relationship)>,
        source: SubjectDiscoverySource,
        at: DateTime<Utc>,
    },
    RecordBranchSubjectScan {
        at: DateTime<Utc>,
    },
    RecordBranchSubjectScanFailure {
        error: String,
    },
    UnlinkSubject {
        subject: Subject,
    },
    SetStalled {
        condition: Option<StalledCondition>,
    },
    SetNudgeObligations {
        obligations: Vec<NudgeObligation>,
    },
    SetTeardownWait {
        message: String,
    },
    RecordLifecycleMutation {
        mutation: LifecycleMutation,
    },
    SetPlacementDecision {
        placement_decision: PlacementDecision,
    },
    Bootstrap {
        workflow_snapshot: WorkflowSnapshot,
        observed_workflow_ref: String,
        observed_workflows: BTreeMap<String, String>,
        work: BTreeMap<String, WorkState>,
        crew_work: BTreeMap<String, BTreeMap<String, CrewWorkState>>,
        phase: ConvoyPhase,
        started_at: Option<DateTime<Utc>>,
    },
    BackfillCrewWork {
        crew_work: BTreeMap<String, BTreeMap<String, CrewWorkState>>,
        completion_overrides: BTreeSet<String>,
    },
    FailInit {
        phase: ConvoyPhase,
        message: String,
        finished_at: DateTime<Utc>,
    },
    AdvanceWorkToReady {
        ready: BTreeMap<String, DateTime<Utc>>,
    },
    /// `cancelled_work` is computed from non-terminal work. Apply checks that
    /// condition again because a concurrent patch may have settled a work item.
    FailConvoy {
        cancelled_work: BTreeMap<String, DateTime<Utc>>,
        finished_at: DateTime<Utc>,
        message: Option<String>,
    },
    RollUpPhase {
        phase: ConvoyPhase,
        started_at: Option<DateTime<Utc>>,
        finished_at: Option<DateTime<Utc>>,
    },
    Settle {
        disposition: String,
        target_mismatches: Vec<TargetMismatch>,
        finished_at: DateTime<Utc>,
    },
    SetSettlementAttention {
        attention: Option<ConvoyAttention>,
    },
    /// Advisory hook health must neither replace nor clear another attention source.
    ObserveTurnHookHealth {
        reason: Option<String>,
        observed_at: DateTime<Utc>,
    },
    QueueSupervisorTurn {
        turn: PendingSupervisorTurn,
    },
    AcknowledgeSupervisorTurn {
        message_id: String,
    },
    WorkLaunching {
        work: String,
        started_at: DateTime<Utc>,
        placement: PlacementStatus,
    },
    WorkRunning {
        work: String,
        started_at: DateTime<Utc>,
        /// Crew whose sessions the vessel actually launched. Latent agents are
        /// absent and stay `Pending` until a handoff starts them.
        launched_roles: BTreeSet<String>,
    },
    WorkInterrupted {
        work: String,
        roles: BTreeSet<String>,
        message: String,
    },
    /// One-shot work outcomes accept non-terminal input or a duplicate of the
    /// same outcome. The apply path rejects a different terminal outcome so
    /// its sticky finished_at cannot be misattributed to this transition.
    ForceWorkCompleted {
        work: String,
        finished_at: DateTime<Utc>,
        message: Option<String>,
    },
    MarkWorkFailed {
        work: String,
        finished_at: DateTime<Utc>,
        message: String,
    },
    MarkWorkCancelled {
        work: String,
        finished_at: DateTime<Utc>,
    },
    MarkConvoyAbandoned {
        expected_phase: ConvoyPhase,
        finished_at: DateTime<Utc>,
        authority: WorkCompletionAuthority,
        reason: String,
    },
    MarkCrewCompleted {
        vessel: String,
        role: String,
        finished_at: DateTime<Utc>,
        message: Option<String>,
        disposition: Option<String>,
        decision_ledger_ref: Option<String>,
        decision_ledger_digest: Option<String>,
        completed_while_crew_active: bool,
        forced_by: Option<PrincipalRef>,
    },
    RefuseCrewCompletion {
        vessel: String,
        role: String,
        expectation: String,
        causes: Vec<CrewCompletionRefusalCause>,
        message: Option<String>,
    },
    MarkCrewFailed {
        vessel: String,
        role: String,
        finished_at: DateTime<Utc>,
        message: String,
    },
    MarkCrewStalled {
        convoy: String,
        vessel: String,
        role: String,
        at: DateTime<Utc>,
        reason: StallReason,
        proposed_disposition: Option<StallProposedDisposition>,
        message: String,
    },
    HandoffCrewWork {
        vessel: String,
        sender_role: String,
        target_role: String,
        handed_off_at: DateTime<Utc>,
        message: String,
    },
    ResumeCrewWork {
        vessel: String,
        role: String,
        resumed_at: DateTime<Utc>,
        prompt: String,
        brief_id: Option<String>,
    },
    SetPendingBrief {
        pending_brief: PendingBrief,
    },
    ClearPendingBrief,
    DeliverPendingBrief {
        vessel: String,
        role: String,
        delivered_at: DateTime<Utc>,
        content: String,
        completion_message: Option<String>,
        disposition: Option<String>,
        decision_ledger_ref: Option<String>,
        decision_ledger_digest: Option<String>,
        completed_while_crew_active: bool,
        forced_by: Option<PrincipalRef>,
    },
    RecordTurnDelivery {
        source: String,
        episode: TurnDeliveryEpisode,
        vessel: String,
        role: String,
        prompt: String,
    },
    ObserveQueuedTurnDeliveries {
        observations: Vec<QueuedTurnObservation>,
        observed_at: DateTime<Utc>,
    },
    FailTurnDelivery {
        source: String,
        failure: TurnDeliveryFailure,
    },
    RefuseTurnDelivery {
        source: String,
        episode: TurnDeliveryEpisode,
        attention: ConvoyAttention,
    },
    RollUpWork {
        work: String,
        phase: WorkPhase,
        transitioned_at: DateTime<Utc>,
        message: Option<String>,
    },
}

impl StatusPatch<ConvoyStatus> for ConvoyStatusPatch {
    fn apply(&self, status: &mut ConvoyStatus) {
        // Optimistic retry can reapply a patch computed from an older phase.
        // Terminal outcomes reject stale and duplicate patches. An explicit
        // abandon may override a terminal outcome only when its caller saw
        // that exact phase. Mutation audit records remain appendable, and
        // SetStalled and queued-turn observations may clear stale attention.
        if status.phase.is_terminal()
            && !matches!(
                self,
                Self::ObserveEnvironment { .. }
                    | Self::RecordLifecycleMutation { .. }
                    | Self::SetStalled { .. }
                    | Self::SetTeardownWait { .. }
                    | Self::ObserveQueuedTurnDeliveries { .. }
            )
            && !matches!(self, Self::MarkConvoyAbandoned { expected_phase, .. } if *expected_phase == status.phase && status.phase != ConvoyPhase::Abandoned)
        {
            return;
        }
        match self {
            Self::ObserveEnvironment { vessel, observation } => {
                status.environment_observations.entry(vessel.clone()).or_default().merge(observation)
            }
            Self::RecordEnsureAdmission { config } => {
                status.ensure_admission.get_or_insert_with(|| config.clone());
            }
            Self::DiscoverSubjects { subjects, source, at } => {
                for (subject, relationship) in subjects {
                    status.discover_subject(subject.clone(), *relationship, *source, *at);
                }
            }
            Self::RecordBranchSubjectScan { at } => {
                status.branch_subject_scan_at = Some(*at);
                status.branch_subject_scan_error = None;
            }
            Self::RecordBranchSubjectScanFailure { error } => status.branch_subject_scan_error = Some(error.clone()),
            Self::UnlinkSubject { subject } => status.unlink_subject(subject),
            Self::SetStalled { condition } => {
                status.stalled = if status.phase.is_terminal() { None } else { condition.clone() };
            }
            Self::SetNudgeObligations { obligations } => status.nudge_obligations = obligations.clone(),
            Self::SetTeardownWait { message } => status.message = Some(message.clone()),
            Self::RecordLifecycleMutation { mutation } => {
                const RETAINED_MUTATIONS: usize = 32;
                status.lifecycle_mutations.push(mutation.clone());
                if status.lifecycle_mutations.len() > RETAINED_MUTATIONS {
                    status.lifecycle_mutations.remove(0);
                }
            }
            Self::SetPlacementDecision { placement_decision } => {
                status.placement_decision.get_or_insert_with(|| placement_decision.clone());
            }
            Self::Bootstrap { workflow_snapshot, observed_workflow_ref, observed_workflows, work, crew_work, phase, started_at } => {
                status.provisioning.get_or_insert(ConvoyProvisioningState::NotStarted);
                status.workflow_snapshot = Some(workflow_snapshot.clone());
                status.observed_workflow_ref = Some(observed_workflow_ref.clone());
                status.observed_workflows = Some(observed_workflows.clone());
                status.work = work.clone();
                status.crew_work = crew_work.clone();
                status.phase = *phase;
                if let Some(started_at) = started_at {
                    status.started_at.get_or_insert(*started_at);
                }
            }
            Self::BackfillCrewWork { crew_work, completion_overrides } => {
                for (vessel, missing_crew) in crew_work {
                    let crew = status.crew_work.entry(vessel.clone()).or_default();
                    for (role, state) in missing_crew {
                        crew.entry(role.clone()).or_insert_with(|| state.clone());
                    }
                }
                for work in completion_overrides {
                    if let Some(state) = status.work.get_mut(work) {
                        state.completion_authority = WorkCompletionAuthority::HumanOverride;
                    }
                }
            }
            Self::FailInit { phase, message, finished_at } => {
                status.provisioning.get_or_insert(ConvoyProvisioningState::NotStarted);
                status.phase = *phase;
                status.message = Some(message.clone());
                status.finished_at.get_or_insert(*finished_at);
                if phase.is_terminal() {
                    clear_operator_pending_brief(status);
                }
            }
            Self::AdvanceWorkToReady { ready } => {
                if !matches!(status.provisioning, Some(ConvoyProvisioningState::Started { .. })) {
                    if let Some(started_at) = ready.values().min() {
                        status.provisioning = Some(ConvoyProvisioningState::Started { started_at: *started_at });
                    }
                }
                for (work, ready_at) in ready {
                    if let Some(state) = status.work.get_mut(work) {
                        state.phase = WorkPhase::Ready;
                        state.ready_at.get_or_insert(*ready_at);
                    }
                }
            }
            Self::FailConvoy { cancelled_work, finished_at, message } => {
                let previous_phase = status.phase;
                status.phase = ConvoyPhase::Failed;
                if previous_phase != ConvoyPhase::Failed {
                    status.finished_at = None;
                }
                status.finished_at.get_or_insert(*finished_at);
                status.message = message.clone();
                for (work, cancelled_at) in cancelled_work {
                    if let Some(state) = status.work.get_mut(work) {
                        if !state.phase.is_terminal() {
                            state.phase = WorkPhase::Cancelled;
                            state.finished_at.get_or_insert(*cancelled_at);
                        }
                    }
                }
                clear_operator_pending_brief(status);
            }
            Self::RollUpPhase { phase, started_at, finished_at } => {
                // Derived Active roll-up is a continuation of the convoy voyage, not a new attempt.
                let previous_phase = status.phase;
                status.phase = *phase;
                if *phase == ConvoyPhase::Active {
                    status.finished_at = None;
                }
                if let Some(started_at) = started_at {
                    status.started_at.get_or_insert(*started_at);
                }
                if let Some(finished_at) = finished_at {
                    // A changed terminal outcome settles at its own transition time.
                    if previous_phase != *phase {
                        status.finished_at = None;
                    }
                    status.finished_at.get_or_insert(*finished_at);
                }
                if phase.is_terminal() {
                    clear_operator_pending_brief(status);
                }
            }
            Self::Settle { disposition, target_mismatches, finished_at } => {
                let previous_phase = status.phase;
                status.phase = ConvoyPhase::Landed;
                status.target_mismatches = target_mismatches.clone();
                status.disposition = Some(disposition.clone());
                if previous_phase != ConvoyPhase::Landed {
                    status.finished_at = None;
                }
                status.finished_at.get_or_insert(*finished_at);
                status.attention = None;
                clear_operator_pending_brief(status);
            }
            Self::SetSettlementAttention { attention } => status.attention = attention.clone(),
            Self::ObserveTurnHookHealth { reason, observed_at } => {
                if status.attention.as_ref().is_none_or(|attention| attention.source == ConvoyAttention::MISSING_TURN_HOOK_SOURCE) {
                    match reason {
                        Some(reason) => {
                            let raised_at = status.attention.as_ref().map_or(*observed_at, |attention| attention.raised_at);
                            status.attention = Some(ConvoyAttention {
                                source: ConvoyAttention::MISSING_TURN_HOOK_SOURCE.into(),
                                reason: reason.clone(),
                                raised_at,
                            });
                        }
                        None => status.attention = None,
                    }
                }
            }
            Self::QueueSupervisorTurn { turn } => {
                let existing_order = status
                    .turn_deliveries
                    .get(&turn.message.id)
                    .and_then(|delivery| delivery.pending_supervisor_turn.as_ref())
                    .map(|turn| turn.queued_order);
                let next_order = status
                    .turn_deliveries
                    .values()
                    .filter_map(|delivery| delivery.pending_supervisor_turn.as_ref().map(|turn| turn.queued_order))
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                let mut queued = turn.clone();
                queued.queued_order = existing_order.unwrap_or(next_order);
                status.turn_deliveries.entry(turn.message.id.clone()).or_default().pending_supervisor_turn = Some(queued);
            }
            Self::AcknowledgeSupervisorTurn { message_id } => {
                if let Some(delivery) = status.turn_deliveries.get_mut(message_id) {
                    delivery.pending_supervisor_turn = None;
                    if delivery.episodes.is_empty() && delivery.pending_brief.is_none() {
                        status.turn_deliveries.remove(message_id);
                    }
                }
                // One-generation cleanup for attention written by the original #2285 patch.
                if !status.turn_deliveries.values().any(|delivery| delivery.pending_supervisor_turn.is_some())
                    && status.attention.as_ref().is_some_and(|attention| attention.source == "supervisor-turn-delivery")
                {
                    status.attention = None;
                }
            }
            Self::WorkLaunching { work, started_at, placement } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Launching;
                    state.started_at.get_or_insert(*started_at);
                    state.placement = Some(placement.clone());
                }
            }
            Self::WorkRunning { work, started_at, launched_roles } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Running;
                    state.completion_authority = WorkCompletionAuthority::CrewRollup;
                    state.message = None;
                }
                if let Some(crew) = status.crew_work.get_mut(work) {
                    // Only the crew the vessel launched are working. A latent
                    // agent has no session yet, so it stays Pending until a
                    // handoff starts it.
                    for (_, state) in
                        crew.iter_mut().filter(|(role, state)| state.phase == CrewWorkPhase::Pending && launched_roles.contains(*role))
                    {
                        state.phase = CrewWorkPhase::Working;
                        state.started_at.get_or_insert(*started_at);
                    }
                    for state in crew.values_mut().filter(|state| state.phase == CrewWorkPhase::Interrupted) {
                        state.phase = CrewWorkPhase::Working;
                        state.message = None;
                    }
                }
            }
            Self::WorkInterrupted { work, roles, message } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Interrupted;
                    state.completion_authority = WorkCompletionAuthority::CrewRollup;
                    state.message = Some(message.clone());
                    state.finished_at = None;
                }
                if let Some(crew) = status.crew_work.get_mut(work) {
                    for (role, state) in crew.iter_mut().filter(|(role, state)| {
                        roles.contains(*role) && matches!(state.phase, CrewWorkPhase::Working | CrewWorkPhase::Interrupted)
                    }) {
                        state.phase = CrewWorkPhase::Interrupted;
                        state.message = Some(format!("crew session for `{role}` was interrupted"));
                        state.finished_at = None;
                    }
                }
            }
            Self::ForceWorkCompleted { work, finished_at, message } => {
                if let Some(state) = status.work.get_mut(work) {
                    if state.phase.is_terminal() && state.phase != WorkPhase::Complete {
                        return;
                    }
                    state.phase = WorkPhase::Complete;
                    state.completion_authority = WorkCompletionAuthority::HumanOverride;
                    state.finished_at.get_or_insert(*finished_at);
                    state.message = message.clone();
                }
                enter_landing_if_completion_claims_settled(status);
            }
            Self::MarkWorkFailed { work, finished_at, message } => {
                if let Some(state) = status.work.get_mut(work) {
                    if state.phase.is_terminal() && state.phase != WorkPhase::Failed {
                        return;
                    }
                    state.phase = WorkPhase::Failed;
                    state.finished_at.get_or_insert(*finished_at);
                    state.message = Some(message.clone());
                }
            }
            Self::MarkWorkCancelled { work, finished_at } => {
                if let Some(state) = status.work.get_mut(work) {
                    if state.phase.is_terminal() && state.phase != WorkPhase::Cancelled {
                        return;
                    }
                    state.phase = WorkPhase::Cancelled;
                    state.finished_at.get_or_insert(*finished_at);
                }
            }
            Self::MarkConvoyAbandoned { expected_phase, finished_at, authority, reason } => {
                if status.phase != *expected_phase {
                    return;
                }
                status.phase = ConvoyPhase::Abandoned;
                status.finished_at = Some(*finished_at);
                let actor = match authority {
                    WorkCompletionAuthority::CrewRollup => "crew rollup".to_string(),
                    WorkCompletionAuthority::HumanOverride => "human override".to_string(),
                    WorkCompletionAuthority::Principal(principal) => principal.name.clone(),
                    WorkCompletionAuthority::Unattributed => "unattributed".to_string(),
                };
                status.message = Some(format!("abandoned by {actor}: {reason}"));
                for state in status.work.values_mut().filter(|state| !state.phase.is_terminal()) {
                    state.phase = WorkPhase::Abandoned;
                    state.completion_authority = authority.clone();
                    state.finished_at.get_or_insert(*finished_at);
                    state.message = Some(reason.clone());
                }
                clear_operator_pending_brief(status);
            }
            Self::MarkCrewCompleted {
                vessel,
                role,
                finished_at,
                message,
                disposition,
                decision_ledger_ref,
                decision_ledger_digest,
                completed_while_crew_active,
                forced_by,
            } => {
                clear_nudge_budget(status, vessel, role);
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    state.completion_refusal = None;
                    // Duplicate settlement is sticky; changing the settled outcome records its own time.
                    if state.phase != CrewWorkPhase::Done
                        || disposition.as_ref().is_some_and(|disposition| state.disposition.as_ref() != Some(disposition))
                    {
                        state.finished_at = None;
                    }
                    state.phase = CrewWorkPhase::Done;
                    state.finished_at.get_or_insert(*finished_at);
                    state.message = message.clone();
                    if disposition.is_some() {
                        state.disposition = disposition.clone();
                    }
                    if decision_ledger_ref.is_some() {
                        state.decision_ledger_ref = decision_ledger_ref.clone();
                    }
                    if decision_ledger_digest.is_some() {
                        state.decision_ledger_digest = decision_ledger_digest.clone();
                    }
                }
                if let Some(principal) = forced_by {
                    if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                        if state.decision_ledger_ref.is_none() && state.decision_ledger_digest.is_none() {
                            state.completion_override =
                                Some(CrewCompletionOverride { principal: principal.clone(), forced_at: *finished_at });
                        }
                    }
                }
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    state.completed_while_crew_active |= *completed_while_crew_active;
                }
                clear_stall_for_crew(status, vessel, role);
                enter_landing_if_completion_claims_settled(status);
            }
            Self::RefuseCrewCompletion { vessel, role, expectation, causes, message } => {
                clear_nudge_budget(status, vessel, role);
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    let new_expectation = state.completion_refusal.as_ref().is_none_or(|prior| prior.expectation != *expectation);
                    let consecutive_count = state
                        .completion_refusal
                        .as_ref()
                        .filter(|prior| prior.expectation == *expectation)
                        .map_or(1, |prior| prior.consecutive_count.saturating_add(1));
                    state.completion_refusal = Some(
                        CrewCompletionRefusal::builder()
                            .expectation(expectation.clone())
                            .causes(causes.clone())
                            .consecutive_count(consecutive_count)
                            .maybe_message(message.clone())
                            .build(),
                    );
                    if new_expectation
                        && status.stalled.as_ref().is_some_and(|stalled| {
                            stalled.leaves.iter().any(|leaf| {
                                matches!(&leaf.address, LeafAddress::Work { work, .. } if work == vessel)
                                    && leaf.field_path == format!(".crew.{role}.phase")
                            })
                        })
                    {
                        status.stalled = None;
                    }
                }
            }
            Self::MarkCrewFailed { vessel, role, finished_at, message } => {
                if let Some(work) = status.work.get_mut(vessel) {
                    work.completion_authority = WorkCompletionAuthority::CrewRollup;
                }
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    // Duplicate settlement is sticky; changing the settled outcome records its own time.
                    if state.phase != CrewWorkPhase::Failed {
                        state.finished_at = None;
                    }
                    state.phase = CrewWorkPhase::Failed;
                    state.finished_at.get_or_insert(*finished_at);
                    state.message = Some(message.clone());
                }
                clear_pending_brief_for(status, vessel, role);
                clear_stall_for_crew(status, vessel, role);
            }
            Self::MarkCrewStalled { convoy, vessel, role, at, reason, proposed_disposition, message } => {
                clear_nudge_budget(status, vessel, role);
                // A completion claim is settled work. A late stall cannot turn a
                // world-owned landing wait back into a crew obligation.
                if status.crew_work.get(vessel).and_then(|crew| crew.get(role)).is_some_and(|state| state.phase == CrewWorkPhase::Done) {
                    return;
                }
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    state.phase = CrewWorkPhase::Stalled;
                    state.finished_at = None;
                    state.message = Some(message.clone());
                }
                status.stalled = Some(StalledCondition {
                    leaves: vec![Leaf {
                        address: LeafAddress::Work { convoy: convoy.clone(), work: vessel.clone() },
                        field_path: format!(".crew.{role}.phase"),
                        operator: LeafOperator::Equal,
                        literal: "Done".into(),
                    }],
                    maker: Some(LeafMaker::Actor { vessel: vessel.clone(), role: role.clone() }),
                    evidence: message.clone(),
                    source: StallEvidenceSource::Crew,
                    cause: None,
                    began_at: *at,
                    rung: StallRung::Operator,
                    supervisor: None,
                    supervision_index: None,
                    supervision_exhausted: false,
                    reason: Some(*reason),
                    proposed_disposition: *proposed_disposition,
                    nudge_history: Vec::new(),
                });
            }
            Self::HandoffCrewWork { vessel, sender_role, target_role, handed_off_at, message } => {
                if let Some(work) = status.work.get_mut(vessel) {
                    work.completion_authority = WorkCompletionAuthority::CrewRollup;
                }
                let Some(crew) = status.crew_work.get_mut(vessel) else {
                    return;
                };
                let target_was_done = crew.get(target_role).is_some_and(|state| state.phase == CrewWorkPhase::Done);
                if let Some(target) = crew.get_mut(target_role) {
                    if matches!(target.phase, CrewWorkPhase::Pending | CrewWorkPhase::Done | CrewWorkPhase::HandedBack) {
                        // Hand-back continues the same crew process, preserving its original start.
                        target.phase = CrewWorkPhase::Working;
                        target.started_at.get_or_insert(*handed_off_at);
                        target.finished_at = None;
                        target.decision_ledger_digest = None;
                        target.message = Some(message.clone());
                    }
                }
                if target_was_done && sender_role != target_role {
                    if let Some(sender) = crew.get_mut(sender_role) {
                        sender.phase = CrewWorkPhase::HandedBack;
                        sender.finished_at = Some(*handed_off_at);
                        sender.message = Some(message.clone());
                    }
                    clear_pending_brief_for(status, vessel, sender_role);
                }
            }
            Self::ResumeCrewWork { vessel, role, resumed_at, prompt, brief_id } => {
                status.phase = ConvoyPhase::Active;
                status.finished_at = None;
                if let Some(work) = status.work.get_mut(vessel) {
                    work.phase = WorkPhase::Running;
                    work.finished_at = None;
                    work.completion_authority = WorkCompletionAuthority::CrewRollup;
                }
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    if brief_id.is_some() {
                        state.resumed_at = Some(*resumed_at);
                        state.resume_brief_id = brief_id.clone();
                    } else {
                        state.resumed_at = None;
                        state.resume_brief_id = None;
                    }
                    state.phase = CrewWorkPhase::Working;
                    state.started_at.get_or_insert(*resumed_at);
                    state.finished_at = None;
                    state.decision_ledger_digest = None;
                    state.message = Some(prompt.clone());
                }
                status.stalled = None;
                clear_pending_brief_for(status, vessel, role);
            }
            Self::SetPendingBrief { pending_brief } => {
                status.turn_deliveries.entry(PENDING_BRIEF_DELIVERY_SOURCE.to_string()).or_default().pending_brief =
                    Some(pending_brief.clone());
            }
            Self::ClearPendingBrief => {
                clear_operator_pending_brief(status);
            }
            Self::DeliverPendingBrief {
                vessel,
                role,
                delivered_at,
                content,
                completion_message,
                disposition,
                decision_ledger_ref,
                decision_ledger_digest,
                completed_while_crew_active,
                forced_by,
            } => {
                let matches_pending =
                    status.pending_brief().is_some_and(|brief| brief.vessel == *vessel && brief.role == *role && brief.content == *content);
                if !matches_pending {
                    return;
                }
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    state.superseded_claims.push(SupersededCrewClaim {
                        claimed_at: *delivered_at,
                        message: completion_message.clone(),
                        disposition: disposition.clone(),
                        decision_ledger_ref: decision_ledger_ref.clone(),
                        decision_ledger_digest: decision_ledger_digest.clone(),
                        completion_override: forced_by
                            .as_ref()
                            .filter(|_| decision_ledger_ref.is_none() && decision_ledger_digest.is_none())
                            .map(|principal| CrewCompletionOverride { principal: principal.clone(), forced_at: *delivered_at }),
                        completed_while_crew_active: *completed_while_crew_active,
                    });
                }
                clear_operator_pending_brief(status);
                status.phase = ConvoyPhase::Active;
                status.finished_at = None;
                if let Some(work) = status.work.get_mut(vessel) {
                    work.phase = WorkPhase::Running;
                    work.finished_at = None;
                    work.completion_authority = WorkCompletionAuthority::CrewRollup;
                }
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    state.phase = CrewWorkPhase::Working;
                    state.started_at.get_or_insert(*delivered_at);
                    state.finished_at = None;
                    state.message = Some(content.clone());
                    state.disposition = None;
                    state.decision_ledger_ref = None;
                    state.decision_ledger_digest = None;
                    state.completion_override = None;
                    state.completed_while_crew_active = false;
                }
            }
            Self::RecordTurnDelivery { source, episode, vessel, role, prompt } => {
                let delivery = status.turn_deliveries.entry(source.clone()).or_default();
                delivery.failure = None;
                if !delivery.episodes.iter().any(|existing| existing.subject_revision == episode.subject_revision) {
                    delivery.episodes.push(episode.clone());
                }
                status.attention = None;
                if let Some(work) = status.work.get_mut(vessel) {
                    work.phase = WorkPhase::Running;
                    work.finished_at = None;
                    work.completion_authority = WorkCompletionAuthority::CrewRollup;
                }
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    state.phase = CrewWorkPhase::Working;
                    state.finished_at = None;
                    state.decision_ledger_digest = None;
                    state.message = Some(prompt.clone());
                }
                status.phase = ConvoyPhase::Active;
                status.finished_at = None;
            }
            Self::ObserveQueuedTurnDeliveries { observations, observed_at } => {
                for QueuedTurnObservation { source, subject_revision, confirmed, blocking_reason } in observations {
                    if let Some(episode) = status
                        .turn_deliveries
                        .get_mut(source)
                        .and_then(|delivery| delivery.episodes.iter_mut().find(|episode| episode.subject_revision == *subject_revision))
                    {
                        if let TurnDeliveryOutcome::Queued { rung, blocking_reason: reason, .. } = &mut episode.outcome {
                            if *confirmed {
                                episode.outcome = TurnDeliveryOutcome::Delivered { rung: *rung, delivered_at: *observed_at };
                            } else {
                                *reason = blocking_reason.clone();
                            }
                        }
                    }
                }
                // Re-evaluate all pending episodes after optimistic retry. One confirmation
                // must not clear another queued turn's attention or unrelated operator attention.
                // When unrelated attention occupies the slot, overdue turns remain visible
                // in explain. This advisory is raised once that other attention clears.
                if status.attention.as_ref().is_none_or(|attention| attention.source == ConvoyAttention::QUEUED_TURN_SOURCE) {
                    let terminal = status.phase.is_terminal();
                    let overdue = status
                        .turn_deliveries
                        .iter()
                        .flat_map(|(source, delivery)| {
                            delivery.episodes.iter().filter_map(move |episode| match &episode.outcome {
                                TurnDeliveryOutcome::Queued { queued_at, blocking_reason, message_id, .. }
                                    if !terminal && observed_at.signed_duration_since(*queued_at) > TurnDeliveryOutcome::QUEUED_BOUND =>
                                {
                                    Some(format!(
                                        "{source}: {message_id} queued since {} without submission: {blocking_reason}",
                                        queued_at.to_rfc3339()
                                    ))
                                }
                                _ => None,
                            })
                        })
                        .collect::<Vec<_>>();
                    status.attention = if overdue.is_empty() {
                        None
                    } else {
                        Some(ConvoyAttention {
                            source: ConvoyAttention::QUEUED_TURN_SOURCE.into(),
                            reason: overdue.join("\n"),
                            raised_at: status.attention.as_ref().map_or(*observed_at, |attention| attention.raised_at),
                        })
                    };
                }
            }
            Self::FailTurnDelivery { source, failure } => {
                let delivery = status.turn_deliveries.entry(source.clone()).or_default();
                let changed = delivery.failure.as_ref().is_none_or(|prior| prior.kind != failure.kind || prior.reason != failure.reason);
                delivery.failure = Some(failure.clone());
                // Keep unrelated settlement/operator attention and the original
                // raised_at across backoff attempts with unchanged failure evidence.
                if changed && status.attention.as_ref().is_none_or(|attention| attention.source == *source) {
                    status.attention =
                        Some(ConvoyAttention { source: source.clone(), reason: failure.reason.clone(), raised_at: failure.failed_at });
                }
            }
            Self::RefuseTurnDelivery { source, episode, attention } => {
                let delivery = status.turn_deliveries.entry(source.clone()).or_default();
                delivery.failure = None;
                if !delivery.episodes.iter().any(|existing| existing.subject_revision == episode.subject_revision) {
                    delivery.episodes.push(episode.clone());
                }
                status.attention = Some(attention.clone());
            }
            Self::RollUpWork { work, phase, transitioned_at, message } => {
                if let Some(state) = status.work.get_mut(work) {
                    // Work roll-up reports the same process across continuation and re-settlement.
                    let previous_phase = state.phase;
                    state.phase = *phase;
                    state.completion_authority = WorkCompletionAuthority::CrewRollup;
                    state.message = message.clone();
                    match phase {
                        WorkPhase::Complete | WorkPhase::Failed | WorkPhase::Cancelled | WorkPhase::Abandoned => {
                            if previous_phase != *phase {
                                state.finished_at = None;
                            }
                            state.finished_at.get_or_insert(*transitioned_at);
                        }
                        WorkPhase::Pending
                        | WorkPhase::Ready
                        | WorkPhase::Launching
                        | WorkPhase::Running
                        | WorkPhase::Stalled
                        | WorkPhase::Interrupted => {
                            state.finished_at = None;
                        }
                    }
                }
            }
        }
    }
}

pub mod controller_patches {
    use super::*;

    pub fn bootstrap(
        workflow_snapshot: WorkflowSnapshot,
        observed_workflow_ref: String,
        observed_workflows: BTreeMap<String, String>,
        work: BTreeMap<String, WorkState>,
        crew_work: BTreeMap<String, BTreeMap<String, CrewWorkState>>,
        phase: ConvoyPhase,
        started_at: Option<DateTime<Utc>>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::Bootstrap { workflow_snapshot, observed_workflow_ref, observed_workflows, work, crew_work, phase, started_at }
    }

    pub fn fail_init(phase: ConvoyPhase, message: String, finished_at: DateTime<Utc>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::FailInit { phase, message, finished_at }
    }

    pub fn backfill_crew_work(
        crew_work: BTreeMap<String, BTreeMap<String, CrewWorkState>>,
        completion_overrides: BTreeSet<String>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::BackfillCrewWork { crew_work, completion_overrides }
    }

    pub fn advance_work_to_ready(ready: BTreeMap<String, DateTime<Utc>>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::AdvanceWorkToReady { ready }
    }

    pub fn fail_convoy(
        cancelled_work: BTreeMap<String, DateTime<Utc>>,
        finished_at: DateTime<Utc>,
        message: Option<String>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::FailConvoy { cancelled_work, finished_at, message }
    }

    pub fn roll_up_phase(phase: ConvoyPhase, started_at: Option<DateTime<Utc>>, finished_at: Option<DateTime<Utc>>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::RollUpPhase { phase, started_at, finished_at }
    }

    pub fn settle(disposition: String, target_mismatches: Vec<TargetMismatch>, finished_at: DateTime<Utc>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::Settle { disposition, target_mismatches, finished_at }
    }

    pub fn roll_up_work(work: String, phase: WorkPhase, transitioned_at: DateTime<Utc>, message: Option<String>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::RollUpWork { work, phase, transitioned_at, message }
    }
}

pub mod provisioning_patches {
    use super::*;

    pub fn work_launching(work: String, started_at: DateTime<Utc>, placement: PlacementStatus) -> ConvoyStatusPatch {
        ConvoyStatusPatch::WorkLaunching { work, started_at, placement }
    }

    pub fn work_running(work: String, started_at: DateTime<Utc>, launched_roles: BTreeSet<String>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::WorkRunning { work, started_at, launched_roles }
    }

    pub fn work_interrupted(work: String, roles: BTreeSet<String>, message: String) -> ConvoyStatusPatch {
        ConvoyStatusPatch::WorkInterrupted { work, roles, message }
    }
}

fn enter_landing_if_completion_claims_settled(status: &mut ConvoyStatus) {
    if status.phase != ConvoyPhase::Active || status.work.is_empty() {
        return;
    }

    let all_work_claimed_complete = status.work.iter().all(|(work, state)| {
        state.phase == WorkPhase::Complete
            || (state.completion_authority == WorkCompletionAuthority::CrewRollup
                && status
                    .crew_work
                    .get(work)
                    .is_some_and(|crew| !crew.is_empty() && crew.values().all(|member| member.phase == CrewWorkPhase::Done)))
    });
    if all_work_claimed_complete {
        status.phase = ConvoyPhase::Landing;
    }
}

pub mod external_patches {
    use super::*;

    pub fn force_work_completed(work: String, finished_at: DateTime<Utc>, message: Option<String>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::ForceWorkCompleted { work, finished_at, message }
    }

    pub fn mark_work_failed(work: String, finished_at: DateTime<Utc>, message: String) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkWorkFailed { work, finished_at, message }
    }

    pub fn mark_work_cancelled(work: String, finished_at: DateTime<Utc>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkWorkCancelled { work, finished_at }
    }

    pub fn mark_convoy_abandoned(
        expected_phase: ConvoyPhase,
        finished_at: DateTime<Utc>,
        authority: WorkCompletionAuthority,
        reason: String,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkConvoyAbandoned { expected_phase, finished_at, authority, reason }
    }

    pub fn mark_crew_completed(
        vessel: String,
        role: String,
        finished_at: DateTime<Utc>,
        message: Option<String>,
        disposition: Option<String>,
        decision_ledger_ref: Option<String>,
    ) -> ConvoyStatusPatch {
        mark_crew_completed_with_context(vessel, role, finished_at, message, disposition, decision_ledger_ref, None, false, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn mark_crew_completed_with_context(
        vessel: String,
        role: String,
        finished_at: DateTime<Utc>,
        message: Option<String>,
        disposition: Option<String>,
        decision_ledger_ref: Option<String>,
        decision_ledger_digest: Option<String>,
        completed_while_crew_active: bool,
        forced_by: Option<PrincipalRef>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkCrewCompleted {
            vessel,
            role,
            finished_at,
            message,
            disposition,
            decision_ledger_ref,
            decision_ledger_digest,
            completed_while_crew_active,
            forced_by,
        }
    }

    pub fn mark_crew_failed(vessel: String, role: String, finished_at: DateTime<Utc>, message: String) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkCrewFailed { vessel, role, finished_at, message }
    }

    pub fn mark_crew_stalled(
        convoy: String,
        vessel: String,
        role: String,
        at: DateTime<Utc>,
        reason: StallReason,
        proposed_disposition: Option<StallProposedDisposition>,
        message: String,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkCrewStalled { convoy, vessel, role, at, reason, proposed_disposition, message }
    }

    pub fn handoff_crew_work(
        vessel: String,
        sender_role: String,
        target_role: String,
        handed_off_at: DateTime<Utc>,
        message: String,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::HandoffCrewWork { vessel, sender_role, target_role, handed_off_at, message }
    }

    pub fn resume_crew_work(
        vessel: String,
        role: String,
        resumed_at: DateTime<Utc>,
        prompt: String,
        brief_id: Option<String>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::ResumeCrewWork { vessel, role, resumed_at, prompt, brief_id }
    }

    pub fn set_pending_brief(pending_brief: PendingBrief) -> ConvoyStatusPatch {
        ConvoyStatusPatch::SetPendingBrief { pending_brief }
    }

    pub fn clear_pending_brief() -> ConvoyStatusPatch {
        ConvoyStatusPatch::ClearPendingBrief
    }

    #[allow(clippy::too_many_arguments)]
    pub fn deliver_pending_brief(
        vessel: String,
        role: String,
        delivered_at: DateTime<Utc>,
        content: String,
        completion_message: Option<String>,
        disposition: Option<String>,
        decision_ledger_ref: Option<String>,
        decision_ledger_digest: Option<String>,
        completed_while_crew_active: bool,
        forced_by: Option<PrincipalRef>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::DeliverPendingBrief {
            vessel,
            role,
            delivered_at,
            content,
            completion_message,
            disposition,
            decision_ledger_ref,
            decision_ledger_digest,
            completed_while_crew_active,
            forced_by,
        }
    }

    pub fn record_turn_delivery(
        source: String,
        episode: TurnDeliveryEpisode,
        vessel: String,
        role: String,
        prompt: String,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::RecordTurnDelivery { source, episode, vessel, role, prompt }
    }

    pub fn refuse_turn_delivery(source: String, episode: TurnDeliveryEpisode, attention: ConvoyAttention) -> ConvoyStatusPatch {
        ConvoyStatusPatch::RefuseTurnDelivery { source, episode, attention }
    }
}

#[cfg(test)]
mod subject_tests {
    use super::*;
    use crate::{CrewMessageDelivery, CrewMessageSender, TerminalCrewMessage};

    // #2684: confirmation is monotonic, and clearing one queued episode must
    // retain attention for another overdue episode and unrelated operator attention.
    #[hegel::test]
    fn queued_turn_observations_are_monotonic_and_preserve_attention(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let unrelated = tc.draw(gs::booleans());
        let steps = tc.draw(gs::integers::<usize>().min_value(2).max_value(12));
        let start = Utc::now();
        let now = start + chrono::Duration::seconds(301);
        let other = ConvoyAttention { source: "settlement".into(), reason: "keep this".into(), raised_at: start };
        let mut status = ConvoyStatus::default();
        for source in ["first", "second"] {
            status.turn_deliveries.insert(source.into(), TurnDeliveryStatus {
                episodes: vec![TurnDeliveryEpisode {
                    subject_revision: "head".into(),
                    evidence_at: start,
                    judged_claim_at: start,
                    outcome: TurnDeliveryOutcome::Queued {
                        rung: TurnDeliveryRung::WarmSession,
                        queued_at: start,
                        vessel: "work".into(),
                        role: "coder".into(),
                        message_id: source.into(),
                        blocking_reason: "Pending".into(),
                    },
                    sender: Default::default(),
                }],
                ..Default::default()
            });
        }
        status.attention = unrelated.then_some(other.clone());
        for age in [-1, 299, 300, 301] {
            let mut observed = status.clone();
            ConvoyStatusPatch::ObserveQueuedTurnDeliveries {
                observations: vec![QueuedTurnObservation {
                    source: "first".into(),
                    subject_revision: "head".into(),
                    confirmed: false,
                    blocking_reason: "Pending".into(),
                }],
                observed_at: start + chrono::Duration::seconds(age),
            }
            .apply(&mut observed);
            assert_eq!(observed.attention.is_some(), unrelated || age > 300);
        }
        let mut confirmed = [false; 2];
        // Explicit operation sequence includes repeated pending observations after
        // confirmation, both episode orders, and an absent revision (optimistic retries).
        for _ in 0..steps {
            let index = tc.draw(gs::integers::<usize>().min_value(0).max_value(1));
            let receipt = tc.draw(gs::booleans());
            let missing_revision = tc.draw(gs::booleans());
            let source = ["first", "second"][index];
            ConvoyStatusPatch::ObserveQueuedTurnDeliveries {
                observations: vec![QueuedTurnObservation {
                    source: source.into(),
                    subject_revision: if missing_revision { "other" } else { "head" }.into(),
                    confirmed: receipt,
                    blocking_reason: "attention Unobservable".into(),
                }],
                observed_at: now,
            }
            .apply(&mut status);
            confirmed[index] |= receipt && !missing_revision;
            for (index, source) in ["first", "second"].iter().enumerate() {
                assert_eq!(
                    matches!(status.turn_deliveries[*source].episodes[0].outcome, TurnDeliveryOutcome::Delivered { .. }),
                    confirmed[index]
                );
            }
            if unrelated {
                assert_eq!(status.attention, Some(other.clone()));
            } else {
                assert_eq!(status.attention.is_some(), !confirmed.iter().all(|confirmed| *confirmed));
                if let Some(attention) = &status.attention {
                    assert_eq!(attention.source, ConvoyAttention::QUEUED_TURN_SOURCE);
                    assert_eq!(attention.raised_at, now);
                }
            }
        }
        // Terminal convoys must not retain queued-turn NeedsYou attention. Clearing
        // the advisory must still preserve unrelated attention, for every terminal phase.
        for phase in [ConvoyPhase::Landed, ConvoyPhase::Failed, ConvoyPhase::Cancelled, ConvoyPhase::Abandoned] {
            let mut terminal = status.clone();
            terminal.phase = phase;
            terminal.turn_deliveries.get_mut("first").unwrap().episodes[0].outcome = TurnDeliveryOutcome::Queued {
                rung: TurnDeliveryRung::WarmSession,
                queued_at: start,
                vessel: "work".into(),
                role: "coder".into(),
                message_id: "first".into(),
                blocking_reason: "Pending".into(),
            };
            terminal.attention = Some(if unrelated {
                other.clone()
            } else {
                ConvoyAttention { source: ConvoyAttention::QUEUED_TURN_SOURCE.into(), reason: "overdue".into(), raised_at: now }
            });
            ConvoyStatusPatch::ObserveQueuedTurnDeliveries { observations: Vec::new(), observed_at: now }.apply(&mut terminal);
            assert_eq!(terminal.attention, unrelated.then_some(other.clone()));
            assert_eq!(terminal.phase, phase);
        }
    }

    // #2634: hook health is advisory; a settlement attention raised between
    // reading health and applying its patch must survive both raise and clear.
    #[hegel::test]
    fn turn_hook_health_preserves_other_attention(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Both raise/clear operations, duplicate patches, and observation times
        // before/after the existing attention cover optimistic retry interleavings.
        let raised_at = Utc::now();
        let observed_at = raised_at + chrono::Duration::seconds(tc.draw(gs::integers::<i64>().min_value(-1).max_value(1)));
        let reason = tc.draw(gs::booleans()).then(|| "no hook".to_string());
        let mut status = ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() };
        let health = ConvoyStatusPatch::ObserveTurnHookHealth { reason, observed_at };
        health.apply(&mut status);
        let settlement = ConvoyAttention { source: "settlement".into(), reason: "review pending".into(), raised_at };
        ConvoyStatusPatch::SetSettlementAttention { attention: Some(settlement.clone()) }.apply(&mut status);
        for _ in 0..2 {
            health.apply(&mut status);
            assert_eq!(status.attention, Some(settlement.clone()));
        }
    }

    // Glue: web roots are normalized consistently across configured Forge
    // and resolved Repository inputs. This pinned SSH installation makes the
    // transport URL's canonical root different from the public web root, so
    // using URL fallback instead of the resolved service_url cannot pass.
    #[test]
    fn convoy_reference_context_normalizes_and_reuses_public_web_root() {
        let forge = crate::ForgeSpec {
            forge_id: "lab".into(),
            kind: crate::ForgeKind::Forgejo,
            hosts: BTreeSet::from(["transport.example".into()]),
            https_url: "https://forge.example/install/".into(),
            git_ssh_host: "transport.example".into(),
        };
        let resolved = crate::RepositorySpec::remote("https://forge.example/install/team/repo")
            .expect("repository")
            .on_forge(&forge)
            .expect("forge repository");
        let snapshot = ConvoyRepositorySpec::builder()
            .repo_ref(resolved.key())
            .url("git@transport.example:install/team/repo.git".into())
            .source_ref("main".into())
            .target_ref("main".into())
            .workspace_slug("repo".into())
            .subpaths(Vec::new())
            .build();
        let configured =
            convoy_reference_context(std::slice::from_ref(&snapshot), None, None, std::slice::from_ref(&forge), |_| Some(&resolved));
        let from_repository = convoy_reference_context(std::slice::from_ref(&snapshot), None, None, &[], |_| Some(&resolved));
        // Stored records may predate constructor normalization. Both sources
        // must still yield the same normalized public root for those records.
        let simple_forge = crate::ForgeSpec { https_url: "https://forge.example/".into(), ..forge.clone() };
        let simple = crate::RepositorySpec::remote("https://forge.example/team/repo")
            .expect("repository")
            .on_forge(&simple_forge)
            .expect("forge repository");
        let mut stored = serde_json::to_value(&simple).expect("serialized repository");
        stored["forge"]["service_url"] = serde_json::json!("https://forge.example/");
        let legacy = serde_json::from_value::<crate::RepositorySpec>(stored).expect("legacy repository");
        let mut simple_snapshot = snapshot.clone();
        simple_snapshot.url = "git@transport.example:team/repo.git".into();
        let simple_configured =
            convoy_reference_context(std::slice::from_ref(&simple_snapshot), None, None, std::slice::from_ref(&simple_forge), |_| {
                Some(&simple)
            });
        let from_legacy = convoy_reference_context(std::slice::from_ref(&simple_snapshot), None, None, &[], |_| Some(&legacy));
        assert_eq!(simple_configured.repositories, from_legacy.repositories);
        assert_eq!(from_legacy.repositories[0].web_base, "https://forge.example");
        assert_eq!(configured.repositories.len(), 1);
        assert_eq!(configured.repositories, from_repository.repositories);
        assert_eq!(configured.repositories[0].web_base, "https://forge.example/install");
        let subject = configured.parse("https://forge.example/install/team/repo/pulls/7").expect("public PR URL");
        assert_eq!(subject.url(&from_repository).as_deref(), Some("https://forge.example/install/team/repo/pulls/7"));
    }

    // #2202: every caller builds the same source, alias, and URL context
    // whether it has a resolved Repository or only the admitted URL and Forge.
    #[hegel::test]
    fn convoy_reference_context_agrees_across_inputs(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Span GitHub and a named Forgejo installation, HTTPS/SSH aliases,
        // project aliases/default names, empty lists, duplicates and invalid URLs.
        let custom_forge = tc.draw(gs::booleans());
        let ssh = tc.draw(gs::booleans());
        let project_alias = tc.draw(gs::booleans());
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let invalid = tc.draw(gs::booleans());
        let forge = crate::ForgeSpec {
            forge_id: if custom_forge { "lab" } else { "github.com" }.into(),
            kind: if custom_forge { crate::ForgeKind::Forgejo } else { crate::ForgeKind::Github },
            hosts: BTreeSet::from(["git-alias.example".into()]),
            https_url: if custom_forge { "https://forge.example/install" } else { "https://github.com" }.into(),
            git_ssh_host: "git-alias.example".into(),
        };
        let url = if ssh { "git@git-alias.example:team/repo.git".to_string() } else { format!("{}/team/repo", forge.https_url) };
        // For an installation prefix, the SSH path includes that prefix too.
        let url = if ssh && custom_forge { "git@git-alias.example:install/team/repo.git".into() } else { url };
        let resolved = crate::RepositorySpec::remote(format!("{}/team/repo", forge.https_url))
            .expect("repository")
            .on_forge(&forge)
            .expect("forge repository");
        let snapshot = ConvoyRepositorySpec {
            repo_ref: resolved.key(),
            url,
            source_ref: "main".into(),
            target_ref: "main".into(),
            workspace_slug: "repo".into(),
            subpaths: Vec::new(),
        };
        let project = crate::ProjectSpec::builder()
            .display_name("Project".into())
            .default_workflow_ref("workflow".into())
            .repositories(vec![crate::ProjectRepositorySpec::builder()
                .repo(resolved.key())
                .maybe_alias(project_alias.then(|| "code".into()))
                .build()])
            .build();
        let mut snapshots = vec![snapshot; count];
        if invalid {
            snapshots.push(ConvoyRepositorySpec {
                repo_ref: RepositoryKey("invalid".into()),
                url: "invalid".into(),
                source_ref: "main".into(),
                target_ref: "main".into(),
                workspace_slug: "invalid".into(),
                subpaths: Vec::new(),
            });
        }
        let inferred = convoy_reference_context(&snapshots, Some("project"), Some(&project), std::slice::from_ref(&forge), |_| None);
        let from_records =
            convoy_reference_context(&snapshots, Some("project"), Some(&project), &[], |key| (*key == resolved.key()).then_some(&resolved));
        assert_eq!(inferred.repositories, from_records.repositories);
        assert_eq!(inferred.repositories.len(), count);
        for alias in &inferred.repositories {
            assert_eq!(alias.alias, if project_alias { "code" } else { "repo" });
            assert_eq!(alias.source.service, forge.forge_id);
            assert_eq!(alias.source.scope, "team/repo");
            assert_eq!(alias.web_base, forge.https_url);
            let url = format!("{}/team/repo/{}/7", forge.https_url, if custom_forge { "pulls" } else { "pull" });
            let subject = inferred.parse(&url).expect("parse forge PR");
            assert_eq!(subject.url(&inferred), Some(url));
        }
    }

    #[test]
    fn settlement_uses_every_active_change_request_subject() {
        let request = |scope: &str, id: &str| Subject {
            kind: flotilla_protocol::SubjectKind::ChangeRequest,
            source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: scope.into() },
            id: id.into(),
        };
        let first = request("flotilla-org/flotilla", "2372");
        let followup = request("flotilla-org/flotilla", "2394");
        let other_repo = request("flotilla-org/cleat", "281");
        let superseded = request("flotilla-org/flotilla", "2339");
        let mut status = ConvoyStatus::default();
        for subject in [&first, &followup, &other_repo, &superseded] {
            status.discover_subject(subject.clone(), Relationship::Produces, SubjectDiscoverySource::Claim, Utc::now());
        }
        status.discover_subject(superseded.clone(), Relationship::Supersedes, SubjectDiscoverySource::Operator, Utc::now());
        let convoy = ResourceObject::<Convoy> {
            metadata: crate::ObjectMeta {
                name: "multi-cr".into(),
                namespace: "default".into(),
                resource_version: "1".into(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: Utc::now(),
                merge: None,
            },
            spec: ConvoySpec::builder().workflow_ref("interactive".to_string()).build(),
            status: Some(status),
        };
        let addresses = expected_change_request_leaves(&convoy, &BTreeMap::new())
            .expect("subject leaves")
            .into_iter()
            .map(|leaf| leaf.address)
            .collect::<Vec<_>>();
        assert_eq!(addresses.len(), 6);
        for subject in [&first, &followup, &other_repo] {
            assert!(addresses.contains(&subject.leaf().expect("subject address")));
        }
        assert!(subject_relationship_conflicts(&convoy).is_empty(), "plural production is normal");
        let mut adopted = convoy.clone();
        adopted.spec.subjects.push(DeclaredSubject {
            subject: first.clone(),
            relationship: Relationship::Adopts,
            issue: None,
            change_request: None,
        });
        for source in
            [SubjectDiscoverySource::Branch, SubjectDiscoverySource::Claim, SubjectDiscoverySource::Relay, SubjectDiscoverySource::Operator]
        {
            let status = adopted.status.as_mut().expect("status");
            status.subjects.clear();
            status.discover_subject(first.clone(), Relationship::Produces, source, Utc::now());
            assert!(subject_relationship_conflicts(&adopted).is_empty(), "declared adoption permits production from {source:?}");
        }
        let mut conflicting = convoy.clone();
        conflicting.status.as_mut().expect("status").discover_subject(
            first.clone(),
            Relationship::Adopts,
            SubjectDiscoverySource::Operator,
            Utc::now(),
        );
        assert_eq!(subject_relationship_conflicts(&conflicting), vec![first]);
        let mut unlinked = convoy.clone();
        let status = unlinked.status.as_mut().expect("status");
        status.subjects.clear();
        status.crew_work.insert(
            "work".into(),
            BTreeMap::from([(
                "coder".into(),
                CrewWorkState::builder()
                    .phase(CrewWorkPhase::Done)
                    .message("https://github.com/flotilla-org/flotilla/pull/2372".to_string())
                    .build(),
            )]),
        );
        assert!(
            expected_change_request_leaves(&unlinked, &BTreeMap::new()).expect("subject leaves").is_empty(),
            "a claim message alone cannot bypass the persisted subject set"
        );
    }

    #[test]
    fn proposed_stall_disposition_round_trips_and_old_status_decodes() {
        let mut status = ConvoyStatus::default();
        let prior_generation = serde_json::to_value(&status).expect("serialize status");
        let decoded: ConvoyStatus = serde_json::from_value(prior_generation).expect("decode status without branch scan");
        assert!(decoded.branch_subject_scan_at.is_none());
        assert!(decoded.branch_subject_scan_error.is_none());
        status
            .crew_work
            .insert("work".into(), BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]));
        ConvoyStatusPatch::MarkCrewStalled {
            convoy: "job".into(),
            vessel: "work".into(),
            role: "coder".into(),
            at: Utc::now(),
            reason: StallReason::Scope,
            proposed_disposition: Some(StallProposedDisposition::ReduceScope),
            message: "ship the decoder first".into(),
        }
        .apply(&mut status);
        let written = serde_json::to_value(&status).expect("serialize stall");
        let restored: ConvoyStatus = serde_json::from_value(written.clone()).expect("decode stall");
        assert_eq!(restored.stalled.expect("stall").proposed_disposition, Some(StallProposedDisposition::ReduceScope));
        let mut previous = written;
        previous["stalled"].as_object_mut().expect("stall object").remove("proposed_disposition");
        let restored: ConvoyStatus = serde_json::from_value(previous).expect("decode prior-generation status");
        assert_eq!(restored.stalled.expect("stall").proposed_disposition, None);

        ConvoyStatusPatch::MarkCrewStalled {
            convoy: "job".into(),
            vessel: "work".into(),
            role: "coder".into(),
            at: Utc::now(),
            reason: StallReason::Decision,
            proposed_disposition: Some(StallProposedDisposition::Fail),
            message: "the brief is contradictory".into(),
        }
        .apply(&mut status);
        assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Stalled);
        assert_eq!(status.stalled.expect("stall").proposed_disposition, Some(StallProposedDisposition::Fail));
    }

    #[test]
    fn delivery_retry_preserves_original_and_unrelated_attention() {
        let now = Utc::now();
        let failure = TurnDeliveryFailure::builder()
            .reason("missing observation".into())
            .failed_at(now)
            .kind(TurnDeliveryFailureKind::Transient)
            .attempts(1)
            .retry_at(now + chrono::Duration::seconds(5))
            .build();
        let mut status = ConvoyStatus::default();
        ConvoyStatusPatch::FailTurnDelivery { source: "checks".into(), failure: failure.clone() }.apply(&mut status);
        let original = status.attention.clone();
        let retry = TurnDeliveryFailure { attempts: 2, failed_at: now + chrono::Duration::seconds(5), ..failure.clone() };
        ConvoyStatusPatch::FailTurnDelivery { source: "checks".into(), failure: retry.clone() }.apply(&mut status);
        assert_eq!(status.attention, original);
        assert_eq!(status.turn_deliveries["checks"].failure.as_ref().expect("delivery retry fixture").attempts, 2);
        let unrelated = ConvoyAttention { source: "settlement".into(), reason: "operator decision".into(), raised_at: now };
        status.attention = Some(unrelated.clone());
        ConvoyStatusPatch::FailTurnDelivery {
            source: "checks".into(),
            failure: TurnDeliveryFailure { reason: "new failure".into(), ..retry },
        }
        .apply(&mut status);
        assert_eq!(status.attention, Some(unrelated));
    }

    #[test]
    fn supervisor_turn_acknowledgment_clears_only_prior_generation_delivery_attention() {
        let old_attention = ConvoyAttention {
            source: "supervisor-turn-delivery".to_string(),
            reason: "Pending delivery".to_string(),
            raised_at: Utc::now(),
        };
        let mut status = ConvoyStatus {
            attention: Some(old_attention.clone()),
            turn_deliveries: BTreeMap::from([("turn-1".to_string(), TurnDeliveryStatus {
                pending_supervisor_turn: Some(PendingSupervisorTurn {
                    queued_order: 0,
                    vessel: "govern".to_string(),
                    role: "governor".to_string(),
                    message: TerminalCrewMessage {
                        id: "turn-1".to_string(),
                        text: "Supervise".to_string(),
                        sender: CrewMessageSender::FlotillaEscalation { from: "coder@work".to_string() },
                        delivery: CrewMessageDelivery::Queued,
                        acknowledged: Default::default(),
                        following: Vec::new(),
                    },
                }),
                ..Default::default()
            })]),
            ..Default::default()
        };
        ConvoyStatusPatch::AcknowledgeSupervisorTurn { message_id: "turn-1".to_string() }.apply(&mut status);
        assert!(status.attention.is_none());

        status.attention = Some(ConvoyAttention { source: "settlement".to_string(), ..old_attention });
        ConvoyStatusPatch::AcknowledgeSupervisorTurn { message_id: "turn-1".to_string() }.apply(&mut status);
        assert_eq!(status.attention.as_ref().map(|attention| attention.source.as_str()), Some("settlement"));
    }

    #[test]
    fn supervisor_turns_keep_enqueue_order_independent_of_message_ids() {
        let mut status = ConvoyStatus::default();
        for id in ["z-first", "a-second"] {
            ConvoyStatusPatch::QueueSupervisorTurn {
                turn: PendingSupervisorTurn {
                    vessel: "work".into(),
                    role: "coder".into(),
                    message: TerminalCrewMessage {
                        id: id.into(),
                        text: id.into(),
                        sender: CrewMessageSender::FlotillaNudge,
                        delivery: CrewMessageDelivery::Queued,
                        acknowledged: Default::default(),
                        following: Vec::new(),
                    },
                    queued_order: 0,
                },
            }
            .apply(&mut status);
        }
        assert_eq!(status.turn_deliveries["z-first"].pending_supervisor_turn.as_ref().expect("first").queued_order, 1);
        assert_eq!(status.turn_deliveries["a-second"].pending_supervisor_turn.as_ref().expect("second").queued_order, 2);
    }

    #[test]
    fn legacy_subject_fields_decode_and_write_only_the_new_set() {
        let old = serde_json::json!({
            "workflow_ref": "interactive",
            "repositories": [{
                "url": "https://github.com/flotilla-org/flotilla",
                "repo_ref": "github-flotilla",
                "source_ref": "main",
                "target_ref": "main",
                "workspace_slug": "flotilla"
            }],
            "issues": [{
                "reference": {"source": {"service": "github.com", "scope": "flotilla-org/flotilla"}, "id": "2182"},
                "snapshot": {"title": "Convoy subjects", "state": "open", "as_of": "2026-09-29T00:00:00Z"}
            }],
            "change_request": {"id": "2184", "repository_ref": "github-flotilla", "title": "ADR 49"}
        });
        let spec: ConvoySpec = serde_json::from_value(old).expect("old convoy spec decodes");
        assert_eq!(spec.subjects.len(), 2);
        assert!(spec.subjects.iter().any(|entry| entry.relationship == Relationship::WorksOn));
        assert!(spec.subjects.iter().any(|entry| entry.relationship == Relationship::Adopts));
        let written = serde_json::to_value(&spec).expect("serialize new shape");
        assert!(written.get("issues").is_none());
        assert!(written.get("change_request").is_none());
        let decoded: ConvoySpec = serde_json::from_value(written).expect("new convoy spec decodes");
        assert_eq!(decoded.issues, spec.issues);
        assert_eq!(decoded.change_request, spec.change_request);

        let opaque = serde_json::json!({
            "workflow_ref": "interactive",
            "issues": [{
                "reference": {"source": {"service": "tracker.example", "scope": "team/project"}, "id": "ABC-42"},
                "snapshot": {"title": "Opaque issue", "state": "open", "as_of": "2026-09-29T00:00:00Z"}
            }]
        });
        let spec: ConvoySpec = serde_json::from_value(opaque).expect("opaque prior-generation issue decodes");
        assert_eq!(spec.subjects[0].subject.id, "ABC-42");
        serde_json::to_value(spec).expect("opaque issue writes in new set");
    }

    #[test]
    fn discovery_refreshes_sources_and_unlink_suppresses_refresh() {
        let subject = Subject {
            kind: flotilla_protocol::SubjectKind::ChangeRequest,
            source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "flotilla-org/cleat".into() },
            id: "281".into(),
        };
        let now = Utc::now();
        let later = now + chrono::Duration::seconds(1);
        let mut status = ConvoyStatus::default();
        status.discover_subject(subject.clone(), Relationship::Produces, SubjectDiscoverySource::Branch, now);
        status.discover_subject(subject.clone(), Relationship::Produces, SubjectDiscoverySource::Branch, later);
        status.discover_subject(subject.clone(), Relationship::Produces, SubjectDiscoverySource::Claim, later);
        status.discover_subject(subject.clone(), Relationship::Produces, SubjectDiscoverySource::Relay, now);
        status.discover_subject(subject.clone(), Relationship::Produces, SubjectDiscoverySource::Relay, later);
        assert_eq!(status.subjects.len(), 1);
        assert_eq!(status.subjects[0].sources.len(), 3);
        assert_eq!(status.subjects[0].sources[0].at, later);
        assert_eq!(status.subjects[0].sources[2].source, SubjectDiscoverySource::Relay);
        assert_eq!(status.subjects[0].sources[2].at, later);
        status.unlink_subject(&subject);
        status.discover_subject(subject.clone(), Relationship::Produces, SubjectDiscoverySource::Branch, later);
        assert!(status.subjects.is_empty());
        status.discover_subject(subject, Relationship::Supersedes, SubjectDiscoverySource::Operator, later);
        assert_eq!(status.subjects.len(), 1);
        assert!(status.unlinked_subjects.is_empty());
    }
}
