//! Vessel reach at admission (#4299): each vessel's reach set is composed from
//! what it is given (its repositories' remotes, its credentials' servers and
//! the fleet services it uses) and what the fleet and project declare, then
//! frozen into its requirement. Placement refuses a candidate that cannot meet
//! a destination, naming it, rather than leaving a crew to time out in a clone.
use std::collections::{BTreeMap, BTreeSet};

use flotilla_environment::reach::{is_address, ReachResolver, Resolution};
use flotilla_resources::{
    reach::{self, PrivateReach, ReachDestination},
    ConvoyRepositorySpec, CredentialConsumer, CredentialSpec, FleetDesignation, Forge, Project, Repository, ResourceError,
    VesselRequirement, WorkflowTemplateSpec, FLEET_DESIGNATION_NAME,
};
use flotilla_store::ResourceBackend;

/// The declared levels and fleet services every vessel of a convoy shares.
struct SharedReach {
    fleet: Vec<ReachDestination>,
    project: Vec<ReachDestination>,
}

async fn shared_reach(backend: &ResourceBackend, namespace: &str, project_ref: Option<&str>) -> Result<SharedReach, String> {
    let mut fleet = Vec::new();
    match backend.definitions::<FleetDesignation>(namespace).get(FLEET_DESIGNATION_NAME).await {
        Ok(designation) => {
            fleet.extend(designation.spec.image_cache.as_ref().and_then(|cache| cache.registry_destination()));
            fleet.extend(designation.spec.reach);
        }
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

/// Where each destination resolves on this, the admitting, host.
pub async fn classify(resolver: &dyn ReachResolver, reach: &BTreeSet<ReachDestination>) -> Vec<(ReachDestination, Resolution)> {
    let mut classified = Vec::with_capacity(reach.len());
    for destination in reach {
        classified.push((destination.clone(), resolver.resolve(destination).await));
    }
    classified
}

/// Why a candidate whose private destinations are met by `private` cannot meet
/// the reach set, naming the first destination it cannot; `None` when it can.
///
/// Resolution happens on the admitting host, which may not be the candidate's
/// controlling host. A destination that does not resolve here is left to
/// provisioning, which resolves it where it is carried and names it if it fails.
pub fn unmet_reach(classified: &[(ReachDestination, Resolution)], private: PrivateReach) -> Option<String> {
    classified.iter().find_map(|(destination, resolution)| match resolution {
        Resolution::Private(_) => match private {
            PrivateReach::Unmet => Some(format!("cannot reach {destination}: it resolves to a private address the provider cannot carry")),
            PrivateReach::Exposure if is_address(destination) => {
                Some(format!("cannot reach {destination}: a private address is carried only under a name"))
            }
            PrivateReach::Exposure | PrivateReach::Native | PrivateReach::Undeclared => None,
        },
        Resolution::Public | Resolution::Unresolved(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(host: &str) -> ReachDestination {
        ReachDestination::new(host, 443).unwrap()
    }

    // #4299: a candidate that cannot carry a private destination is refused
    // with that destination named; one that carries it by exposure, or shares
    // the controlling host's network, is not. Public destinations and those
    // the admitting host cannot resolve never refuse a candidate.
    #[test]
    fn a_candidate_that_cannot_meet_a_private_destination_is_refused_naming_it() {
        let private = Resolution::Private("10.0.0.5".parse().unwrap());
        let classified = vec![
            (at("github.com"), Resolution::Public),
            (at("elsewhere.test"), Resolution::Unresolved("unknown".into())),
            (at("forgejo.lab.test"), private.clone()),
        ];
        let refusal = unmet_reach(&classified, PrivateReach::Unmet).expect("refused");
        assert!(refusal.contains("forgejo.lab.test:443"), "{refusal}");
        assert_eq!(unmet_reach(&classified, PrivateReach::Exposure), None);
        assert_eq!(unmet_reach(&classified, PrivateReach::Native), None);
        assert_eq!(unmet_reach(&classified, PrivateReach::Undeclared), None, "an advertisement that does not say is left to provisioning");
        assert_eq!(unmet_reach(&classified[..2], PrivateReach::Unmet), None);
        let address = [(at("10.0.0.5"), private)];
        assert!(unmet_reach(&address, PrivateReach::Exposure).is_some_and(|refusal| refusal.contains("10.0.0.5:443")));
        assert_eq!(unmet_reach(&address, PrivateReach::Native), None);
    }

    // #4299: a vessel's frozen reach set is the union of what it declares, its
    // repositories' remotes, its credentials' servers, the fleet's image cache
    // registry and the fleet's and project's declarations. A vessel scoped to
    // other repositories, with no credentials, keeps only the shared levels.
    #[tokio::test]
    async fn fleet_and_project_declarations_compose_with_what_the_vessel_is_given() {
        use flotilla_resources::{
            CredentialLifecycle, CredentialSource, CredentialSpecSpec, FleetDesignationSpec, ForgeKind, ForgeSpec, ImageCacheBinding,
            InputMeta, ProjectSpec, RepositorySpec,
        };
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let meta = |name: &str| InputMeta::builder().name(name.into()).build();
        let destination = |host: &str, port| ReachDestination::new(host, port).unwrap();
        let mut project = ProjectSpec::builder().display_name("app".into()).build();
        project.reach = vec![destination("objects.lab.test", 9000)];
        backend.using::<Project>("flotilla").create(&meta("app"), &project).await.unwrap();
        backend
            .definitions::<FleetDesignation>("flotilla")
            .create(
                &meta(FLEET_DESIGNATION_NAME),
                &FleetDesignationSpec {
                    fleet: Some("fleet".into()),
                    project: "app".into(),
                    image_cache: Some(ImageCacheBinding {
                        repository: "registry.lab.test:5000/cache".into(),
                        pull_credential: "pull".into(),
                        push_credential: "push".into(),
                    }),
                    reach: vec![destination("artifacts.lab.test", 443)],
                },
            )
            .await
            .unwrap();
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
                    https_url: "https://forgejo.lab.test".into(),
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
            .credential_refs(["github".to_string(), "forgejo".to_string()].into())
            .reach([destination("Declared.Test", 8443)].into())
            .build();
        let bare = VesselRequirement::builder().name("bare".into()).crew(Vec::new()).repository_refs(Vec::new()).build();
        let mut workflow = WorkflowTemplateSpec::builder().vessels(vec![given, bare]).build();
        freeze_workflow_reach(&backend, "flotilla", Some("app"), &repositories, &mut workflow).await.unwrap();
        let shared =
            [destination("artifacts.lab.test", 443), destination("objects.lab.test", 9000), destination("registry.lab.test", 5000)];
        let expected: BTreeSet<_> = shared
            .iter()
            .cloned()
            .chain([
                destination("declared.test", 8443),
                destination("github.com", 443),
                destination("api.github.com", 443),
                destination("forgejo.lab.test", 443),
            ])
            .collect();
        assert_eq!(workflow.vessels[0].reach, expected);
        assert_eq!(workflow.vessels[1].reach, shared.into_iter().collect());
        // Freezing again, as each admission check does, changes nothing.
        let frozen = workflow.clone();
        freeze_workflow_reach(&backend, "flotilla", Some("app"), &repositories, &mut workflow).await.unwrap();
        assert_eq!(workflow, frozen);
    }
}
