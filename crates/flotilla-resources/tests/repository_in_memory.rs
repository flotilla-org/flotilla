use std::collections::BTreeMap;

use flotilla_resources::{
    normalize_project_spec, repository_display_labels, resolve_project_issue_sources, DefaultBranchObservation, DefaultBranchProvenance,
    InMemoryBackend, InputMeta, IssueFieldValue, IssueFilter, IssueSource, IssueSourceBindingSpec, IssueSourceResolution,
    IssueSourceUnavailable, ProjectRepositoryRole, ProjectRepositorySpec, ProjectSpec, Repository, RepositoryIdentity, RepositoryKey,
    RepositoryRelation, RepositorySpec, ResourceBackend, SqliteBackend,
};

#[tokio::test]
async fn declared_issue_source_does_not_hide_an_unavailable_member_repository() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repositories = backend.including_replicas::<Repository>("flotilla");
    let override_source = IssueSource { service: "linear".into(), scope: "WIDGET".into() };
    let project = ProjectSpec {
        display_name: "Widgets".into(),
        default_workflow_ref: "single-agent-contained".into(),
        issue_sources: vec![IssueSourceBindingSpec::builder().source(override_source.clone()).alias("widgets".to_string()).build()],
        dispatch_policy: None,
        repositories: vec![ProjectRepositorySpec {
            repo: RepositoryKey("repository-not-present-on-this-host".into()),
            alias: None,
            roles: Default::default(),
            subpath: None,
            default_branch: None,
        }],
    };

    assert!(matches!(
        resolve_project_issue_sources(&repositories, &project).await,
        IssueSourceResolution::Unavailable(IssueSourceUnavailable::RepositoryUnavailable { .. })
    ));
}

#[tokio::test]
async fn project_issue_bindings_add_exclude_and_filter_derived_sources() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repository_writer = backend.using::<Repository>("flotilla");
    let repositories = backend.including_replicas::<Repository>("flotilla");
    let github = RepositorySpec::remote("https://github.com/acme/app").expect("repository");
    repository_writer.create(&InputMeta::builder().name(github.key().to_string()).build(), &github).await.expect("create repository");
    let github_source = IssueSource { service: "https://github.com".into(), scope: "acme/app".into() };
    let forgejo_source = IssueSource { service: "https://forgejo.lab.flotilla.work".into(), scope: "fork-issues/zellij".into() };
    let project = ProjectSpec {
        display_name: "Zellij".into(),
        default_workflow_ref: "single-agent-contained".into(),
        issue_sources: vec![
            IssueSourceBindingSpec::builder().source(github_source).exclude(true).build(),
            IssueSourceBindingSpec::builder()
                .source(forgejo_source.clone())
                .alias("zellij".to_string())
                .filter(IssueFilter { match_fields: BTreeMap::from([("component".into(), IssueFieldValue::One("terminal".into()))]) })
                .build(),
        ],
        dispatch_policy: None,
        repositories: vec![ProjectRepositorySpec::builder()
            .repo(github.key())
            .alias("zellij".to_string())
            .roles([ProjectRepositoryRole::Code].into())
            .build()],
    };

    let IssueSourceResolution::Available { bindings } = resolve_project_issue_sources(&repositories, &project).await else {
        panic!("bindings should resolve");
    };
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].source, forgejo_source);
    assert_eq!(bindings[0].alias, "zellij");
    assert_eq!(bindings[0].filter.match_fields["component"], IssueFieldValue::One("terminal".into()));

    let yaml = serde_yml::to_string(&project).expect("serialize project");
    assert_eq!(serde_yml::from_str::<ProjectSpec>(&yaml).expect("deserialize project"), project);
}

