//! Repository addressing uses durable intent and observed checkout facts, never observation roots.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use flotilla_protocol::{RepoIdentity, RepoSelector};
use flotilla_resources::checkout_records::Launches;
use flotilla_resources::discovered_checkouts::DiscoveredCheckouts;
use flotilla_resources::lifecycle_reads::LifecycleReads;
use flotilla_resources::{Checkout, CheckoutPhase, CheckoutSpec, Project, Repository, RepositoryKey};
use flotilla_store::ResourceBackend;

use flotilla_paths::path_context::canonical_or_original;

pub async fn resolve_repository_with_lifecycle_reads(
    backend: &ResourceBackend,
    discovered: &DiscoveredCheckouts,
    namespace: &str,
    local_host: &str,
    selector: &RepoSelector,
    reads: &dyn LifecycleReads,
) -> Result<Option<RepositoryKey>, String> {
    let selector = match selector {
        // Legacy TUI/provider callers encode a local checkout root in RepoIdentity.
        // Resolve it only through checkout facts. Remove when the Plane-A callers
        // all carry Repository keys (the path-identity retirement slice of #1721).
        RepoSelector::Identity(identity) if identity.authority == "local" => RepoSelector::Path(identity.path.clone().into()),
        RepoSelector::Identity(identity) if identity.authority == "repository" => {
            RepoSelector::Repository(RepositoryKey(identity.path.clone()))
        }
        selector => selector.clone(),
    };
    let label = selector.to_string();
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
            let cwd = canonical_or_original(cwd);
            let checkouts = local_checkouts_with_lifecycle_reads(discovered, namespace, local_host, reads).await?;
            // Nested known checkouts scope to the deepest containing checkout.
            let mut deepest = 0;
            for checkout in checkouts {
                let (path, key) = match &checkout.spec {
                    CheckoutSpec::Observed(spec) => (spec.path.as_str(), &spec.repo_ref),
                    spec => {
                        (checkout.status.as_ref().and_then(|status| status.path.as_deref()).expect("local checkout path"), spec.repo_ref())
                    }
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
        0 => Ok(None),
        1 => {
            let key = candidates.into_iter().next().expect("one candidate");
            if repositories.items.iter().any(|source| source.object.metadata.name == key.0) {
                Ok(Some(key))
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

/// Ready checkouts on the local host: the engine's ready checkouts held here whose environment
/// runs directly on this host, and the checkouts this host's refresh discovered. An engine checkout
/// of a name, ready or not, wins over the discovery of it.
pub async fn local_checkouts_with_lifecycle_reads(
    discovered: &DiscoveredCheckouts,
    namespace: &str,
    local_host: &str,
    reads: &dyn LifecycleReads,
) -> Result<Vec<flotilla_resources::ResourceObject<Checkout>>, String> {
    let mut checkouts =
        discovered.views(namespace).into_iter().map(|checkout| (checkout.metadata.name.clone(), checkout)).collect::<BTreeMap<_, _>>();
    let held = flotilla_resources::checkout_records::latest_launches(reads.checkouts()?.launches(Launches::Here)?);
    // The engine's checkout of a name, in any state, wins over the observation of it: an ended
    // checkout's stale observation cannot resurrect it.
    for name in held.keys() {
        checkouts.remove(name);
    }
    let ready = held.into_values().filter(|record| record.is_ready()).collect::<Vec<_>>();
    // The environments those checkouts stand on that run directly on this host.
    let mut local_environments = BTreeSet::new();
    let standing_on = ready.iter().filter_map(|record| record.spec.env_ref()).collect::<BTreeSet<_>>();
    if !standing_on.is_empty() {
        let environments = reads.environments()?;
        for name in standing_on {
            if let Some(environment) = environments.find(name)? {
                if is_local_direct_environment(&environment.spec, local_host)? {
                    local_environments.insert(name.to_owned());
                }
            }
        }
    }
    for record in ready {
        let local = match &record.spec {
            CheckoutSpec::Observed(spec) => spec.host_ref == local_host,
            spec => spec.env_ref().is_some_and(|env| local_environments.contains(env)),
        };
        if local {
            let mut view = record.read_view();
            view.metadata.namespace = namespace.to_owned();
            checkouts.insert(view.metadata.name.clone(), view);
        }
    }
    Ok(checkouts
        .into_values()
        .filter(|checkout| {
            if checkout.status.as_ref().is_some_and(|status| status.phase != CheckoutPhase::Ready) {
                return false;
            }
            match &checkout.spec {
                CheckoutSpec::Observed(spec) => spec.host_ref == local_host && Path::new(&spec.path).is_absolute(),
                _ => checkout.status.as_ref().and_then(|status| status.path.as_deref()).is_some_and(|path| Path::new(path).is_absolute()),
            }
        })
        .map(|mut checkout| {
            // Only local facts may be resolved against this host's filesystem.
            if let CheckoutSpec::Observed(spec) = &mut checkout.spec {
                spec.path = canonical_or_original(Path::new(&spec.path)).to_string_lossy().into_owned();
            }
            if let Some(path) = checkout.status.as_mut().and_then(|status| status.path.as_mut()) {
                *path = canonical_or_original(Path::new(path)).to_string_lossy().into_owned();
            }
            checkout
        })
        .collect())
}

/// Whether `spec` is a direct environment of `local_host`: one its provider address places on
/// the host, or, with no address, whose host reference names it.
fn is_local_direct_environment(spec: &flotilla_resources::EnvironmentSpec, local_host: &str) -> Result<bool, String> {
    if spec.host_direct.is_none() {
        return Ok(false);
    }
    match &spec.provider_address {
        Some(address) => {
            let host_address =
                flotilla_address::Address::root(flotilla_address::Kind::Host, local_host).map_err(|error| error.to_string())?;
            Ok(address.to_string().starts_with(&format!("{host_address}/provider:")))
        }
        None => Ok(spec.host_direct.as_ref().is_some_and(|direct| direct.host_ref == local_host)),
    }
}

#[cfg(test)]
mod tests {
    use flotilla_provisioning_testkit::Ending;
    use flotilla_resources::discovered_checkouts::DiscoveredCheckout;
    use flotilla_resources::{InputMeta, ObservedCheckoutSpec, ProjectRepositoryRole, ProjectRepositorySpec, ProjectSpec, RepositorySpec};
    use flotilla_store_memory::InMemoryBackend;
    use hegel::generators as gs;

    use super::*;

    // WORKLOAD.md: admission must select only this host's direct environments,
    // independently of the number of unrelated per-vessel environments.
    #[hegel::test]
    fn direct_environment_query_covers_only_local_current_and_legacy_records(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Escaped/case-sensitive hosts, both stored generations, missing direct
        // inputs, and a conflicting legacy host alongside an explicit address.
        let host = tc.draw(gs::text().alphabet("aZ09 /:%é").min_size(1).max_size(16));
        let address = flotilla_address::Address::root(flotilla_address::Kind::Host, &host).expect("host address");
        let mut selected = BTreeSet::new();
        let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(24));
        let mut expected = BTreeSet::new();
        for row in 0..count {
            let local = tc.draw(gs::booleans());
            let legacy = tc.draw(gs::booleans());
            let direct = tc.draw(gs::booleans());
            let target = if local { host.clone() } else { format!("{host}-other") };
            let mut spec = serde_json::json!({});
            if !legacy {
                let place = flotilla_address::Address::root(flotilla_address::Kind::Host, &target).expect("place");
                spec["provider_address"] = format!("{place}/provider:custom").into();
            }
            if direct {
                spec["host_direct"] =
                    serde_json::json!({"host_ref": if legacy { target.clone() } else { host.clone() }, "repo_default_dir":"/work"});
            } else {
                spec["docker"] = serde_json::json!({"host_ref": target, "image":"image"});
            }
            let name = format!("environment-{row}");
            let spec = serde_json::from_value::<flotilla_resources::EnvironmentSpec>(spec).expect("environment spec");
            if is_local_direct_environment(&spec, &host).expect("predicate") {
                selected.insert(name.clone());
            }
            if local && direct {
                expected.insert(name);
            }
            assert_eq!(selected, expected, "candidate cover after every insert at {address}");
        }
    }

    fn backend() -> ResourceBackend {
        ResourceBackend::new(InMemoryBackend::default())
    }

    /// Resolves `selector` with no checkout held on the engine.
    async fn resolve_repository(
        durable: &ResourceBackend,
        observed: &DiscoveredCheckouts,
        namespace: &str,
        local_host: &str,
        selector: &RepoSelector,
    ) -> Result<Option<RepositoryKey>, String> {
        resolve_repository_with_lifecycle_reads(
            durable,
            observed,
            namespace,
            local_host,
            selector,
            crate::test_support::no_held_checkouts().as_ref(),
        )
        .await
    }

    /// The engine's checkouts, served with its environments: `host-direct` runs directly on `host`.
    fn engine_on(host: &str) -> flotilla_provisioning_testkit::CheckoutFixture {
        use flotilla_resources::{EnvironmentSpec, HostDirectEnvironmentSpec};
        use std::sync::Arc;
        let store = flotilla_provisioning_testkit::CheckoutFixture::default().store;
        let environments = flotilla_provisioning_testkit::EnvironmentFixture::over("home", Arc::clone(&store));
        let clones = flotilla_provisioning_testkit::CloneFixture::over("home", Arc::clone(&store));
        environments.request(
            "host-direct",
            &EnvironmentSpec {
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: host.into(), repo_default_dir: "/work".into() }),
                ..Default::default()
            },
            None,
        );
        flotilla_provisioning_testkit::CheckoutFixture::over("home", store)
            .serving(Arc::clone(&environments.environments) as _, Arc::clone(&clones.clones) as _)
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

    /// Discovers checkout `name` of `key` at `path` on `host`, beside the others discovered.
    #[bon::builder]
    fn checkout(discovered: &DiscoveredCheckouts, name: &str, path: &str, key: RepositoryKey, #[builder(default = "local")] host: &str) {
        let mut found = discovered.all().into_iter().filter(|checkout| checkout.spec.repo_ref == key).collect::<Vec<_>>();
        found.push(DiscoveredCheckout {
            name: name.into(),
            repo_slug: key.to_string(),
            spec: ObservedCheckoutSpec::builder()
                .r#ref("main".into())
                .path(path.into())
                .repo_ref(key.clone())
                .host_ref(host.into())
                .is_main(true)
                .build(),
        });
        discovered.replace(&key, found);
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
            let observed = DiscoveredCheckouts::default();
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
            assert_eq!(resolve_repository(&durable, &observed, "test", "local", &selector).await, Ok(Some(key)));
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
            let observed = DiscoveredCheckouts::default();
            let parent = repository(&durable, "widgets", "acme/widgets").await;
            let child = repository(&durable, "other", "acme/other").await;
            checkout().discovered(&observed).name("parent").path("/work/widgets").key(parent.clone()).call();
            checkout().discovered(&observed).name("remote").path("/work/widgets/nested").key(child.clone()).host("remote").call();
            if nested {
                checkout().discovered(&observed).name("nested").path("/work/widgets/nested").key(child.clone()).call();
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
                Ok(Some(expected))
            );
            assert_eq!(
                resolve_repository(&durable, &observed, "test", "local", &RepoSelector::Path("/work/widgets-other".into())).await,
                Ok(None)
            );
        });
    }

    // #2570: local checkout facts and selectors agree through directory symlinks,
    // including an absent suffix. Generate both stored spellings and suffix depths.
    #[cfg(unix)]
    #[hegel::test]
    fn symlinked_cwd_resolves_physical_checkout(tc: hegel::TestCase) {
        let stored_alias = tc.draw(gs::booleans());
        let depth = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let temp = tempfile::tempdir().expect("fixture");
        let physical = temp.path().join("physical");
        std::fs::create_dir(&physical).expect("physical directory");
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&physical, &alias).expect("symlink");
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let durable = backend();
            let observed = DiscoveredCheckouts::default();
            let key = repository(&durable, "widgets", "acme/widgets").await;
            let stored = if stored_alias { &alias } else { &physical };
            checkout().discovered(&observed).name("checkout").path(stored.to_str().expect("path")).key(key.clone()).call();
            let mut cwd = alias.clone();
            for _ in 0..depth {
                cwd.push("missing");
            }
            assert_eq!(resolve_repository(&durable, &observed, "test", "local", &RepoSelector::Path(cwd)).await, Ok(Some(key.clone())));
            assert_eq!(resolve_repository(&durable, &observed, "test", "local", &RepoSelector::Path(physical)).await, Ok(Some(key)));
            // Component boundaries still prevent sibling-prefix matches.
            assert_eq!(
                resolve_repository(&durable, &observed, "test", "local", &RepoSelector::Path(temp.path().join("physical-other"))).await,
                Ok(None)
            );
        });
    }

    // Unknown, empty, ambiguous, and dangling selectors refuse without adopting or minting anything.
    #[tokio::test]
    async fn invalid_selectors_are_read_only_refusals() {
        let durable = backend();
        let observed = DiscoveredCheckouts::default();
        let first = repository(&durable, "widgets", "acme/widgets").await;
        let second = repository(&durable, "other", "acme/other").await;
        project(&durable, "a", first, "primary").await;
        project(&durable, "b", second, "primary").await;
        project(&durable, "dangling", RepositoryKey("absent".into()), "missing").await;
        for query in ["", "unknown", "primary", "missing", "/work/widgets", "widgets-other"] {
            let result = resolve_repository(&durable, &observed, "test", "local", &RepoSelector::Query(query.into())).await;
            if matches!(query, "unknown" | "/work/widgets" | "widgets-other") {
                assert_eq!(result, Ok(None), "unknown selector is a typed no-match");
            } else {
                assert!(!result.expect_err("refused selector").contains("tracked"));
            }
        }
        assert!(resolve_repository(&durable, &observed, "test", "local", &RepoSelector::Repository(RepositoryKey("absent".into())))
            .await
            .is_err());
        assert_eq!(durable.using::<Repository>("test").list().await.expect("repositories").items.len(), 2);
        assert!(observed.all().is_empty());
    }
    // An engine checkout scopes the CLI only once it is ready on this host: a requested
    // checkout's path, or one in another host's environment, never does, and its ending removes
    // it even where a stale observation of its name remains.
    #[tokio::test]
    async fn cwd_uses_ready_local_engine_checkouts_over_observations() {
        use flotilla_resources::FreshCloneCheckoutSpec;

        let durable = backend();
        let observed = DiscoveredCheckouts::default();
        let key = repository(&durable, "widgets", "acme/widgets").await;
        let engine = engine_on("local");
        let spec = CheckoutSpec::FreshClone(
            FreshCloneCheckoutSpec::builder()
                .repo_ref(key.clone())
                .env_ref("host-direct".into())
                .r#ref("main".into())
                .target_path("/actual/widgets".into())
                .url("https://github.com/acme/widgets".into())
                .build(),
        );
        let launch = engine.request("created", &spec, None);
        let resolve = |host: &'static str, path: &'static str| {
            let (durable, observed, reads) = (durable.clone(), observed.clone(), engine.reads.clone());
            async move {
                resolve_repository_with_lifecycle_reads(&durable, &observed, "test", host, &RepoSelector::Path(path.into()), reads.as_ref())
                    .await
            }
        };
        assert_eq!(resolve("local", "/actual/widgets").await, Ok(None), "a requested checkout's path does not scope");
        engine.ready(&launch, None);
        assert_eq!(resolve("local", "/actual/widgets/src").await, Ok(Some(key.clone())));
        assert_eq!(resolve("remote", "/actual/widgets/src").await, Ok(None), "another host's checkout does not scope");
        // A stale observation of its name cannot resurrect a released checkout.
        checkout().discovered(&observed).name("created").path("/actual/widgets").key(key).call();
        engine.finish(&launch, Ending::Released);
        assert_eq!(resolve("local", "/actual/widgets").await, Ok(None));
    }

    // Local repository addressing uses the engine's ready path and excludes every non-ready held
    // checkout, the engine's checkout of a name winning over an observation of it; a read that
    // fails is an error, never an empty answer.
    #[tokio::test]
    async fn repository_paths_follow_checkout_engine() {
        use flotilla_resources::CheckoutWorktreeSpec;
        let durable = backend();
        let observed = DiscoveredCheckouts::default();
        let key = repository(&durable, "widgets", "acme/widgets").await;
        let temp = tempfile::tempdir().expect("paths");
        let live = temp.path().join("live");
        let stale = temp.path().join("stale");
        checkout().discovered(&observed).name("managed").path(stale.to_str().expect("path")).key(key.clone()).call();
        let engine = engine_on("local");
        let launch = engine.request(
            "managed",
            &CheckoutSpec::Worktree(CheckoutWorktreeSpec {
                repo_ref: key.clone(),
                env_ref: "host-direct".into(),
                r#ref: "main".into(),
                base_ref: None,
                target_path: live.to_string_lossy().into_owned(),
                clone_ref: "source".into(),
            }),
            None,
        );
        let reads = engine.reads.as_ref();
        assert!(local_checkouts_with_lifecycle_reads(&observed, "test", "local", reads).await.expect("requested").is_empty());
        engine.finish(&launch, Ending::Ready(live.to_str().expect("path")));
        assert_eq!(
            resolve_repository_with_lifecycle_reads(&durable, &observed, "test", "local", &RepoSelector::Path(live.clone()), reads)
                .await
                .expect("ready path"),
            Some(key)
        );
        assert!(resolve_repository_with_lifecycle_reads(&durable, &observed, "test", "local", &RepoSelector::Path(stale), reads)
            .await
            .expect("stale path")
            .is_none());
        engine.finish(&launch, Ending::Lost(live.to_str().expect("path")));
        assert!(local_checkouts_with_lifecycle_reads(&observed, "test", "local", reads).await.expect("lost").is_empty());
        engine.poison();
        assert!(local_checkouts_with_lifecycle_reads(&observed, "test", "local", reads)
            .await
            .expect_err("checked read")
            .contains("poisoned"));
    }
}
