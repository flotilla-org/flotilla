//! Vessel reach at admission (#4299): each vessel's reach set is composed from
//! what it is given (its repositories' remotes and its credentials' servers)
//! and what the fleet and project declare, then frozen into its requirement.
//! At placement each candidate answers, per destination, how it would meet it
//! or that it cannot; admission looks nothing up. A destination a candidate
//! cannot meet, or gives no answer for, refuses it, naming the destination,
//! rather than leaving a crew to time out in a clone.
use std::collections::{BTreeMap, BTreeSet};

use flotilla_environment::reach::{PlannedRoute, ReachAnswer};
use flotilla_resources::{
    reach::{self, ReachDestination, ReachMeans, ReachRoute, ReachWay},
    ConvoyRepositorySpec, CredentialConsumer, CredentialSpec, FleetDesignation, Forge, Project, Repository, ResourceError,
    VesselRequirement, WorkflowTemplateSpec, FLEET_DESIGNATION_NAME,
};
use flotilla_store::ResourceBackend;

/// The declared levels every vessel of a convoy shares.
struct SharedReach {
    fleet: Vec<ReachDestination>,
    project: Vec<ReachDestination>,
}

async fn shared_reach(backend: &ResourceBackend, namespace: &str, project_ref: Option<&str>) -> Result<SharedReach, String> {
    let mut fleet = Vec::new();
    match backend.definitions::<FleetDesignation>(namespace).get(FLEET_DESIGNATION_NAME).await {
        Ok(designation) => fleet.extend(designation.spec.reach),
        Err(ResourceError::NotFound { .. }) => {}
        Err(error) => return Err(format!("read fleet designation for reach: {error}")),
    }
    let project = match project_ref {
        Some(name) => match backend.including_replicas::<Project>(namespace).get(name).await {
            Ok(project) => project.object.spec.reach,
            Err(ResourceError::NotFound { .. }) => Vec::new(),
            Err(error) => return Err(format!("read project {name} for reach: {error}")),
        },
        None => Vec::new(),
    };
    Ok(SharedReach { fleet, project })
}

/// One vessel's destinations derived from what it is given: its repositories'
/// remotes and the servers its credentials name.
async fn derived_reach(
    backend: &ResourceBackend,
    namespace: &str,
    repositories: &[ConvoyRepositorySpec],
    vessel: &VesselRequirement,
    forges: &mut BTreeMap<String, Option<String>>,
) -> Result<BTreeSet<ReachDestination>, String> {
    let mut derived = BTreeSet::new();
    let scoped =
        |repository: &&ConvoyRepositorySpec| vessel.repository_refs.as_ref().is_none_or(|refs| refs.contains(&repository.repo_ref));
    for repository in repositories.iter().filter(scoped) {
        derived.extend(reach::url_destination(&repository.url));
        match backend.including_replicas::<Repository>(namespace).get(&repository.repo_ref.to_string()).await {
            Ok(record) => derived.extend(record.object.spec.remotes().iter().filter_map(|remote| reach::url_destination(remote))),
            Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(format!("read repository {} for reach: {error}", repository.repo_ref)),
        }
    }
    for name in &vessel.credential_refs {
        let consumer = match backend.definitions::<CredentialSpec>(namespace).get(name).await {
            Ok(credential) => credential.spec.consumer,
            // Credential validation names a missing declaration.
            Err(ResourceError::NotFound { .. }) => continue,
            Err(error) => return Err(format!("read credential {name} for reach: {error}")),
        };
        let forge_url = match &consumer {
            CredentialConsumer::Forgejo { forge_ref, .. } => {
                if !forges.contains_key(forge_ref) {
                    let url = match backend.definitions::<Forge>(namespace).get(forge_ref).await {
                        Ok(forge) => Some(forge.spec.https_url),
                        Err(ResourceError::NotFound { .. }) => None,
                        Err(error) => return Err(format!("read forge {forge_ref} for reach: {error}")),
                    };
                    forges.insert(forge_ref.clone(), url);
                }
                forges[forge_ref].clone()
            }
            _ => None,
        };
        derived.extend(reach::credential_destinations(&consumer, forge_url.as_deref()));
    }
    Ok(derived)
}