#[test]
fn creatable_issue_binding_must_create_values_matching_its_filter() {
    let binding = IssueSourceBindingSpec::builder()
        .source(IssueSource { service: "https://github.com".into(), scope: "acme/app".into() })
        .alias("app".to_string())
        .filter(IssueFilter { match_fields: BTreeMap::from([("labels".into(), IssueFieldValue::One("terminal".into()))]) })
        .create_with(BTreeMap::from([("labels".into(), IssueFieldValue::Many(vec!["bug".into()]))]))
        .creatable(true)
        .build();
    let spec = ProjectSpec {
        display_name: "App".into(),
        default_workflow_ref: "implement".into(),
        issue_sources: vec![binding],
        repositories: vec![ProjectRepositorySpec::builder().repo(RepositoryKey("app".into())).build()],
        dispatch_policy: None,
    };

    assert!(normalize_project_spec(spec)
        .expect_err("mismatched creation values must fail")
        .contains("does not satisfy filter field `labels`"));
}

#[test]
fn issue_bindings_reject_state_as_band_semantics() {
    let spec = ProjectSpec {
        display_name: "App".into(),
        default_workflow_ref: "implement".into(),
        issue_sources: vec![IssueSourceBindingSpec::builder()
            .source(IssueSource { service: "https://github.com".into(), scope: "acme/app".into() })
            .filter(IssueFilter { match_fields: BTreeMap::from([("state".into(), IssueFieldValue::One("open".into()))]) })
            .build()],
        repositories: vec![ProjectRepositorySpec::builder().repo(RepositoryKey("app".into())).build()],
        dispatch_policy: None,
    };

    assert_eq!(normalize_project_spec(spec).expect_err("state is not a binding field"), "issue source bindings cannot configure state");
}

#[tokio::test]
async fn project_issue_sources_are_the_deduplicated_union_of_repository_forges() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repository_writer = backend.using::<Repository>("flotilla");
    let first = RepositorySpec::remote("https://github.com/flotilla-org/flotilla.git").expect("first repository");
    let second = RepositorySpec::remote("https://gitlab.com/widgets/api.git").expect("second repository");
    for spec in [&first, &second] {
        repository_writer.create(&InputMeta::builder().name(spec.key().to_string()).build(), spec).await.expect("repository should create");
    }
    let repositories = backend.including_replicas::<Repository>("flotilla");
    let project = ProjectSpec {
        display_name: "Widgets".into(),
        default_workflow_ref: "single-agent-contained".into(),
        issue_sources: vec![IssueSourceBindingSpec::builder()
            .source(IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() })
            .filter(IssueFilter { match_fields: BTreeMap::from([("labels".into(), IssueFieldValue::One("ready".into()))]) })
            .build()],
        dispatch_policy: None,
        repositories: vec![
            ProjectRepositorySpec {
                repo: first.key(),
                alias: Some("core".into()),
                roles: [ProjectRepositoryRole::Code].into(),
                subpath: None,
                default_branch: None,
            },
            ProjectRepositorySpec {
                repo: second.key(),
                alias: Some("api".into()),
                roles: [ProjectRepositoryRole::Code].into(),
                subpath: None,
                default_branch: None,
            },
            ProjectRepositorySpec {
                repo: first.key(),
                alias: None,
                roles: Default::default(),
                subpath: Some("duplicate-source".into()),
                default_branch: None,
            },
        ],
    };

    let IssueSourceResolution::Available { bindings } = resolve_project_issue_sources(&repositories, &project).await else {
        panic!("derived sources should resolve");
    };
    assert_eq!(bindings.iter().map(|binding| &binding.source).collect::<Vec<_>>(), vec![
        &IssueSource { service: "https://gitlab.com".into(), scope: "widgets/api".into() },
        &IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() },
    ]);
    assert_eq!(bindings.iter().map(|binding| binding.alias.as_str()).collect::<Vec<_>>(), vec!["api", "core"]);
    assert!(bindings.iter().find(|binding| binding.alias == "core").expect("filtered derived binding").creatable);
}

