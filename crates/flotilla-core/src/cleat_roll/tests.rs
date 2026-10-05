use std::sync::Mutex;

use async_trait::async_trait;

use super::*;
use crate::providers::{
    discovery::{EnvironmentAssertion, EnvironmentBag},
    testing::MockRunner,
};

#[derive(Deserialize)]
struct RecordedOutput {
    stdout: String,
    stderr: String,
    success: bool,
}

fn recorded(name: &str) -> CommandOutput {
    let json = match name {
        "success" => include_str!("fixtures/success.json"),
        "unchanged" => include_str!("fixtures/unchanged.json"),
        "warning" => include_str!("fixtures/warning.json"),
        "nonzero" => include_str!("fixtures/nonzero.json"),
        _ => panic!("unknown recording"),
    };
    let output: RecordedOutput = serde_json::from_str(json).expect("recorded output");
    CommandOutput { stdout: output.stdout, stderr: output.stderr, success: output.success }
}

fn target(runner: Arc<dyn CommandRunner>, name: &str, contained: bool) -> CleatTarget {
    CleatTarget::builder()
        .environment("crew-env".into())
        .runtime_root(PathBuf::from("/state/crew cleat"))
        .name(name.into())
        .binary("/tools/cleat".into())
        .runner(runner)
        .contained(contained)
        .build()
}

// #2041: each recorded drain result is retained. Unchanged is successful;
// warnings and unsuccessful exits fail the roll without suppressing later targets.
// Boundary table: exhaust the four pinned CLI outcomes against an injected runner.
#[tokio::test]
async fn recorded_drain_outcomes_preserve_reports_and_continue() {
    for (name, failed, changed) in
        [("success", false, Some(true)), ("unchanged", false, Some(false)), ("warning", true, Some(true)), ("nonzero", true, None)]
    {
        let output = recorded(name);
        let expected_stdout = output.stdout.clone();
        let runner = Arc::new(MockRunner::with_outputs(vec![Ok(output), Ok(recorded("unchanged"))]));
        let targets = [target(runner.clone(), "default", false), target(runner.clone(), "named", false)];
        let report = drain("host".into(), "generation-2".into(), Path::new("/incoming/bin/cleat"), &targets, vec![]).await;
        assert_eq!(report.failed(), failed, "{name}: {report:?}");
        assert_eq!(report.attempts.len(), 2);
        assert_eq!(report.attempts[0].stdout, expected_stdout);
        assert_eq!(report.attempts[0].report.as_ref().map(|report| report.changed), changed);
        if let Some(parsed) = &report.attempts[0].report {
            let original: Value = serde_json::from_str(&expected_stdout).expect("drain JSON");
            assert_eq!(serde_json::to_value(parsed).expect("serialize"), original);
        }
        assert!(report.attempts[1].error.is_none());
        assert_eq!(runner.remaining(), 0);
        assert_eq!(
            runner.calls()[0],
            ("/incoming/bin/cleat".into(), vec![
                "--runtime-root".into(),
                "/state/crew cleat".into(),
                "--server".into(),
                "default".into(),
                "server".into(),
                "drain".into(),
                "--json".into()
            ])
        );
    }
}

// The roll surfaces spawn errors and malformed successful JSON and keeps the
// raw stdout/stderr evidence. Already-known inventory errors also fail the roll.
#[tokio::test]
async fn invalid_and_unavailable_drain_evidence_fails_closed() {
    let runner = Arc::new(MockRunner::with_outputs(vec![
        Err("spawn refused".into()),
        Ok(CommandOutput { stdout: "invalid JSON".into(), stderr: "warning text".into(), success: true }),
    ]));
    let report = drain(
        "host".into(),
        "gen".into(),
        Path::new("/incoming/cleat"),
        &[target(runner.clone(), "first", false), target(runner.clone(), "second", false)],
        vec!["missing environment runner".into()],
    )
    .await;
    assert!(report.failed());
    assert_eq!(report.attempts[0].error.as_deref(), Some("spawn refused"));
    assert!(report.attempts[1].error.as_deref().expect("error").contains("invalid drain JSON"));
    assert_eq!(report.attempts[1].stdout, "invalid JSON");
    assert_eq!(report.attempts[1].stderr, "warning text");
    assert_eq!(runner.remaining(), 0);
}

