use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use flotilla_protocol::HostName;
use flotilla_resources::{InMemoryBackend, Repository, RepositorySpec, ResourceBackend};

use super::support::test_meta;
use crate::config::ConfigStore;
use crate::discovery_api::EnvironmentAssertion;
use crate::discovery_api::EnvironmentBag;
use crate::in_process::InProcessDaemon;
use crate::providers::change_request::observation::ChangeRequestObservationSource;
use crate::providers::change_request::observation::ChangeRequestRef;
use crate::providers::ChannelLabel;
use crate::providers::CommandOutput;
use crate::providers::CommandRunner;
use crate::testkits::discovery::fake_discovery_with_runner;

#[derive(Default)]
struct BusyObservationRunner {
    pages: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl CommandRunner for BusyObservationRunner {
    async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        Ok("test version".into())
    }
    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }
    async fn run_output(
        &self,
        cmd: &str,
        args: &[&str],
        cwd: &Path,
        label: &ChannelLabel,
    ) -> Result<crate::providers::CommandOutput, String> {
        if cmd == "gh" && args.first() == Some(&"api") && args.get(1) == Some(&"--include") {
            let number =
                args.get(2).ok_or("REST endpoint")?.rsplit('/').next().ok_or("PR number")?.parse::<u64>().map_err(|e| e.to_string())?;
            return Ok(CommandOutput {
                stdout: format!(
                    "HTTP/2 200 OK\r\nETag: busy\r\n\r\n{}",
                    serde_json::json!({"number":number,"updated_at":"2026-10-07T00:00:00Z"})
                ),
                stderr: String::new(),
                exit_code: Some(0),
            });
        }
        if cmd != "gh" || args.first() != Some(&"api") || args.get(1) != Some(&"graphql") {
            return self.run(cmd, args, cwd, label).await.map(|stdout| crate::providers::CommandOutput {
                stdout,
                stderr: String::new(),
                exit_code: Some(0),
            });
        }
        let query = args.iter().find_map(|arg| arg.strip_prefix("query=")).ok_or("query")?;
        let comments = serde_json::json!({"pageInfo": {"hasPreviousPage": true, "startCursor": "more"}, "nodes": []});
        let document = if query.contains("pr:pullRequest") {
            self.pages.lock().expect("pages").push(query.into());
            serde_json::json!({"data": {"repository": {"pr": {"comments": comments}}}})
        } else {
            let busy = serde_json::json!({"state": "OPEN", "reviewDecision": null, "comments": comments,
                "reviews": {"nodes": []}, "reviewThreads": {"nodes": []}});
            serde_json::json!({"data": {"repository": {"pr1": busy, "pr2": busy, "pr3": busy}}})
        };
        Ok(crate::providers::CommandOutput {
            stdout: format!("HTTP/2 200 OK\r\n\r\n{document}"),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
}

#[tokio::test(start_paused = true)]
async fn source_pagination_fairness_survives_provider_rediscovery() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), r#"machine_id = "fair-pagination-test""#).expect("daemon config");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let repository = RepositorySpec::remote("https://github.com/team/one").expect("repository");
    backend.using::<Repository>("flotilla").create(&test_meta(&repository.key().to_string()), &repository).await.expect("repository");
    let runner = Arc::new(BusyObservationRunner::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery_with_runner(false, runner.clone()),
        HostName::new("test-host"),
        backend,
    )
    .await;
    daemon
        .replace_local_environment_bag_for_test(EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh")))
        .expect("capability");
    let subjects = [1, 2, 3].map(|number| ChangeRequestRef {
        namespace: "flotilla".into(),
        service: "github.com".into(),
        scope: "team/one".into(),
        number,
    });
    for _ in 0..4 {
        let error = daemon.change_request_observation_source.observe_group(&subjects, &subjects[0]).await.expect_err("incomplete history");
        assert!(error.to_string().contains("pagination budget"));
        tokio::time::advance(Duration::from_secs(10)).await;
    }
    let pages = runner.pages.lock().expect("pages");
    assert_eq!(pages.len(), 32, "all four cycles retain the eight-page shared budget");
    for (cycle, number) in [1, 2, 3, 1].into_iter().enumerate() {
        assert!(pages[cycle * 8].contains(&format!("number:{number}")), "priority rotates through every PR and wraps across rediscovery");
    }
}

use crate::testkits::discovery::InProcessDiscoveryExt;