#[tokio::test]
async fn project_issue_source_resolution_reports_typed_unavailability() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repository_writer = backend.using::<Repository>("flotilla");
    let local = RepositorySpec::local("host-01", "/srv/widgets/.git").expect("local repository");
    repository_writer.create(&InputMeta::builder().name(local.key().to_string()).build(), &local).await.expect("repository should create");
    let repositories = backend.including_replicas::<Repository>("flotilla");
    let local_only = ProjectSpec {
        display_name: "Widgets".into(),
        default_workflow_ref: "single-agent-contained".into(),
        issue_sources: Vec::new(),
        dispatch_policy: None,
        repositories: vec![ProjectRepositorySpec {
            repo: local.key(),
            alias: None,
            roles: Default::default(),
            subpath: None,
            default_branch: None,
        }],
    };
    assert_eq!(
        resolve_project_issue_sources(&repositories, &local_only).await,
        IssueSourceResolution::Unavailable(IssueSourceUnavailable::NoIssueSource)
    );

    let missing = RepositoryKey("missing".into());
    let unresolved = ProjectSpec {
        repositories: vec![ProjectRepositorySpec {
            repo: missing.clone(),
            alias: None,
            roles: Default::default(),
            subpath: None,
            default_branch: None,
        }],
        ..local_only
    };
    assert!(matches!(
        resolve_project_issue_sources(&repositories, &unresolved).await,
        IssueSourceResolution::Unavailable(IssueSourceUnavailable::RepositoryUnavailable { repository, .. }) if repository == missing
    ));
}

#[tokio::test]
async fn repository_roundtrips_and_is_immutable_in_local_backends() {
    let backends = [
        ResourceBackend::InMemory(InMemoryBackend::default()),
        ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite backend should open")),
    ];
    for backend in backends {
        let repositories = backend.using::<Repository>("flotilla");
        let spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla.git").expect("remote should normalize");
        let key = spec.key();

        repositories.create(&InputMeta::builder().name(key.to_string()).build(), &spec).await.expect("repository should create");
        let fetched = repositories.get(&key.to_string()).await.expect("repository should fetch");

        assert_eq!(fetched.spec, spec);
        fetched.spec.verify_key(&key).expect("fetched identity should match key");
        assert!(fetched.spec.verify_key(&RepositoryKey("wrong".to_string())).is_err());

        let replacement = RepositorySpec::remote("https://github.com/flotilla-org/other").expect("replacement spec");
        let update = repositories
            .update(&InputMeta::builder().name(key.to_string()).build(), &fetched.metadata.resource_version, &replacement)
            .await;
        assert!(update.expect_err("repository identity must be immutable").to_string().contains("immutable"));
    }
}

#[tokio::test]
async fn repository_fork_provenance_roundtrips_and_can_enrich_identity() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repositories = backend.using::<Repository>("flotilla");
    let identity = RepositorySpec::remote("https://forgejo.lab/fork-issues/zellij").expect("fork identity");
    let key = identity.key();
    repositories.create(&InputMeta::builder().name(key.to_string()).build(), &identity).await.expect("repository should create");
    let fork =
        identity.with_upstream("https://github.com/zellij-org/zellij.git", RepositoryRelation::Fork).expect("upstream should normalize");

    let enriched = flotilla_resources::ensure_repository(&repositories, &key, &fork).await.expect("provenance should enrich");

    assert_eq!(enriched.spec, fork);
    assert!(enriched.spec.is_fork());
    assert_eq!(enriched.spec.upstream().expect("upstream").url, "https://github.com/zellij-org/zellij");
}

#[test]
fn remote_less_worktrees_converge_on_host_and_git_common_directory() {
    let first = RepositorySpec::local("host-01", "/srv/repos/flotilla/.git/./").expect("local identity");
    let second = RepositorySpec::local("host-01", "/srv/repos/flotilla/.git").expect("local identity");

    assert_eq!(first.key(), second.key());
    assert!(matches!(first.identity(), RepositoryIdentity::Local { .. }));
}

#[test]
fn declared_remotes_share_the_first_remote_identity() {
    let canonical = "https://github.com/flotilla-org/flotilla";
    let mirror = "https://forgejo.lab/lab/flotilla.git";
    let repository =
        RepositorySpec::remote(mirror).expect("mirror observation").with_remotes([canonical, mirror]).expect("multi-remote declaration");

    assert_eq!(repository.key(), RepositorySpec::remote(canonical).expect("canonical repository").key());
    assert_eq!(repository.remotes(), [canonical, "https://forgejo.lab/lab/flotilla"]);
    assert!(repository.declares_remote(mirror));
    assert_eq!(repository.forge().expect("canonical forge").service_url, "https://github.com");
    assert_eq!(repository.repo_fact_value(), "flotilla-org/flotilla");
}