// The inventory uses registered environment assertions and recorded crew
// endpoints, including private roots, and collapses physical generations.
#[test]
fn crew_inventory_uses_owned_roots_and_logical_names() {
    let runner = Arc::new(MockRunner::with_outputs(vec![]));
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("CLEAT_RUNTIME_DIR", "/contained-cleat/work"))
        .with(EnvironmentAssertion::env_var("CLEAT_DAEMON", "named@4"));
    let environment =
        CleatEnvironment::builder().id(EnvironmentId::new("contained-work")).bag(bag).runner(runner.clone()).contained(true).build();
    let endpoints =
        [CleatEndpoint { runtime_root: "/named private/root".into(), daemon: "other@1".into(), session: "first".into() }, CleatEndpoint {
            runtime_root: "/named private/root".into(),
            daemon: "other@2".into(),
            session: "second".into(),
        }];
    let targets = crew_targets(&environment, &endpoints).expect("targets");
    assert_eq!(targets.iter().map(|target| (target.runtime_root.display().to_string(), target.name.clone())).collect::<Vec<_>>(), vec![
        ("/contained-cleat/work".into(), "default".into()),
        ("/contained-cleat/work".into(), "named".into()),
        ("/named private/root".into(), "other".into())
    ]);
    assert!(targets.iter().all(|target| target.contained && target.environment == "contained-work"));
    assert!(runner.calls().is_empty(), "inventory must not rediscover daemons in shell");
    assert_eq!(crew_targets(&environment, &[]).expect("empty inventory").len(), 2);
}

// Persistent root precedence is CLEAT_RUNTIME_DIR, absolute XDG_STATE_HOME,
// then the registered HOME. Unknown or relative roots are refused.
#[test]
fn default_runtime_roots_follow_injected_environment() {
    for (state, home, expected) in [
        (Some("/xdg"), Some("/home"), Some("/xdg/cleat")),
        (Some("relative"), Some("/home"), Some("/home/.local/state/cleat")),
        (None, Some("/home"), Some("/home/.local/state/cleat")),
        (None, None, None),
    ] {
        let mut bag = EnvironmentBag::new();
        if let Some(state) = state {
            bag = bag.with(EnvironmentAssertion::env_var("XDG_STATE_HOME", state));
        }
        if let Some(home) = home {
            bag = bag.with(EnvironmentAssertion::env_var("HOME", home));
        }
        let environment = CleatEnvironment::builder()
            .id(EnvironmentId::new("host"))
            .bag(bag)
            .runner(Arc::new(MockRunner::with_outputs(vec![])))
            .contained(false)
            .build();
        match expected {
            Some(root) => assert_eq!(crew_targets(&environment, &[]).expect("root")[0].runtime_root, Path::new(root)),
            None => assert!(crew_targets(&environment, &[]).is_err()),
        }
    }
}

// Container daemons must start successors inside their execution environment,
// using the incoming CLI rather than the older read-only mount. Delivery happens
// once per environment/root even when it has several logical daemons.
#[tokio::test]
async fn contained_successor_uses_delivered_incoming_cli_once() {
    struct Runner {
        inner: MockRunner,
        copies: Mutex<Vec<(PathBuf, PathBuf)>>,
    }
    #[async_trait]
    impl CommandRunner for Runner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            self.inner.run(cmd, args, cwd, label).await
        }
        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            self.inner.run_output(cmd, args, cwd, label).await
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            false
        }
        async fn write_file_from(&self, source: &Path, destination: &Path) -> Result<(), String> {
            self.copies.lock().expect("copies").push((source.into(), destination.into()));
            Ok(())
        }
    }
    let empty = || Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success: true });
    let runner = Arc::new(Runner {
        inner: MockRunner::with_outputs(vec![empty(), empty(), Ok(recorded("success")), Ok(recorded("unchanged"))]),
        copies: Mutex::new(vec![]),
    });
    let report = drain(
        "host".into(),
        "new-gen".into(),
        Path::new("/incoming/bin/cleat"),
        &[target(runner.clone(), "default", true), target(runner.clone(), "named", true)],
        vec![],
    )
    .await;
    assert!(!report.failed(), "{report:?}");
    assert_eq!(*runner.copies.lock().expect("copies"), vec![
        (PathBuf::from("/incoming/bin/cleat"), PathBuf::from("/state/crew cleat/.fleet-bin/new-gen/bin/cleat")),
        (PathBuf::from("/incoming/lib/libghostty-vt.so.0"), PathBuf::from("/state/crew cleat/.fleet-bin/new-gen/lib/libghostty-vt.so.0")),
    ]);
    let calls = runner.inner.calls();
    assert_eq!(calls[2].0, "env");
    assert_eq!(calls[2].1[0], "LD_LIBRARY_PATH=/state/crew cleat/.fleet-bin/new-gen/lib");
    assert_eq!(calls[2].1[1], "/state/crew cleat/.fleet-bin/new-gen/bin/cleat");
    assert_eq!(calls[3].1[..2], calls[2].1[..2]);
}