/// Compose and freeze each vessel's reach set into its requirement: what it
/// declares, what it is given, and the fleet's and project's declarations.
/// Credentials must already be resolved into the requirements.
pub async fn freeze_workflow_reach(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    repositories: &[ConvoyRepositorySpec],
    workflow: &mut WorkflowTemplateSpec,
) -> Result<(), String> {
    if workflow.vessels.is_empty() {
        return Ok(());
    }
    let shared = shared_reach(backend, namespace, project_ref).await?;
    let mut forges = BTreeMap::new();
    for vessel in &mut workflow.vessels {
        let derived = derived_reach(backend, namespace, repositories, vessel, &mut forges).await?;
        let declared = std::mem::take(&mut vessel.reach).into_iter().collect::<Vec<_>>();
        vessel.reach = reach::compose(derived, [declared.as_slice(), shared.fleet.as_slice(), shared.project.as_slice()]);
    }
    Ok(())
}

/// A host's provider instances answering, on that host, how each one's
/// vessels would meet a reach set (#4299).
#[async_trait::async_trait]
pub trait ReachAnswers: Send + Sync {
    /// The instance's answers, or `None` when it does not run here.
    async fn answer(&self, instance: &str, reach: &BTreeSet<ReachDestination>) -> Option<Vec<ReachAnswer>>;
}

/// Answers for no instance.
pub struct NoReachAnswers;

#[async_trait::async_trait]
impl ReachAnswers for NoReachAnswers {
    async fn answer(&self, _instance: &str, _reach: &BTreeSet<ReachDestination>) -> Option<Vec<ReachAnswer>> {
        None
    }
}

#[async_trait::async_trait]
impl ReachAnswers for flotilla_discovery::registry::ProviderRegistry {
    async fn answer(&self, instance: &str, reach: &BTreeSet<ReachDestination>) -> Option<Vec<ReachAnswer>> {
        let (_, provider) = self.environment_providers.select(instance)?;
        Some(provider.answer_reach(reach).await)
    }
}

/// The answers a candidate's advertised ways give without looking anything
/// up: every destination on the host's network when that is its preferred
/// way, and none otherwise.
pub fn advertised_answers(ways: &[ReachWay], reach: &BTreeSet<ReachDestination>) -> Vec<ReachAnswer> {
    if ways.first() != Some(&ReachWay::HostNetwork) {
        return Vec::new();
    }
    let met = |destination: &ReachDestination| PlannedRoute {
        route: ReachRoute { destination: destination.clone(), met: ReachMeans::HostNetwork },
        carried_to: None,
    };
    reach.iter().map(|destination| (destination.clone(), Ok(met(destination)))).collect()
}

