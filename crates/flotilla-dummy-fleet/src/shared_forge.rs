//! A Forgejo forge shared by every host of a rehearsal fleet (#3374). Dummy
//! coders open their change requests there over the Forgejo API, as a real
//! crew would, so numbers come from one sequence for the whole fleet. The
//! daemon then observes them through its ordinary Forgejo change-request
//! provider.

use std::sync::Arc;

use flotilla_config::dummy_fleet::DummySharedForgeConfig;
use flotilla_forge_forgejo::{ForgejoAuth, ForgejoClient, ForgejoIssueProviderConfig};
use flotilla_provider_runtime::HttpClient;
use serde_json::json;

pub(crate) struct SharedForge {
    config: DummySharedForgeConfig,
    http: Arc<dyn HttpClient>,
}

impl SharedForge {
    pub(crate) fn new(config: DummySharedForgeConfig, http: Arc<dyn HttpClient>) -> Self {
        Self { config, http }
    }

    /// Open (or find) the pull request for `branch` on `repository`
    /// (`owner/name`), returning its web URL. A branch keeps one pull request,
    /// so a crew resumed after a restart finds the one it opened.
    pub(crate) async fn open(&self, repository: &str, branch: &str, title: &str, body: &str) -> Result<String, String> {
        // The token is read on each use, so a rotated token file takes effect.
        // It is a few bytes on local disk, read once per opened change request.
        let token_path = std::path::PathBuf::from(&self.config.token_path);
        let token = std::fs::read_to_string(&token_path)
            .map_err(|error| format!("shared forge token {}: {error}", token_path.display()))?
            .trim()
            .to_string();
        if token.is_empty() {
            return Err(format!("shared forge token {} is empty", token_path.display()));
        }
        let mut config = ForgejoIssueProviderConfig::new(
            self.config.service_url.clone(),
            self.config.endpoint_url.as_ref().map(|endpoint| format!("{}/api/v1", endpoint.trim_end_matches('/'))),
            ForgejoAuth { token, token_path },
        );
        config.kind = self.config.kind;
        let forge = ForgejoClient::new(Arc::clone(&self.http), config);
        let existing = forge
            .execute(
                repository,
                reqwest::Method::GET,
                "pulls",
                &[("state", "all".into()), ("limit", "50".into()), ("page", "1".into()), ("head", branch.into())],
                None,
            )
            .await?;
        // Forgejo's `head` filter matches the exact branch (recorded lab
        // evidence: forgejo_head_filter_capability.yaml). Check the head ref
        // anyway, so a forge that ignored the filter cannot hand back another
        // branch's pull request.
        // One page suffices: a branch has at most one pull request here,
        // because a coder reuses the one it finds instead of opening another.
        if let Some(pull) = existing.as_array().into_iter().flatten().find(|pull| pull["head"]["ref"].as_str() == Some(branch)) {
            return web_url(&forge.config, repository, pull);
        }
        let created = forge
            .execute(
                repository,
                reqwest::Method::POST,
                "pulls",
                &[],
                Some(json!({"head": branch, "base": self.config.base_branch, "title": title, "body": body})),
            )
            .await?;
        web_url(&forge.config, repository, &created)
    }
}

