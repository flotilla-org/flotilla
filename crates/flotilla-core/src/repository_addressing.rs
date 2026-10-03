//! Repository addressing uses durable intent and observed checkout facts, never observation roots.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use flotilla_protocol::{RepoIdentity, RepoSelector};
use flotilla_resources::{Checkout, CheckoutPhase, CheckoutSpec, Environment, Project, Repository, RepositoryKey, ResourceBackend};

pub(crate) async fn resolve_repository(
    backend: &ResourceBackend,
    observed: &ResourceBackend,
    namespace: &str,
    local_host: &str,
    selector: &RepoSelector,
) -> Result<RepositoryKey, String> {
    let selector = match selector {
        RepoSelector::Identity(identity) if identity.authority == "local" => RepoSelector::Path(identity.path.clone().into()),
        selector => selector.clone(),
    };
    let label = match &selector {
        RepoSelector::Query(query) => query.clone(),
        RepoSelector::Path(path) => path.display().to_string(),
        RepoSelector::Repository(key) => key.to_string(),
        RepoSelector::Identity(identity) => identity.to_string(),
    };
    let repositories = backend.including_replicas::<Repository>(namespace).list().await.map_err(|error| error.to_string())?;
    let mut candidates = BTreeSet::new();
    match &selector {
        RepoSelector::Repository(key) => {
            candidates.insert(key.clone());
        }
        RepoSelector::Query(query) => {
            if query.is_empty() {
                return Err(
                    "repository context required: use --repo <project member alias or forge slug>, or run inside an observed checkout"
                        .into(),
                );
            }
            for source in &repositories.items {
                let repository = &source.object;
                if repository.metadata.name == *query || repository.spec.forge().is_some_and(|forge| forge.repository == *query) {
                    candidates.insert(RepositoryKey(repository.metadata.name.clone()));
                }
            }
            let projects = backend.definitions::<Project>(namespace).list().await.map_err(|error| error.to_string())?;
            for project in projects {
                for member in project.spec.repositories {
                    if member.alias.as_deref() == Some(query) {
                        candidates.insert(member.repo);
                    }
                }
            }
        }
        RepoSelector::Path(cwd) => {
            let mut checkouts = observed
                .clone()
                .using::<Checkout>(namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .map(|checkout| (checkout.metadata.name.clone(), checkout))
                .collect::<BTreeMap<_, _>>();
            // Match Aggregator precedence: durable status wins over its ephemeral projection.
            for source in backend.including_replicas::<Checkout>(namespace).list().await.map_err(|error| error.to_string())?.items {
                checkouts.insert(source.object.metadata.name.clone(), source.object);
            }
            let local_environments = backend
                .including_replicas::<Environment>(namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .filter(|source| source.object.spec.host_direct.as_ref().is_some_and(|direct| direct.host_ref == local_host))
                .map(|source| source.object.metadata.name)
                .collect::<BTreeSet<_>>();
            // Nested known checkouts scope to the deepest containing checkout.
            let mut deepest = 0;
            for checkout in checkouts.into_values() {
                if checkout.status.as_ref().is_some_and(|status| status.phase != CheckoutPhase::Ready) {
                    continue;
                }
                let (path, key) = match &checkout.spec {
                    CheckoutSpec::Observed(spec) if spec.host_ref == local_host => (spec.path.as_str(), &spec.repo_ref),
                    spec if spec.env_ref().is_some_and(|env| local_environments.contains(env)) => {
                        let Some(path) = checkout.status.as_ref().and_then(|status| status.path.as_deref()) else { continue };
                        (path, spec.repo_ref())
                    }
                    _ => continue,
                };
                let path = Path::new(path);
                if !path.is_absolute() || !cwd.starts_with(path) {
                    continue;
                }
                let depth = path.components().count();
                if depth > deepest {
                    candidates.clear();
                    deepest = depth;
                }
                if depth == deepest {
                    candidates.insert(key.clone());
                }
            }
        }
        RepoSelector::Identity(identity) => {
            for source in &repositories.items {
                if source.object.spec.remotes().iter().any(|remote| RepoIdentity::from_remote_url(remote).as_ref() == Some(identity)) {
                    candidates.insert(RepositoryKey(source.object.metadata.name.clone()));
                }
            }
        }
    }
    match candidates.len() {
        0 => Err(format!("no Repository matches '{label}'; adopt a checkout with `flotilla repo add <path>` or declare a Project member")),
        1 => {
            let key = candidates.into_iter().next().expect("one candidate");
            if repositories.items.iter().any(|source| source.object.metadata.name == key.0) {
                Ok(key)
            } else {
                Err(format!("Repository {key} is unavailable; adopt a checkout with `flotilla repo add <path>` or declare the Repository"))
            }
        }
        _ => Err(format!(
            "repository selector '{label}' is ambiguous: {}",
            candidates.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use flotilla_resources::{
        InMemoryBackend, InputMeta, ObservedCheckoutSpec, ProjectRepositoryRole, ProjectRepositorySpec, ProjectSpec, RepositorySpec,
    };
    use hegel::generators as gs;

    use super::*;

    fn backend() -> ResourceBackend {
        ResourceBackend::InMemory(InMemoryBackend::default())
    }

    async fn repository(backend: &ResourceBackend, name: &str, slug: &str) -> RepositoryKey {
        let spec = RepositorySpec::remote(format!("https://github.com/{slug}")).expect("repository spec");
        backend.using::<Repository>("test").create(&InputMeta::builder().name(name.to_string()).build(), &spec).await.expect("repository");
        RepositoryKey(name.into())
    }

    async fn project(backend: &ResourceBackend, name: &str, key: RepositoryKey, alias: &str) {
        let spec = ProjectSpec::builder()
            .display_name(name.to_string())
            .default_workflow_ref("single-agent".into())
            .repositories(vec![ProjectRepositorySpec::builder()
                .repo(key)
                .alias(alias.to_string())
                .roles([ProjectRepositoryRole::Code].into())
                .build()])
            .build();
        backend.using::<Project>("test").create(&InputMeta::builder().name(name.to_string()).build(), &spec).await.expect("project");
    }

    #[bon::builder]
    async fn checkout(backend: &ResourceBackend, name: &str, path: &str, key: RepositoryKey, #[builder(default = "local")] host: &str) {
        let spec = CheckoutSpec::Observed(
            ObservedCheckoutSpec::builder()
                .r#ref("main".into())
                .path(path.into())
                .repo_ref(key)
                .host_ref(host.into())
                .is_main(true)
                .build(),
        );
        backend.using::<Checkout>("test").create(&InputMeta::builder().name(name.to_string()).build(), &spec).await.expect("checkout");
    }

    // #1769: alias and forge slug name the same durable identity regardless of local presence.
    // Generator covers duplicate aliases for one identity, unrelated repositories, and all selector forms.
    #[hegel::test]
    fn addressing_is_independent_of_observation_roots(tc: hegel::TestCase) {
        let duplicates = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
        let extra = tc.draw(gs::booleans());
        let selector_kind = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let durable = backend();
            let observed = backend();
            let key = repository(&durable, "widgets", "acme/widgets").await;
            for i in 0..duplicates {
                project(&durable, &format!("project-{i}"), key.clone(), "primary").await;
            }
            if extra {
                repository(&durable, "other", "acme/other").await;
            }
            let selector = match selector_kind {
                0 => RepoSelector::Query("primary".into()),
                1 => RepoSelector::Query("acme/widgets".into()),
                2 => RepoSelector::Repository(key.clone()),
                _ => RepoSelector::Identity(RepoIdentity::from_remote_url("https://github.com/acme/widgets").expect("legacy identity")),
            };
            assert_eq!(resolve_repository(&durable, &observed, "test", "local", &selector).await, Ok(key));
        });
    }

    // #1769: cwd scopes to the deepest observed local checkout, including linked worktrees.
    // Generator crosses checkout boundaries and includes remote hosts with identical filesystem paths.
    #[hegel::test]
    fn cwd_inference_uses_checkout_facts(tc: hegel::TestCase) {
        let nested = tc.draw(gs::booleans());
        let legacy_local = tc.draw(gs::booleans());
        let depth = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let durable = backend();
            let observed = backend();
            let parent = repository(&durable, "widgets", "acme/widgets").await;
            let child = repository(&durable, "other", "acme/other").await;
            checkout().backend(&observed).name("parent").path("/work/widgets").key(parent.clone()).call().await;
            checkout().backend(&observed).name("remote").path("/work/widgets/nested").key(child.clone()).host("remote").call().await;
            if nested {
                checkout().backend(&observed).name("nested").path("/work/widgets/nested").key(child.clone()).call().await;
            }
            let cwd = format!("/work/widgets/nested{}", "/src".repeat(depth));
            let expected = if nested { child } else { parent };
            assert_eq!(
                resolve_repository(
                    &durable,
                    &observed,
                    "test",
                    "local",
                    &if legacy_local {
                        RepoSelector::Identity(RepoIdentity { authority: "local".into(), path: cwd })
                    } else {
                        RepoSelector::Path(cwd.into())
                    }
                )
                .await,
                Ok(expected)
            );
            assert!(resolve_repository(&durable, &observed, "test", "local", &RepoSelector::Path("/work/widgets-other".into()))
                .await
                .is_err());
        });
    }

    // Unknown, empty, ambiguous, and dangling selectors refuse without adopting or minting anything.
    #[tokio::test]
    async fn invalid_selectors_are_read_only_refusals() {
        let durable = backend();
        let observed = backend();
        let first = repository(&durable, "widgets", "acme/widgets").await;
        let second = repository(&durable, "other", "acme/other").await;
        project(&durable, "a", first, "primary").await;
        project(&durable, "b", second, "primary").await;
        project(&durable, "dangling", RepositoryKey("absent".into()), "missing").await;
        for query in ["", "unknown", "primary", "missing", "/work/widgets", "widgets-other"] {
            let error =
                resolve_repository(&durable, &observed, "test", "local", &RepoSelector::Query(query.into())).await.expect_err("refusal");
            assert!(!error.contains("tracked"));
        }
        assert!(resolve_repository(&durable, &observed, "test", "local", &RepoSelector::Repository(RepositoryKey("absent".into())))
            .await
            .is_err());
        assert_eq!(durable.using::<Repository>("test").list().await.expect("repositories").items.len(), 2);
        assert!(observed.using::<Checkout>("test").list().await.expect("checkouts").items.is_empty());
    }
    // Controller-created checkout status is a presence fact; a desired target path
    // alone, or a checkout in another host/environment, must never scope the CLI.
    #[tokio::test]
    async fn cwd_uses_ready_local_checkout_status_and_durable_precedence() {
        use flotilla_resources::{CheckoutStatus, EnvironmentSpec, FreshCloneCheckoutSpec, HostDirectEnvironmentSpec};

        let durable = backend();
        let observed = backend();
        let key = repository(&durable, "widgets", "acme/widgets").await;
        let environment = EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "local".into(), repo_default_dir: "/work".into() }),
            docker: None,
        };
        durable
            .using::<Environment>("test")
            .create(&InputMeta::builder().name("host-direct".into()).build(), &environment)
            .await
            .expect("environment");
        let spec = CheckoutSpec::FreshClone(
            FreshCloneCheckoutSpec::builder()
                .repo_ref(key.clone())
                .env_ref("host-direct".into())
                .r#ref("main".into())
                .target_path("/desired/widgets".into())
                .url("https://github.com/acme/widgets".into())
                .build(),
        );
        let checkouts = durable.using::<Checkout>("test");
        let current = checkouts.create(&InputMeta::builder().name("created".into()).build(), &spec).await.expect("checkout");
        assert!(resolve_repository(&durable, &observed, "test", "local", &RepoSelector::Path("/desired/widgets".into())).await.is_err());
        let current = checkouts
            .update_status(
                "created",
                &current.metadata.resource_version,
                &CheckoutStatus::builder().phase(CheckoutPhase::Ready).path("/actual/widgets".into()).build(),
            )
            .await
            .expect("ready checkout");
        assert_eq!(
            resolve_repository(&durable, &observed, "test", "local", &RepoSelector::Path("/actual/widgets/src".into())).await,
            Ok(key.clone())
        );
        assert!(resolve_repository(&durable, &observed, "test", "remote", &RepoSelector::Path("/actual/widgets/src".into()))
            .await
            .is_err());
        // A stale ephemeral record cannot resurrect a durable Gone checkout.
        checkout().backend(&observed).name("created").path("/actual/widgets").key(key).call().await;
        checkouts
            .update_status(
                "created",
                &current.metadata.resource_version,
                &CheckoutStatus::builder().phase(CheckoutPhase::Gone).path("/actual/widgets".into()).build(),
            )
            .await
            .expect("gone checkout");
        assert!(resolve_repository(&durable, &observed, "test", "local", &RepoSelector::Path("/actual/widgets".into())).await.is_err());
    }
}