#[test]
fn declared_remotes_must_include_the_observed_repository_and_remain_unique() {
    let observed = RepositorySpec::remote("https://forgejo.lab/lab/flotilla").expect("mirror observation");
    assert!(observed.clone().with_remotes(["https://github.com/flotilla-org/flotilla"]).is_err());
    assert!(observed.with_remotes(["https://forgejo.lab/lab/flotilla", "https://forgejo.lab/lab/flotilla.git"]).is_err());
}

#[test]
fn remote_move_preserves_identity_and_tracks_live_forge() {
    let original = "https://github.com/example/old-name";
    let moved = "https://github.com/example/new-name";
    let original_spec = RepositorySpec::remote(original).expect("original repository");
    let key = original_spec.key();

    let moved_spec = original_spec.update_remotes(moved).expect("remote move");

    assert_eq!(moved_spec.key(), key);
    assert_eq!(moved_spec.remotes(), [moved, original]);
    assert_eq!(moved_spec.live_remote(), Some(moved));
    assert_eq!(moved_spec.forge().expect("forge").repository, "example/new-name");
    moved_spec.verify_key(&key).expect("birth identity remains valid");
}

#[test]
fn repository_display_labels_use_forge_slugs_and_qualify_collisions() {
    let flotilla = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("flotilla repository");
    let flotilla_widgets = RepositorySpec::remote("https://github.com/flotilla-org/widgets").expect("flotilla widgets repository");
    let acme_widgets = RepositorySpec::remote("https://gitlab.com/acme/widgets").expect("acme widgets repository");
    let mirrored_widgets = RepositorySpec::remote("https://github.com/acme/widgets").expect("mirrored widgets repository");
    let repositories = [
        (flotilla.key(), flotilla),
        (flotilla_widgets.key(), flotilla_widgets),
        (acme_widgets.key(), acme_widgets),
        (mirrored_widgets.key(), mirrored_widgets),
    ];

    let labels = repository_display_labels(repositories.iter().map(|(key, spec)| (key, spec)));

    assert_eq!(labels[&repositories[0].0], "flotilla-org/flotilla");
    assert_eq!(labels[&repositories[1].0], "flotilla-org/widgets");
    assert_eq!(labels[&repositories[2].0], "gitlab.com/acme/widgets");
    assert_eq!(labels[&repositories[3].0], "github.com/acme/widgets");
}

#[test]
fn repository_workspace_slugs_are_short_and_qualify_basename_collisions() {
    let cleat = RepositorySpec::remote("https://github.com/flotilla-org/cleat").expect("cleat repository");
    let flotilla = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("flotilla repository");
    let andamento = RepositorySpec::remote("https://github.com/flotilla-org/andamento").expect("andamento repository");
    let github_shared = RepositorySpec::remote("https://github.com/org-a/shared").expect("github shared repository");
    let gitlab_shared = RepositorySpec::remote("https://gitlab.com/org-b/shared").expect("gitlab shared repository");
    let repositories = [cleat, flotilla, andamento, github_shared, gitlab_shared];
    let keyed_repositories = repositories.iter().map(|spec| (spec.key(), spec)).collect::<Vec<_>>();
    let slugs = flotilla_resources::repository_workspace_slugs(keyed_repositories.iter().map(|(key, spec)| (key, *spec)));

    assert_eq!(slugs[&repositories[0].key()], "cleat");
    assert_eq!(slugs[&repositories[1].key()], "flotilla");
    assert_eq!(slugs[&repositories[2].key()], "andamento");
    assert_eq!(slugs[&repositories[3].key()], "github-com-org-a-shared");
    assert_eq!(slugs[&repositories[4].key()], "gitlab-com-org-b-shared");
    assert!(repositories.iter().all(|repository| slugs[&repository.key()] != repository.key().to_string()));
}

