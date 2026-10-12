//! Discovery-time GitHub repository resolution shared by the change-request
//! and issue-tracker factories.

use flotilla_discovery_api::EnvironmentBag;
use flotilla_discovery_api::UnmetRequirement;
use flotilla_resources::ForgeKind;

/// Resolve `owner/repo` for a GitHub origin, reporting every unmet
/// requirement (the `gh` binary and a GitHub origin remote) together.
pub fn github_repo_slug(env: &EnvironmentBag) -> Result<String, Vec<UnmetRequirement>> {
    let mut unmet = vec![];
    if env.find_binary("gh").is_none() {
        unmet.push(UnmetRequirement::MissingBinary("gh".into()));
    }
    let remote = env.find_origin_remote().filter(|(host, _, _)| {
        env.find_origin_forge().map_or_else(|| host.eq_ignore_ascii_case("github.com"), |forge| forge.kind == ForgeKind::Github)
    });
    if remote.is_none() {
        unmet.push(UnmetRequirement::MissingRemoteHost("github.com".into()));
    }
    if !unmet.is_empty() {
        return Err(unmet);
    }
    let (_, owner, repo) = remote.expect("checked above");
    Ok(format!("{owner}/{repo}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use flotilla_discovery_api::EnvironmentAssertion;
    use flotilla_discovery_api::EnvironmentBag;
    use flotilla_discovery_api::UnmetRequirement;
    use flotilla_resources::{ForgeKind, ForgeSpec};

    use super::github_repo_slug;

    // The factories' discovery contract: both requirements are reported
    // together, and a non-GitHub origin forge never yields a GitHub slug.
    #[test]
    fn github_repo_slug_reports_unmet_requirements_and_ignores_other_forges() {
        let gh = EnvironmentAssertion::binary("gh", "/usr/bin/gh");
        let remote = EnvironmentAssertion::remote_host("github.com", "acme", "widgets", "origin");
        assert_eq!(github_repo_slug(&EnvironmentBag::new().with(gh.clone()).with(remote.clone())), Ok("acme/widgets".into()));
        assert_eq!(
            github_repo_slug(&EnvironmentBag::new()),
            Err(vec![UnmetRequirement::MissingBinary("gh".into()), UnmetRequirement::MissingRemoteHost("github.com".into())])
        );
        let forgejo = ForgeSpec::builder()
            .forge_id("lab".into())
            .kind(ForgeKind::Forgejo)
            .hosts(BTreeSet::from(["github.com".into()]))
            .https_url("https://github.com".into())
            .git_ssh_host("github.com".into())
            .build();
        assert_eq!(
            github_repo_slug(&EnvironmentBag::new().with(gh).with(remote).with(EnvironmentAssertion::origin_forge(forgejo))),
            Err(vec![UnmetRequirement::MissingRemoteHost("github.com".into())])
        );
    }
}
