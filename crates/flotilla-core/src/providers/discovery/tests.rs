use flotilla_resources::{ForgeKind, ForgeSpec};

use super::*;
use crate::providers::issue_tracker::forge_issue_source;
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_paths::path_context::ExecutionEnvironmentPath;

fn sample_bag() -> EnvironmentBag {
    EnvironmentBag::new()
        .with(EnvironmentAssertion::versioned_binary("git", "/usr/bin/git", "2.40.0"))
        .with(EnvironmentAssertion::binary("gh", "/usr/bin/gh"))
        .with(EnvironmentAssertion::env_var("GITHUB_TOKEN", "ghp_abc123"))
        .with(EnvironmentAssertion::vcs_checkout("/home/user/project", VcsKind::Git, true))
        .with(EnvironmentAssertion::remote_host("github.com", "acme", "widgets", "upstream"))
        .with(EnvironmentAssertion::remote_host("github.com", "fork-owner", "widgets", "origin"))
        .with(EnvironmentAssertion::auth_file("github", "/home/user/.config/gh/hosts.yml"))
        .with(EnvironmentAssertion::socket("cmux", "/tmp/cmux.sock"))
}

#[test]
fn find_binary_returns_matching_path() {
    let bag = sample_bag();
    assert_eq!(bag.find_binary("git"), Some(&ExecutionEnvironmentPath::new("/usr/bin/git")));
    assert_eq!(bag.find_binary("gh"), Some(&ExecutionEnvironmentPath::new("/usr/bin/gh")));
    assert_eq!(bag.find_binary("nonexistent"), None);
}

#[test]
fn find_env_var_returns_value() {
    let bag = sample_bag();
    assert_eq!(bag.find_env_var("GITHUB_TOKEN"), Some("ghp_abc123"));
    assert_eq!(bag.find_env_var("MISSING"), None);
}

#[test]
fn find_remote_host_prefers_origin() {
    let bag = sample_bag();
    let result = bag.find_remote_host("github.com");
    // Should prefer origin over upstream
    assert_eq!(result, Some(("fork-owner", "widgets", "origin")));
}

#[test]
fn find_remote_host_falls_back_to_first() {
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::remote_host("github.com", "acme", "widgets", "upstream"))
        .with(EnvironmentAssertion::remote_host("github.com", "other", "widgets", "fork"));
    let result = bag.find_remote_host("github.com");
    assert_eq!(result, Some(("acme", "widgets", "upstream")));
}

#[test]
fn find_remote_host_filters_by_platform() {
    let bag = sample_bag();
    assert_eq!(bag.find_remote_host("gitlab.com"), None);
}

#[test]
fn has_auth_checks_provider() {
    let bag = sample_bag();
    assert!(bag.has_auth("github"));
    assert!(!bag.has_auth("gitlab"));
}

#[test]
fn find_vcs_checkout_returns_root_and_flag() {
    let bag = sample_bag();
    let result = bag.find_vcs_checkout(VcsKind::Git);
    assert_eq!(result, Some((&ExecutionEnvironmentPath::new("/home/user/project"), true)));
    assert_eq!(bag.find_vcs_checkout(VcsKind::Jujutsu), None);
}

#[test]
fn repo_slug_from_github() {
    let bag = sample_bag();
    // origin is fork-owner/widgets
    assert_eq!(bag.repo_slug(), Some("fork-owner/widgets".into()));
}

#[test]
fn repo_slug_falls_back_to_gitlab() {
    let bag = EnvironmentBag::new().with(EnvironmentAssertion::remote_host("gitlab.com", "gl-org", "project", "origin"));
    assert_eq!(bag.repo_slug(), Some("gl-org/project".into()));
}

#[test]
fn repo_slug_none_when_empty() {
    let bag = EnvironmentBag::new();
    assert_eq!(bag.repo_slug(), None);
}

#[test]
fn merge_combines_assertions() {
    let bag1 = EnvironmentBag::new().with(EnvironmentAssertion::binary("git", "/usr/bin/git"));
    let bag2 = EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh"));

    let merged = bag1.merge(&bag2);
    assert!(merged.find_binary("git").is_some());
    assert!(merged.find_binary("gh").is_some());
    // Originals unchanged
    assert!(bag1.find_binary("gh").is_none());
}

#[test]
fn find_socket_returns_path() {
    let bag = sample_bag();
    assert_eq!(bag.find_socket("cmux"), Some(&DaemonHostPath::new("/tmp/cmux.sock")));
    assert_eq!(bag.find_socket("nonexistent"), None);
}

#[test]
fn remote_hosts_returns_all() {
    let bag = sample_bag();
    let hosts = bag.remote_hosts();
    assert_eq!(hosts.len(), 2);
}

#[test]
fn extend_adds_multiple() {
    let bag = EnvironmentBag::new().extend(vec![EnvironmentAssertion::binary("a", "/a"), EnvironmentAssertion::binary("b", "/b")]);
    assert!(bag.find_binary("a").is_some());
    assert!(bag.find_binary("b").is_some());
}

#[test]
fn unmet_requirement_variants() {
    // Verify that all UnmetRequirement variants can be constructed and compared
    let reqs = [
        UnmetRequirement::MissingBinary("git".into()),
        UnmetRequirement::MissingEnvVar("TOKEN".into()),
        UnmetRequirement::MissingAuth("github".into()),
        UnmetRequirement::MissingConfig("[provider]".into()),
        UnmetRequirement::MissingRemoteHost("github.com".into()),
        UnmetRequirement::NoVcsCheckout,
        UnmetRequirement::UnknownProviderPreference { category: ProviderCategory::AiUtility, key: "nonexistent".into() },
    ];
    assert_eq!(reqs[0], UnmetRequirement::MissingBinary("git".into()));
    assert_ne!(reqs[0], reqs[1]);
}