fn web_url(config: &ForgejoIssueProviderConfig, repository: &str, pull: &serde_json::Value) -> Result<String, String> {
    let number = pull["number"].as_u64().ok_or_else(|| format!("shared forge pull request has no number: {pull}"))?;
    Ok(config.change_request_url(repository, &number.to_string()))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flotilla_fake_forge::{FakeForge, ForgeSettings, InProcessClient, RepoRef, SystemClock};
    use hegel::generators as gs;

    use super::*;

    const PUBLIC_URL: &str = "https://fake-forge.test:3443";
    const TOKEN: &str = "rehearsal-token";

    fn fake() -> Arc<FakeForge> {
        let fake = Arc::new(FakeForge::new(PUBLIC_URL, Arc::new(SystemClock)));
        fake.add_forge("github", ForgeSettings { token: TOKEN.into(), login: "rehearsal-crew".into() }).expect("forge");
        fake
    }

    fn token_file(token: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("token file");
        writeln!(file, "{token}").expect("write token");
        file
    }

    /// One host's view of the shared forge: its own HTTP client and config.
    fn host(fake: &Arc<FakeForge>, token: &tempfile::NamedTempFile) -> SharedForge {
        SharedForge::new(
            DummySharedForgeConfig {
                service_url: fake.service_url("github"),
                endpoint_url: None,
                kind: flotilla_resources::ForgeKind::Forgejo,
                token_path: token.path().to_string_lossy().into_owned(),
                base_branch: "main".into(),
            },
            Arc::new(InProcessClient::new(Arc::clone(fake))),
        )
    }

    // A dummy crew submits a logical GitHub claim even though the shared
    // Forgejo fake allocates the number. Resuming finds the same claim.
    #[tokio::test]
    async fn rehearsal_claim_uses_original_forge_url() {
        let fake = fake();
        let token = token_file(TOKEN);
        let shared = SharedForge::new(
            DummySharedForgeConfig {
                service_url: "https://github.com".into(),
                endpoint_url: Some(fake.service_url("github")),
                kind: flotilla_resources::ForgeKind::Github,
                token_path: token.path().to_string_lossy().into_owned(),
                base_branch: "main".into(),
            },
            Arc::new(InProcessClient::new(Arc::clone(&fake))),
        );
        let first = shared.open("flotilla-org/flotilla", "dummy/branch", "title", "body").await.expect("open");
        assert_eq!(first, "https://github.com/flotilla-org/flotilla/pull/1");
        assert_eq!(shared.open("flotilla-org/flotilla", "dummy/branch", "title", "body").await.expect("resume"), first);
        assert_eq!(fake.refusals().1, 0);
    }

    // Behaviour (#3374): crews on different hosts that open change requests on
    // the shared forge get one number sequence per repository, so no two
    // hosts produce the same number (the silo rehearsal's `fleet!1` on both
    // kiwi and feta). Opening again for a branch, from any host and after a
    // merge, finds the branch's pull request instead of opening another. The
    // generator draws 1..=10 openings, each from one of three hosts for one of
    // four branches, so repeats across hosts and fresh branches both occur.
    #[hegel::test]
    fn hosts_share_one_number_sequence(tc: hegel::TestCase) {
        let openings: Vec<(usize, usize, bool)> = (0..tc.draw(gs::integers::<usize>().min_value(1).max_value(10)))
            .map(|_| {
                (
                    tc.draw(gs::integers::<usize>().min_value(0).max_value(2)),
                    tc.draw(gs::integers::<usize>().min_value(0).max_value(3)),
                    tc.draw(gs::booleans()),
                )
            })
            .collect();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let fake = fake();
            let token = token_file(TOKEN);
            let hosts = [host(&fake, &token), host(&fake, &token), host(&fake, &token)];
            let repo = RepoRef::new("github", "flotilla-org", "flotilla");
            let mut urls_by_branch = std::collections::BTreeMap::new();
            for (host, branch, merge_after) in openings {
                let branch = format!("dummy/convoy-{branch}/work");
                let url = hosts[host].open("flotilla-org/flotilla", &branch, "Dummy work", "").await.expect("open");
                let known = urls_by_branch.entry(branch.clone()).or_insert_with(|| url.clone());
                assert_eq!(&url, known, "{branch} keeps one pull request across hosts");
                let number: u64 = url.rsplit('/').next().and_then(|number| number.parse().ok()).expect("number");
                assert_eq!(url, format!("{PUBLIC_URL}/github/flotilla-org/flotilla/pulls/{number}"));
                if merge_after {
                    let _ = fake.merge(&repo, number, "merge", None);
                }
                let distinct: std::collections::BTreeSet<&String> = urls_by_branch.values().collect();
                assert_eq!(distinct.len(), urls_by_branch.len(), "distinct branches never share a number");
                assert_eq!(fake.repository(&repo).expect("repository").items.len(), urls_by_branch.len());
            }
        });
    }

    // Behaviour: a token the forge rejects, or a missing token file, fails the
    // open with the reason, leaving the forge unchanged, so the crew can stall
    // with it rather than submit a reference that does not exist.
    #[tokio::test]
    async fn rejected_or_missing_tokens_fail_the_open() {
        let fake = fake();
        let wrong = token_file("guess");
        let error = host(&fake, &wrong).open("flotilla-org/flotilla", "dummy/a", "A", "").await.expect_err("rejected token");
        assert!(error.contains("401"), "{error}");
        let empty = token_file("");
        let error = host(&fake, &empty).open("flotilla-org/flotilla", "dummy/a", "A", "").await.expect_err("empty token");
        assert!(error.contains("is empty"), "{error}");
        let missing = SharedForge::new(
            DummySharedForgeConfig {
                service_url: fake.service_url("github"),
                endpoint_url: None,
                kind: flotilla_resources::ForgeKind::Forgejo,
                token_path: "/nonexistent/token".into(),
                base_branch: "main".into(),
            },
            Arc::new(InProcessClient::new(Arc::clone(&fake))),
        );
        assert!(missing.open("flotilla-org/flotilla", "dummy/a", "A", "").await.is_err());
        assert_eq!(fake.refusals().1, 1, "only the wrong token reached the forge");
        assert!(fake.repository(&RepoRef::new("github", "flotilla-org", "flotilla")).expect("repository").items.is_empty());
    }
}