/// Why a candidate that gave `answers` cannot meet the reach set, naming the
/// first destination it cannot meet or gave no answer for; `None` when it
/// meets every one.
pub fn unmet_reach(reach: &BTreeSet<ReachDestination>, answers: &[ReachAnswer]) -> Option<String> {
    reach.iter().find_map(|destination| match answers.iter().find(|(answered, _)| answered == destination) {
        Some((_, Ok(_))) => None,
        Some((_, Err(reason))) => Some(format!("reach destination {reason}")),
        None => Some(format!("gives no answer for reach destination {destination}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(host: &str) -> ReachDestination {
        ReachDestination::new(host, 443).unwrap()
    }

    // #4299: a candidate is refused at the first destination it cannot meet,
    // in its own words, or gives no answer for; one that meets every
    // destination, however it meets each, is not. A candidate that prefers the
    // host's network answers from its advertisement alone.
    #[test]
    fn a_candidate_is_refused_at_a_destination_it_cannot_meet_or_does_not_answer() {
        let met = |host: &str, met| (at(host), Ok(PlannedRoute { route: ReachRoute { destination: at(host), met }, carried_to: None }));
        let reach = BTreeSet::from([at("forgejo.lab.test"), at("github.com")]);
        let answers = vec![met("github.com", ReachMeans::Egress), met("forgejo.lab.test", ReachMeans::OutwardExposure)];
        assert_eq!(unmet_reach(&reach, &answers), None);
        let cannot = vec![met("github.com", ReachMeans::Egress), (at("forgejo.lab.test"), Err("forgejo.lab.test:443 is private".into()))];
        assert_eq!(unmet_reach(&reach, &cannot).as_deref(), Some("reach destination forgejo.lab.test:443 is private"));
        let unanswered = unmet_reach(&reach, &answers[..1]).expect("refused");
        assert!(unanswered.contains("no answer") && unanswered.contains("forgejo.lab.test:443"), "{unanswered}");
        assert_eq!(unmet_reach(&BTreeSet::new(), &[]), None);
        // A candidate preferring the host's network answers from its advertisement.
        assert_eq!(unmet_reach(&reach, &advertised_answers(&[ReachWay::HostNetwork, ReachWay::OutwardExposure], &reach)), None);
        assert!(advertised_answers(&[ReachWay::OutwardExposure, ReachWay::HostNetwork], &reach).is_empty());
        assert!(advertised_answers(&[], &reach).is_empty());
    }

    // #4299: a vessel's credentials name the servers it must reach, and only
    // the repositories it is scoped to add their remotes; freezing again, as
    // each admission check does, changes nothing. Composition with the fleet's
    // and project's declarations is a daemon scenario (`vessel_reach`); this
    // one stays here because a host holds credentials only through its own
    // store, which a scenario cannot reach.
    #[tokio::test]
    async fn credentials_name_their_servers_and_only_scoped_repositories_add_remotes() {
        use flotilla_resources::{
            CredentialLifecycle, CredentialSource, CredentialSpecSpec, ForgeKind, ForgeSpec, InputMeta, RepositorySpec,
        };
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let meta = |name: &str| InputMeta::builder().name(name.into()).build();
        let destination = |host: &str, port| ReachDestination::new(host, port).unwrap();
        let repository = RepositorySpec::remote("https://forgejo.lab.test/lab/app").unwrap();
        backend.using::<Repository>("flotilla").create(&meta(&repository.key().to_string()), &repository).await.unwrap();
        backend
            .definitions::<Forge>("flotilla")
            .create(
                &meta("lab"),
                &ForgeSpec {
                    forge_id: "lab".into(),
                    kind: ForgeKind::Forgejo,
                    hosts: ["forgejo.lab.test".to_string()].into(),
                    https_url: "https://forgejo.lab.test:3000".into(),
                    git_ssh_host: "forgejo.lab.test".into(),
                },
            )
            .await
            .unwrap();
        for (name, consumer) in [
            ("github", CredentialConsumer::Gh),
            ("forgejo", CredentialConsumer::Forgejo { forge_ref: "lab".into(), username: "crew".into() }),
        ] {
            let spec = CredentialSpecSpec::builder()
                .consumer(consumer)
                .source(CredentialSource::Env { name: "TOKEN".into() })
                .lifecycle(CredentialLifecycle::Static)
                .build();
            backend.definitions::<CredentialSpec>("flotilla").create(&meta(name), &spec).await.unwrap();
        }
        let repositories = vec![ConvoyRepositorySpec {
            url: "https://github.com/example/app".into(),
            repo_ref: repository.key(),
            source_ref: "main".into(),
            target_ref: "main".into(),
            workspace_slug: "app".into(),
            subpaths: Vec::new(),
        }];
        let given = VesselRequirement::builder()
            .name("work".into())
            .crew(Vec::new())
            .credential_refs(["github".to_string(), "forgejo".to_string(), "undeclared".to_string()].into())
            .build();
        let bare = VesselRequirement::builder().name("bare".into()).crew(Vec::new()).repository_refs(Vec::new()).build();
        let mut workflow = WorkflowTemplateSpec::builder().vessels(vec![given, bare]).build();
        freeze_workflow_reach(&backend, "flotilla", None, &repositories, &mut workflow).await.unwrap();
        let expected = BTreeSet::from([
            destination("github.com", 443),
            destination("api.github.com", 443),
            destination("forgejo.lab.test", 443),
            destination("forgejo.lab.test", 3000),
        ]);
        assert_eq!(workflow.vessels[0].reach, expected);
        assert!(workflow.vessels[1].reach.is_empty(), "{:?}", workflow.vessels[1].reach);
        let frozen = workflow.clone();
        freeze_workflow_reach(&backend, "flotilla", None, &repositories, &mut workflow).await.unwrap();
        assert_eq!(workflow, frozen);
    }
}
