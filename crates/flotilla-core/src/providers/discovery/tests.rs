use super::*;
use crate::providers::issue_tracker::forge_issue_source;
use flotilla_resources::{ForgeKind, ForgeSpec};

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
