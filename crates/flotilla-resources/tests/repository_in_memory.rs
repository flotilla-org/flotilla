use flotilla_resources::{
    normalize_project_spec, DefaultBranchObservation, DefaultBranchProvenance, InMemoryBackend, InputMeta, ProjectRepositorySpec,
    ProjectSpec, Repository, RepositoryIdentity, RepositoryKey, RepositorySpec, ResourceBackend, SqliteBackend,
};

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

#[test]
fn remote_less_worktrees_converge_on_host_and_git_common_directory() {
    let first = RepositorySpec::local("host-01", "/srv/repos/flotilla/.git/./").expect("local identity");
    let second = RepositorySpec::local("host-01", "/srv/repos/flotilla/.git").expect("local identity");

    assert_eq!(first.key(), second.key());
    assert!(matches!(first.identity(), RepositoryIdentity::Local { .. }));
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
        issue_source: None,
        repositories: vec![
            ProjectRepositorySpec { repo: repo_b.clone(), subpath: Some("./apps/api".to_string()), default_branch: None },
            ProjectRepositorySpec { repo: repo_a.clone(), subpath: None, default_branch: None },
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
        issue_source: None,
        repositories: vec![ProjectRepositorySpec { repo: repo_b.clone(), subpath: None, default_branch: None }, ProjectRepositorySpec {
            repo: repo_b,
            subpath: None,
            default_branch: None,
        }],
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
            issue_source: None,
            repositories: vec![ProjectRepositorySpec {
                repo: RepositoryKey("repo".to_string()),
                subpath: Some(subpath.to_string()),
                default_branch: None,
            }],
        };
        assert!(normalize_project_spec(spec).is_err(), "{subpath} should be rejected");
    }
}