#[test]
fn provider_descriptor_fields() {
    let desc = ProviderDescriptor::labeled(
        ProviderCategory::ChangeRequest,
        "github",
        "github-cr",
        "GitHub PRs",
        "PR",
        "Pull Requests",
        "pull request",
    );
    assert_eq!(desc.category, ProviderCategory::ChangeRequest);
    assert_eq!(desc.backend, "github");
    assert_eq!(desc.implementation, "github-cr");
    assert_eq!(desc.display_name, "GitHub PRs");
    assert_eq!(desc.abbreviation, "PR");
    assert_eq!(desc.section_label, "Pull Requests");
    assert_eq!(desc.item_noun, "pull request");
}

#[test]
fn provider_descriptor_named_defaults_labels() {
    let desc = ProviderDescriptor::named(ProviderCategory::CloudAgent, "claude");
    assert_eq!(desc.category, ProviderCategory::CloudAgent);
    assert_eq!(desc.backend, "claude");
    assert_eq!(desc.implementation, "claude");
    assert_eq!(desc.display_name, "claude");
    assert!(desc.abbreviation.is_empty());
    assert!(desc.section_label.is_empty());
    assert!(desc.item_noun.is_empty());
}

#[test]
fn provider_descriptor_labeled_simple() {
    let desc = ProviderDescriptor::labeled_simple(ProviderCategory::IssueProvider, "github", "GitHub Issues", "#", "Issues", "issue");
    assert_eq!(desc.category, ProviderCategory::IssueProvider);
    assert_eq!(desc.backend, "github");
    assert_eq!(desc.implementation, "github");
    assert_eq!(desc.display_name, "GitHub Issues");
}

#[test]
fn provider_category_slug_round_trip() {
    let expected = [
        "vcs",
        "change_request",
        "issue_tracker",
        "cloud_agent",
        "ai_utility",
        "workspace_manager",
        "terminal_pool",
        "environment_provider",
    ];
    assert_eq!(ProviderCategory::ALL.map(|category| category.slug()), expected);
}

#[test]
fn repo_identity_from_github_remote() {
    let bag = EnvironmentBag::new().with(EnvironmentAssertion::remote_host("github.com", "rjwittams", "flotilla", "origin"));
    let identity = bag.repo_identity().expect("should have identity");
    assert_eq!(identity.authority, "github.com");
    assert_eq!(identity.path, "rjwittams/flotilla");
}

#[test]
fn repo_identity_from_gitlab_remote() {
    let bag = EnvironmentBag::new().with(EnvironmentAssertion::remote_host("gitlab.com", "gl-org", "project", "origin"));
    let identity = bag.repo_identity().expect("should have identity");
    assert_eq!(identity.authority, "gitlab.com");
    assert_eq!(identity.path, "gl-org/project");
}

#[test]
fn repo_identity_uses_canonical_forge_host_without_trailing_slash() {
    let forge = ForgeSpec::builder()
        .forge_id("lab".into())
        .kind(ForgeKind::Forgejo)
        .hosts(std::collections::BTreeSet::from(["ssh-lab".into()]))
        .https_url("https://forgejo.example.test/".into())
        .git_ssh_host("ssh-lab".into())
        .build();
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::remote_host("ssh-lab", "team", "repo", "origin"))
        .with(EnvironmentAssertion::origin_forge(forge));
    assert_eq!(bag.repo_identity().expect("repo identity").authority, "forgejo.example.test");
}

#[test]
fn repo_identity_keeps_declared_forge_installation_paths_distinct() {
    for (id, prefix) in [("lab", "lab"), ("stage", "stage")] {
        let forge = ForgeSpec::builder()
            .forge_id(id.into())
            .kind(ForgeKind::Forgejo)
            .hosts(std::collections::BTreeSet::from(["ssh-forge".into()]))
            .https_url(format!("https://forgejo.example.test/{prefix}"))
            .git_ssh_host("ssh-forge".into())
            .build();
        let bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::remote_host("ssh-forge", format!("{prefix}/team"), "repo", "origin"))
            .with(EnvironmentAssertion::origin_forge(forge));
        let identity = bag.repo_identity().expect("repo identity");
        assert_eq!(identity.authority, format!("forgejo.example.test/{prefix}"));
        assert_eq!(identity.path, "team/repo");
        assert_eq!(bag.repo_slug().as_deref(), Some("team/repo"));
        let issue_source = forge_issue_source(&identity);
        assert_eq!(issue_source.service, format!("https://forgejo.example.test/{prefix}"));
        assert_eq!(issue_source.scope, "team/repo");
    }
}

#[test]
fn repo_identity_none_when_no_remote() {
    let bag = EnvironmentBag::new();
    assert!(bag.repo_identity().is_none());
}

#[test]
fn environment_bag_assertions_accessor() {
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::BinaryAvailable {
            name: "git".into(),
            path: ExecutionEnvironmentPath::new("/usr/bin/git"),
            version: Some("2.40".into()),
        })
        .with(EnvironmentAssertion::AuthFileExists {
            provider: "github".into(),
            path: ExecutionEnvironmentPath::new("/home/user/.config/gh/hosts.yml"),
        });
    assert_eq!(bag.assertions().len(), 2);
    assert!(matches!(bag.assertions()[0], EnvironmentAssertion::BinaryAvailable { ref name, .. } if name == "git"));
}