#[test]
fn repository_declarations_reject_unresolved_aliases_and_inconsistent_forges() {
    assert!(RepositorySpec::remote("work-github:flotilla-org/flotilla.git").is_err());
    assert!(RepositorySpec::remote("git@github.com:flotilla-org/flotilla.git").is_err());

    let inconsistent = serde_json::json!({
        "identity": { "kind": "remote", "canonical_remote": "https://github.com/flotilla-org/flotilla" },
        "forge": { "service_url": "https://gitlab.com", "repository": "other/repo" }
    });
    assert!(serde_json::from_value::<RepositorySpec>(inconsistent).is_err());
}

#[test]
fn default_branch_resolution_preserves_provenance_and_authority_order() {
    let observations = vec![
        DefaultBranchObservation { branch: "trunk".to_string(), provenance: DefaultBranchProvenance::LocalTrunk },
        DefaultBranchObservation { branch: "main".to_string(), provenance: DefaultBranchProvenance::RemoteSymbolicHead },
        DefaultBranchObservation { branch: "stable".to_string(), provenance: DefaultBranchProvenance::Forge },
    ];

    let (resolved, diagnostics) = flotilla_resources::resolve_default_branch(&observations);

    assert_eq!(resolved.as_deref(), Some("stable"));
    assert!(!diagnostics.is_empty(), "disagreement should remain diagnostic");
    assert_eq!(flotilla_resources::resolve_default_branch(&[]), (None, Vec::new()));
}

#[test]
fn project_normalization_sorts_entries_omits_whole_repo_subpath_and_rejects_duplicates() {
    let repo_a = RepositoryKey("a".to_string());
    let repo_b = RepositoryKey("b".to_string());
    let normalized = normalize_project_spec(ProjectSpec {
        display_name: " Example ".to_string(),
        default_workflow_ref: " single-agent-contained ".to_string(),
        issue_sources: Vec::new(),
        dispatch_policy: None,
        repositories: vec![
            ProjectRepositorySpec {
                repo: repo_b.clone(),
                alias: None,
                roles: Default::default(),
                subpath: Some("./apps/api".to_string()),
                default_branch: None,
            },
            ProjectRepositorySpec { repo: repo_a.clone(), alias: None, roles: Default::default(), subpath: None, default_branch: None },
        ],
    })
    .expect("project should normalize");

    assert_eq!(normalized.display_name, "Example");
    assert_eq!(normalized.default_workflow_ref, "single-agent-contained");
    assert_eq!(normalized.repositories[0].repo, repo_a);
    assert_eq!(normalized.repositories[0].subpath, None);
    assert_eq!(normalized.repositories[1].subpath.as_deref(), Some("apps/api"));

    let duplicate = ProjectSpec {
        display_name: "Example".to_string(),
        default_workflow_ref: "single-agent-contained".to_string(),
        issue_sources: Vec::new(),
        dispatch_policy: None,
        repositories: vec![
            ProjectRepositorySpec { repo: repo_b.clone(), alias: None, roles: Default::default(), subpath: None, default_branch: None },
            ProjectRepositorySpec { repo: repo_b, alias: None, roles: Default::default(), subpath: None, default_branch: None },
        ],
    };
    assert!(normalize_project_spec(duplicate).expect_err("duplicates should fail").contains("duplicate"));

    let serialized = serde_json::to_value(&normalized).expect("project should serialize");
    assert!(serialized["repositories"][0].get("subpath").is_none(), "whole-repo subpath should be omitted");
    assert!(serialized["repositories"][0].get("default_branch").is_none(), "inherited default branch should be omitted");
}

#[test]
fn project_subpaths_reject_absolute_and_parent_traversal() {
    for subpath in ["/tmp/app", "apps/../../secret", "."] {
        let spec = ProjectSpec {
            display_name: "Example".to_string(),
            default_workflow_ref: "single-agent-contained".to_string(),
            issue_sources: Vec::new(),
            dispatch_policy: None,
            repositories: vec![ProjectRepositorySpec {
                repo: RepositoryKey("repo".to_string()),
                alias: None,
                roles: Default::default(),
                subpath: Some(subpath.to_string()),
                default_branch: None,
            }],
        };
        assert!(normalize_project_spec(spec).is_err(), "{subpath} should be rejected");
    }
}