// Host health compares the host-installed SHA with each current serving SHA;
// matching builds are healthy, differing or unavailable builds remain visible.
#[tokio::test]
async fn serving_sha_skew_is_visible_per_runtime() {
    for (installed, serving, expected) in
        [(Some("new"), Some("new"), false), (Some("new"), Some("old"), true), (Some("new"), None, true), (None, Some("old"), true)]
    {
        let runner = Arc::new(MockRunner::with_outputs(vec![Ok(CommandOutput {
            stdout: serde_json::json!({"daemon":{"git_sha":serving}}).to_string(),
            stderr: String::new(),
            success: true,
        })]));
        let messages = build_skew(installed, &[target(runner.clone(), "named", false)]).await;
        assert_eq!(!messages.is_empty(), expected);
        if expected {
            assert!(messages[0].contains("crew-env /state/crew cleat/named"));
            assert!(messages[0].contains(&format!(
                "installed {} vs serving {}",
                installed.unwrap_or("unknown"),
                serving.unwrap_or("unknown")
            )));
        }
        assert_eq!(runner.calls()[0].1, vec!["--runtime-root", "/state/crew cleat", "--server", "named", "version", "--daemon", "--json"]);
    }
}

// Diagnostics retain the exact report for a warning/partial failure. A failed
// diagnostic write itself fails the roll but never discards captured attempts.
#[tokio::test]
async fn diagnostics_retain_failed_attempts_and_surface_storage_errors() {
    let directory = tempfile::tempdir().expect("diagnostics directory");
    let runner = Arc::new(MockRunner::with_outputs(vec![Ok(recorded("warning"))]));
    let mut report = drain("host".into(), "gen".into(), Path::new("/incoming/cleat"), &[target(runner, "default", false)], vec![]).await;
    persist(&mut report, directory.path()).await;
    let path = report.diagnostics_path.as_ref().expect("diagnostics path");
    let stored: RollReport = serde_json::from_slice(&tokio::fs::read(path).await.expect("stored report")).expect("decode report");
    assert!(stored.failed());
    assert_eq!(stored.attempts[0].stdout, report.attempts[0].stdout);
    assert_eq!(stored.attempts[0].report, report.attempts[0].report);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(tokio::fs::metadata(path).await.expect("metadata").permissions().mode() & 0o777, 0o600);
    }
    let not_directory = directory.path().join("file");
    tokio::fs::write(&not_directory, b"occupied").await.expect("create regular file");
    persist(&mut report, &not_directory).await;
    assert!(report.failed());
    assert!(report.diagnostics_path.is_none());
    assert!(report.errors[0].contains("write roll diagnostics"));
    assert_eq!(report.attempts[0].report, stored.attempts[0].report);
}

// Returning a diagnostics path promises a complete readable report immediately,
// including to synchronous callers. An async read can hide a pending write race.
#[tokio::test(flavor = "current_thread")]
async fn retained_report_is_immediately_readable() {
    let directory = tempfile::tempdir().expect("diagnostics directory");
    for index in 0..256 {
        let mut report = RollReport::builder()
            .host("host".into())
            .generation(format!("generation-{index}"))
            .attempts(vec![])
            .errors(vec!["diagnostic".repeat(1024)])
            .build();
        persist(&mut report, directory.path()).await;
        let path = report.diagnostics_path.as_ref().expect("retained report");
        let bytes = std::fs::read(path).expect("read immediately");
        let stored: serde_json::Value = serde_json::from_slice(&bytes).expect("complete report JSON");
        assert_eq!(stored, serde_json::to_value(&report).expect("returned report"));
    }
}
