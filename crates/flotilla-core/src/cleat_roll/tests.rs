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
        let mut outputs = vec![Ok(output)];
        if name == "nonzero" {
            outputs.push(Err("discovery unavailable".into()));
        }
        outputs.push(Ok(recorded("unchanged")));
        let runner = Arc::new(MockRunner::with_outputs(outputs));
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

// #2671: contained toolchains stay pinned until restart. Even a registered
// runner must receive no file copies, subprocesses or daemon mutations.
#[tokio::test]
async fn contained_drain_is_expected_and_never_executes() {
    let runner = Arc::new(MockRunner::with_outputs(vec![]));
    let report =
        drain("host".into(), "new-gen".into(), Path::new("/incoming/bin/cleat"), &[target(runner.clone(), "default", true)], vec![]).await;
    assert!(!report.failed());
    assert!(report.attempts.is_empty());
    assert!(report.information[0].contains("refreshes on restart"));
    assert!(runner.calls().is_empty());
}

// Host health compares the host-installed SHA with each current serving SHA;
// matching builds are healthy, differing or unavailable builds remain visible.
#[tokio::test]
async fn serving_sha_skew_is_visible_per_runtime() {
    for (installed, serving, expected) in
        [(Some("new"), Some("new"), false), (Some("new"), Some("old"), true), (Some("new"), None, false), (None, Some("old"), false)]
    {
        // Subprocess boundary: aliases are absent, version metadata is injected,
        // and daemon discovery reports an empty legacy runtime.
        let runner = Arc::new(MockRunner::with_outputs(vec![
            Err("no sidecar".into()),
            Err("legacy".into()),
            Ok(CommandOutput {
                stdout: serde_json::json!({"daemon":{"git_sha":serving}}).to_string(),
                stderr: String::new(),
                success: true,
            }),
            Ok(CommandOutput { stdout: "[]".into(), stderr: String::new(), success: true }),
        ]));
        let messages = build_skew(installed, &[target(runner.clone(), "named", false)], None).await.actionable;
        assert_eq!(!messages.is_empty(), expected);
        if expected {
            assert!(messages[0].contains("crew-env /state/crew cleat/named"));
            assert!(messages[0].contains(&format!(
                "installed {} vs serving {}",
                installed.unwrap_or("unknown"),
                serving.unwrap_or("unknown")
            )));
        }
        assert_eq!(runner.calls()[2].1, vec!["--runtime-root", "/state/crew cleat", "--server", "named", "version", "--daemon", "--json"]);
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
    let stored: RollReport =
        serde_json::from_slice(&std::fs::read(path).expect("stored report immediately after persistence")).expect("decode report");
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

// Successful drain evidence compares installed with current, never old. A
// host-level warning/failure remains actionable even when version is unknown.
#[tokio::test]
async fn recorded_drain_reports_keep_host_failures_actionable() {
    for name in ["success", "unchanged", "warning", "nonzero"] {
        let mut outputs = vec![Ok(recorded(name))];
        if name == "nonzero" {
            outputs.push(Err("discovery unavailable".into()));
        }
        let runner = Arc::new(MockRunner::with_outputs(outputs));
        let target = target(runner, "default", false);
        let report = drain("host".into(), "gen".into(), Path::new("/incoming/cleat"), std::slice::from_ref(&target), vec![]).await;
        let assessment = assess_drain(&report);
        assert_eq!(!assessment.actionable.is_empty(), matches!(name, "warning" | "nonzero"));
        if let Some(drain) = &report.attempts[0].report {
            let current = serde_json::json!({"daemon": drain.current["build"]});
            assert!(assess_current(sha(&drain.installed), &target, &current).actionable.is_empty());
        }
    }
}

#[derive(Deserialize)]
struct RecordedGenerations {
    drain: RecordedOutput,
    installed: Value,
    current: RecordedOutput,
    listing: RecordedOutput,
    empty: RecordedOutput,
    stale: RecordedOutput,
}

fn generations() -> RecordedGenerations {
    serde_json::from_str(include_str!("fixtures/generations.json")).expect("recorded generations")
}

fn output(record: RecordedOutput) -> CommandOutput {
    CommandOutput { stdout: record.stdout.replace("{root}", "/state/crew cleat"), stderr: record.stderr, success: record.success }
}

// #2671: the real CLI's alias and listing recordings reproduce the r531 shape:
// a new current generation alongside an alive, older draining generation. The
// latter is informational, and unrelated roots must not contaminate health.
#[tokio::test]
async fn recorded_generations_report_draining_without_degrading() {
    let records = generations();
    let installed = sha(&records.installed).expect("installed").to_string();
    // Subprocess boundary: exact drain/version/listing outputs from the CLI;
    // only the alias read is supplied separately.
    let runner = Arc::new(MockRunner::with_outputs(vec![
        Ok(output(records.drain)),
        Ok(CommandOutput { stdout: "default@27\n".into(), stderr: String::new(), success: true }),
        Ok(output(records.current)),
        Ok(output(records.listing)),
    ]));
    let target = target(runner.clone(), "default", false);
    let report = drain("host".into(), "r531".into(), Path::new("/incoming/cleat"), std::slice::from_ref(&target), vec![]).await;
    assert!(!report.failed());
    let assessment = build_skew(Some(&installed), &[target], Some(&report)).await;
    assert!(assessment.actionable.is_empty(), "{assessment:?}");
    assert_eq!(assessment.information.len(), 1);
    assert!(assessment.information[0].contains("draining 3 sessions on fd66a712"));
    assert_eq!(runner.calls()[1].1, ["/state/crew cleat/default"]);
    assert_eq!(runner.calls()[2].1[3], "default@27");
    assert_eq!(runner.remaining(), 0);
}

// Generate every observation class (current/stale/absent/invalid), container
// status and installed-known boundary. Old sessions and pinned vessels must
// never change whether the host's current alias has actionable skew.
#[hegel::test]
fn recorded_current_observations_degrade_only_actionable_host_skew(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let class = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let contained = tc.draw(gs::booleans());
    let installed_known = tc.draw(gs::booleans());
    let records = generations();
    let current = match class {
        0 => parsed(Ok(output(records.current))).expect("current"),
        1 => parsed(Ok(output(records.stale))).expect("stale"),
        2 => parsed(Ok(output(records.empty))).unwrap_or(Value::Null),
        _ => Value::Null,
    };
    let target = target(Arc::new(MockRunner::with_outputs(vec![])), "default", contained);
    let installed = if installed_known { sha(&records.installed) } else { None };
    let assessment = assess_current(installed, &target, &current);
    assert_eq!(!assessment.actionable.is_empty(), class == 1 && installed_known && !contained);
    let informational = contained || !installed_known || class >= 2;
    assert_eq!(assessment.information.len(), usize::from(informational));
    if contained {
        assert!(assessment.information[0].contains("refreshes on restart"));
    }
    let condition = assessment.into_condition(chrono::Utc::now());
    if class == 1 && installed_known && !contained {
        assert_eq!(condition.expect("actionable condition").value, flotilla_resources::ConditionValue::False);
    } else if informational {
        assert_eq!(condition.expect("informational condition").value, flotilla_resources::ConditionValue::True);
    } else {
        assert!(condition.is_none());
    }
}

// #2671: a failed connection with a confirmed empty daemon inventory is a
// healthy no-op; unavailable discovery must still retain the host drain error.
#[tokio::test]
async fn absent_host_daemon_drain_does_not_warn() {
    for (listing, absent) in [
        (Ok(CommandOutput { stdout: "[]".into(), stderr: String::new(), success: true }), true),
        (
            Ok(CommandOutput {
                stdout: serde_json::json!([
                    {"name":"other@26", "runtime_root":"/state/crew cleat", "alive":true},
                    {"name":"default@26", "runtime_root":"/another/root", "alive":true},
                    {"name":"default@25", "runtime_root":"/state/crew cleat", "alive":false}
                ])
                .to_string(),
                stderr: String::new(),
                success: true,
            }),
            true,
        ),
        (Ok(CommandOutput { stdout: "invalid".into(), stderr: String::new(), success: true }), false),
        (Ok(CommandOutput { stdout: "{}".into(), stderr: String::new(), success: true }), false),
        (Err("listing unavailable".into()), false),
    ] {
        let runner = Arc::new(MockRunner::with_outputs(vec![Ok(recorded("nonzero")), listing]));
        let report =
            drain("host".into(), "gen".into(), Path::new("/incoming/cleat"), &[target(runner.clone(), "default", false)], vec![]).await;
        assert_eq!(report.failed(), !absent);
        assert_eq!(assess_drain(&report).actionable.is_empty(), absent);
        assert_eq!(runner.remaining(), 0);
    }
}

// Unix aliases select the physical current generation. Legacy directory
// sidecars are consulted only when the main path is not a symlink; stale
// sidecars must never override a valid main alias.
#[tokio::test]
async fn current_alias_precedes_legacy_sidecar() {
    for (main, sidecar, expected) in [
        (Some("default@27"), None, "default@27"),
        (None, Some("default@28"), "default@28"),
        (None, None, "default"),
        (Some("../default@26"), None, "default"),
    ] {
        // Subprocess boundary: readlink results, including legacy/missing paths.
        let result = |value: Option<&str>| {
            value.map_or_else(
                || Err("not a symlink".into()),
                |value| Ok(CommandOutput { stdout: format!("{value}\n"), stderr: String::new(), success: true }),
            )
        };
        let mut outputs = vec![result(main)];
        if main != Some("default@27") {
            outputs.push(result(sidecar));
        }
        let runner = Arc::new(MockRunner::with_outputs(outputs));
        let current = current_target(&target(runner.clone(), "default", false)).await;
        assert_eq!(current.name, expected);
        assert_eq!(runner.calls()[0].1, ["/state/crew cleat/default"]);
        if main != Some("default@27") {
            assert_eq!(runner.calls()[1].1, ["/state/crew cleat/.default.current"]);
        }
        assert_eq!(runner.remaining(), 0);
    }
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
            .errors(if index % 2 == 0 { vec![] } else { vec!["diagnostic".repeat(1024)] })
            .build();
        persist(&mut report, directory.path()).await;
        let path = report.diagnostics_path.as_ref().expect("retained report");
        let bytes = std::fs::read(path).expect("read immediately");
        let stored: serde_json::Value = serde_json::from_slice(&bytes).expect("complete report JSON");
        assert_eq!(stored, serde_json::to_value(&report).expect("returned report"));
    }
}

// #2693: an absent current socket is informational when older generations
// remain alive: report waiting for their sessions, whether serving or draining.
// The subprocess double supplies discovery; unrelated roots/names and dead
// generations must not turn an empty target into a waiting target.
#[tokio::test]
async fn absent_current_reports_waiting_for_live_generations() {
    for state in ["serving", "draining"] {
        let listing = serde_json::json!([
            {"name":"default@26", "runtime_root":"/state/crew cleat", "alive":true, "drain_state":state},
            {"name":"default@25", "runtime_root":"/state/crew cleat", "alive":true, "drain_state":"draining"},
            {"name":"default@24", "runtime_root":"/state/crew cleat", "alive":false},
            {"name":"other@26", "runtime_root":"/state/crew cleat", "alive":true},
            {"name":"default@26", "runtime_root":"/another/root", "alive":true}
        ]);
        let runner = Arc::new(MockRunner::with_outputs(vec![
            Ok(recorded("nonzero")),
            Ok(CommandOutput { stdout: listing.to_string(), stderr: String::new(), success: true }),
        ]));
        let report =
            drain("host".into(), "gen".into(), Path::new("/incoming/cleat"), &[target(runner.clone(), "default", false)], vec![]).await;
        assert!(!report.failed(), "{report:?}");
        assert!(assess_drain(&report).actionable.is_empty());
        assert_eq!(report.information.len(), 2);
        for (message, name) in report.information.iter().zip(["default@26", "default@25"]) {
            assert!(message.contains(name), "{message}");
            assert!(message.contains("waiting for sessions to end"), "{message}");
        }
        assert!(!report.attempts[0].success, "retain the actual CLI exit status");
        assert!(report.attempts[0].stderr.starts_with("connect daemon:"));
        assert_eq!(runner.remaining(), 0);
    }
}

// A permission failure is actionable even when older generations might be
// alive: only an absent/unserved socket warrants the waiting classification.
#[tokio::test]
async fn inaccessible_current_socket_remains_actionable() {
    // Subprocess boundary: the CLI cannot access its socket.
    let runner = Arc::new(MockRunner::with_outputs(vec![Ok(CommandOutput {
        stdout: String::new(),
        stderr: "connect daemon: Permission denied (os error 13)".into(),
        success: false,
    })]));
    let report =
        drain("host".into(), "gen".into(), Path::new("/incoming/cleat"), &[target(runner.clone(), "default", false)], vec![]).await;
    assert!(report.failed());
    assert!(report.information.is_empty());
    assert_eq!(runner.remaining(), 0);
}
