//! REST observation fixture shared by admission, completion, branch, and cooldown scenarios.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use flotilla_protocol::HostName;
use flotilla_resources::{InMemoryBackend, Repository, RepositoryKey, RepositorySpec, ResourceBackend};

use super::support::test_meta;
use crate::config::ConfigStore;
use crate::in_process::convoy_admission::RepositoryChangeRequestProvider;
use crate::in_process::InProcessDaemon;
use crate::providers::{CommandOutput, CommandRunner};
use crate::testkits::discovery::fake_discovery;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RestAdmissionReply {
    Ordinary,
    Limited,
    NoDeadline,
    Secondary,
    Absent,
    MissingBase,
    Success,
}

#[derive(Clone, Copy)]
pub(super) enum RestAdmissionLookup {
    Id,
    Branch,
}

pub(super) enum RestAdmissionSelection {
    Failure(RestAdmissionReply),
    Absent,
    Found(usize),
    Ambiguous,
}

// GitHub subprocess boundary: retain failed stdout headers as the real CLI does.
pub(super) struct AdmissionRestRunner {
    pub(super) responses: BTreeMap<String, CommandOutput>,
    pub(super) calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl CommandRunner for AdmissionRestRunner {
    async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &crate::providers::ChannelLabel) -> Result<String, String> {
        panic!("admission REST reads use run_output")
    }

    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        _cwd: &Path,
        _label: &crate::providers::ChannelLabel,
    ) -> Result<CommandOutput, String> {
        assert_eq!(cmd, "gh");
        self.calls.fetch_add(1, Ordering::SeqCst);
        let endpoint = args.iter().find(|arg| arg.starts_with("repos/")).expect("REST endpoint");
        let scope = endpoint.strip_prefix("repos/").expect("repository path").split("/pulls").next().expect("scope");
        let response = self.responses.get(scope).expect("configured repository");
        Ok(CommandOutput { stdout: response.stdout.clone(), stderr: response.stderr.clone(), exit_code: response.exit_code })
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }
}

pub(super) fn rest_admission_response(reply: RestAdmissionReply, lookup: RestAdmissionLookup) -> CommandOutput {
    use RestAdmissionReply::*;
    let (status, headers, body, success) = match reply {
        Limited => (
            403,
            "X-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 1893456000\r\n",
            serde_json::json!({"message":"API rate limit exceeded"}),
            false,
        ),
        NoDeadline => (403, "X-RateLimit-Remaining: 0\r\n", serde_json::json!({"message":"API rate limit exceeded"}), false),
        Secondary => {
            (403, "X-RateLimit-Remaining: 4989\r\nRetry-After: 30\r\n", serde_json::json!({"message":"secondary rate limit"}), false)
        }
        Ordinary => (
            403,
            "X-RateLimit-Remaining: 4989\r\nX-RateLimit-Reset: 1893456000\r\n",
            serde_json::json!({"message":"Resource not accessible by integration"}),
            false,
        ),
        Absent => match lookup {
            RestAdmissionLookup::Branch => (200, "", serde_json::json!([]), true),
            RestAdmissionLookup::Id => (404, "", serde_json::json!({"message":"Not Found"}), false),
        },
        MissingBase | Success => {
            let pr = serde_json::json!({"number":7,"title":"Wanted","head":{"ref":"feature/wanted"},"base":{"ref": if reply == MissingBase { serde_json::Value::Null } else { serde_json::json!("main") }},"state":"open"});
            (
                200,
                "",
                match lookup {
                    RestAdmissionLookup::Branch => serde_json::json!([pr]),
                    RestAdmissionLookup::Id => pr,
                },
                true,
            )
        }
    };
    CommandOutput {
        stdout: format!("HTTP/2 {status}\r\n{headers}\r\n{body}"),
        stderr: if reply == Ordinary { "rate limited diagnostics unavailable".into() } else { "gh: Not Found".into() },
        exit_code: Some(if success { 0 } else { 1 }),
    }
}

pub(super) struct RestAdmissionFixture {
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) daemon: Arc<InProcessDaemon>,
    pub(super) keys: Vec<RepositoryKey>,
    pub(super) _config: tempfile::TempDir,
}

pub(super) async fn rest_admission_fixture(outcomes: [RestAdmissionReply; 2], lookup: RestAdmissionLookup) -> RestAdmissionFixture {
    use crate::providers::{change_request::github::GitHubChangeRequest, github_api::GhApiClient};
    let config = tempfile::tempdir().expect("tempdir");
    std::fs::write(config.path().join("daemon.toml"), "machine_id = \"rest-admission-test\"\n").expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let responses = outcomes
        .into_iter()
        .enumerate()
        .map(|(index, reply)| (format!("team/repo{index}"), rest_admission_response(reply, lookup)))
        .collect();
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = Arc::new(AdmissionRestRunner { responses, calls: Arc::clone(&calls) });
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(config.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
    )
    .await;
    daemon.set_provisioning_namespace("flotilla".into()).await;
    let mut keys = Vec::new();
    for index in 0..2 {
        let scope = format!("team/repo{index}");
        let repository = RepositorySpec::remote(format!("https://github.com/{scope}")).expect("repository");
        let key = repository.key();
        backend.using::<Repository>("flotilla").create(&test_meta(&key.to_string()), &repository).await.expect("repository");
        daemon.convoy_admission.repository_change_requests.write().await.insert(
            key.clone(),
            RepositoryChangeRequestProvider {
                service_url: repository.forge().expect("forge").service_url.clone(),
                repository: scope.clone(),
                provider: Arc::new(GitHubChangeRequest::new(
                    "github".into(),
                    scope,
                    Arc::new(GhApiClient::new(runner.clone())),
                    runner.clone(),
                )),
            },
        );
        keys.push(key);
    }
    RestAdmissionFixture { calls, daemon, keys, _config: config }
}
