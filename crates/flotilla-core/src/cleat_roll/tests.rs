use super::*;
use crate::testkits::replay::testing::MockRunner;
use flotilla_discovery_api::EnvironmentAssertion;
use flotilla_discovery_api::EnvironmentBag;

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
    CommandOutput { stdout: output.stdout, stderr: output.stderr, exit_code: Some(if output.success { 0 } else { 1 }) }
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
            (
                "/incoming/bin/cleat".into(),
                vec![
                    "--runtime-root".into(),
                    "/state/crew cleat".into(),
                    "--server".into(),
                    "default".into(),
                    "server".into(),
                    "handover".into(),
                    "--json".into()
                ]
            )
        );
    }
}

// The roll surfaces spawn errors and malformed successful JSON and keeps the
// raw stdout/stderr evidence. Already-known inventory errors also fail the roll.
#[tokio::test]
async fn invalid_and_unavailable_drain_evidence_fails_closed() {
    let runner = Arc::new(MockRunner::with_outputs(vec![
        Err("spawn refused".into()),
        Ok(CommandOutput { stdout: "invalid JSON".into(), stderr: "warning text".into(), exit_code: Some(0) }),
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
    let endpoints = [
        CleatEndpoint { runtime_root: "/named private/root".into(), daemon: "other@1".into(), session: "first".into() },
        CleatEndpoint { runtime_root: "/named private/root".into(), daemon: "other@2".into(), session: "second".into() },
    ];
    let targets = crew_targets(&environment, &endpoints).expect("targets");
    assert_eq!(
        targets.iter().map(|target| (target.runtime_root.display().to_string(), target.name.clone())).collect::<Vec<_>>(),
        vec![
            ("/contained-cleat/work".into(), "default".into()),
            ("/contained-cleat/work".into(), "named".into()),
            ("/named private/root".into(), "other".into())
        ]
    );
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

#[cfg(target_os = "linux")]
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../ci/fleet-candidates/fixtures/guest_tools.rs"));

#[cfg(target_os = "linux")]
fn refresh_guest_catalog(release: &Path) {
    use sha2::{Digest, Sha256};
    let path = release.join("share/flotilla/guest-tools/catalog.json");
    let guest = release.join("share/flotilla/guest-tools/linux-amd64");
    let mut catalog: Value = serde_json::from_slice(&std::fs::read(&path).expect("catalog")).expect("catalog JSON");
    for file in catalog["platforms"]["linux/amd64"]["files"].as_array_mut().expect("guest files") {
        let bytes = std::fs::read(guest.join(file["path"].as_str().expect("file path"))).expect("guest bytes");
        file["sha256"] = format!("{:x}", Sha256::digest(&bytes)).into();
        file["size_bytes"] = bytes.len().into();
    }
    std::fs::write(path, serde_json::to_vec(&catalog).expect("updated catalog")).expect("catalog");
}

#[cfg(target_os = "linux")]
fn incoming_fixture() -> (tempfile::TempDir, PathBuf) {
    use std::process::Command;
    let directory = tempfile::tempdir().expect("fixture");
    let release = directory.path().join("release");
    write_guest_fixture(&release);
    let guest = release.join("share/flotilla/guest-tools/linux-amd64");
    let incoming = guest.join("bin/cleat");
    let library = guest.join("lib/libghostty-vt.so.0");
    std::fs::create_dir_all(incoming.parent().expect("bin")).expect("bin");
    std::fs::create_dir_all(library.parent().expect("lib")).expect("lib");
    let source = directory.path().join("library.c");
    std::fs::write(&source, "const char *generation(void) { return \"incoming\"; }\n").expect("library source");
    assert!(Command::new("cc")
        .args(["-shared", "-fPIC", "-Wl,-soname,libghostty-vt.so.0"])
        .arg(&source)
        .arg("-o")
        .arg(&library)
        .status()
        .expect("C compiler")
        .success());
    let source = directory.path().join("cleat.c");
    std::fs::write(&source, format!("#include <stdio.h>\n#include <string.h>\nextern const char *generation(void);\nint main(void) {{ if (strcmp(generation(), \"incoming\")) return 1; puts({}); return 0; }}\n", serde_json::to_string(&recorded("success").stdout).expect("C string"))).expect("executable source");
    assert!(Command::new("cc")
        .arg(&source)
        .arg("-L")
        .arg(library.parent().expect("lib"))
        .args(["-l:libghostty-vt.so.0", "-Wl,-rpath,$ORIGIN/../lib", "-o"])
        .arg(&incoming)
        .status()
        .expect("C compiler")
        .success());
    refresh_guest_catalog(&release);
    // Distinct controller bytes ensure contained delivery selects the guest build.
    let controller = release.join("bin/cleat");
    std::fs::create_dir_all(controller.parent().expect("controller bin")).expect("controller bin");
    std::fs::create_dir_all(release.join("lib")).expect("controller lib");
    std::fs::copy(&incoming, &controller).expect("controller executable");
    let mut controller_bytes = std::fs::read(&controller).expect("controller bytes");
    controller_bytes.extend_from_slice(b"controller-only");
    std::fs::write(&controller, controller_bytes).expect("distinct controller");
    std::fs::copy(library, release.join("lib/libghostty-vt.so.0")).expect("controller library");
    (directory, controller)
}

// All layouts execute the incoming generation with its loader-relative library.
// The process boundary fixture refuses a missing library and records the actual executable.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn contained_roll_uses_incoming_payload_and_relative_library() {
    use flotilla_provider_runtime::ProcessCommandRunner;
    for (contained, old_binary) in [(true, "/usr/local/bin/cleat"), (true, "/opt/flotilla/bin/cleat"), (false, "cleat")] {
        let (directory, incoming) = incoming_fixture();
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::env_var("PATH", "/usr/bin:/bin"));
        let mut target = target(ControlledTerminalEnvironment::from_bag(&bag).runner(Arc::new(ProcessCommandRunner)), "default", contained);
        target.runtime_root = directory.path().join("persistent cleat");
        target.binary = old_binary.into();
        for _ in 0..2 {
            let report = drain("host".into(), "new-gen".into(), &incoming, std::slice::from_ref(&target), vec![]).await;
            assert!(!report.failed(), "{report:?}");
            assert!(report.attempts[0].report.as_ref().expect("report").changed);
            if contained {
                let payload = target.runtime_root.join(".fleet-bin/new-gen");
                assert_eq!(
                    std::fs::read(payload.join("bin/cleat")).expect("delivered executable"),
                    std::fs::read(directory.path().join("release/share/flotilla/guest-tools/linux-amd64/bin/cleat"))
                        .expect("incoming guest")
                );
                assert_eq!(
                    std::fs::read(payload.join("lib/libghostty-vt.so.0")).expect("delivered library"),
                    std::fs::read(directory.path().join("release/share/flotilla/guest-tools/linux-amd64/lib/libghostty-vt.so.0"))
                        .expect("incoming guest library")
                );
            }
        }
    }
}

// Failed per-session handovers preserve diagnostics and become build skew,
// even if the successor is current or the CLI exits successfully with a warning.
#[tokio::test]
async fn partial_handover_results_are_actionable_without_session_destruction() {
    for (stayed, warning, successful) in [(true, false, false), (false, true, true)] {
        let mut value: Value = serde_json::from_str(&recorded("success").stdout).unwrap();
        value["stayed"] = if stayed {
            serde_json::json!([{"session_id":"live-shell", "reason":"attached client refused"}])
        } else {
            serde_json::json!([])
        };
        value["moved"] =
            if warning { serde_json::json!([{"session_id":"live-shell", "warning":"forwarder retained"}]) } else { serde_json::json!([]) };
        let stdout = value.to_string();
        // Subprocess boundary: cleat's documented handover result, not a daemon.
        let runner = Arc::new(MockRunner::with_outputs(vec![Ok(CommandOutput {
            stdout: stdout.clone(),
            stderr: String::new(),
            exit_code: Some(if successful { 0 } else { 1 }),
        })]));
        let report =
            drain("host".into(), "new-gen".into(), Path::new("/incoming/bin/cleat"), &[target(runner.clone(), "default", false)], vec![])
                .await;
        assert!(report.failed());
        assert_eq!(report.attempts[0].stdout, stdout);
        assert!(report.attempts[0].error.as_ref().unwrap().contains("live-shell"));
        assert_eq!(assess_drain(&report).into_condition(chrono::Utc::now()).unwrap().condition_type, "CleatBuildSkew");
        assert_eq!(runner.calls().len(), 1);
        assert_eq!(&runner.calls()[0].1[4..], ["server", "handover", "--json"]);
    }
}

// Summary and per-session warnings may name the same forwarder event. Keep
// that diagnostic once, while preserving the complete raw handover evidence.
#[tokio::test]
async fn duplicate_handover_warnings_are_reported_once() {
    let mut value: Value = serde_json::from_str(&recorded("success").stdout).unwrap();
    value["warning"] = serde_json::json!("forwarder retained");
    value["moved"] = serde_json::json!([
        {"session_id":"shell", "warning":"forwarder retained"},
        {"session_id":"shell", "warning":"distinct warning"},
        {"session_id":"shell", "warning":"distinct warning"}
    ]);
    let stdout = value.to_string();
    let runner =
        Arc::new(MockRunner::with_outputs(vec![Ok(CommandOutput { stdout: stdout.clone(), stderr: String::new(), exit_code: Some(0) })]));
    let report = drain("host".into(), "gen".into(), Path::new("/incoming/cleat"), &[target(runner, "default", false)], vec![]).await;
    let error = report.attempts[0].error.as_ref().unwrap();
    assert_eq!(error.matches("forwarder retained").count(), 1);
    assert_eq!(error.matches("distinct warning").count(), 1);
    assert_eq!(report.attempts[0].stdout, stdout);
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
                exit_code: Some(0),
            }),
            Ok(CommandOutput { stdout: "[]".into(), stderr: String::new(), exit_code: Some(0) }),
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

// ADR 0062 A7: a failed vessel handover remains visible after a daemon restart
// loses its in-memory roll report. An old child reaper with no sessions is healthy.
// IPC exception: curl authors the standard GET; this process-seam test checks
// its complete argv (Unix socket, method's default, endpoint; no headers/body).
// Real cleat protocol behavior is covered by the candidate's host handover gate.
#[tokio::test]
async fn contained_old_sessions_remain_skewed_without_a_captured_roll_report() {
    for (response, actionable, diagnostic) in [
        (Ok(r#"{"session_count":0}"#.into()), false, ""),
        (Ok(r#"{"session_count":1}"#.into()), true, "1 sessions remain"),
        (Ok(r#"{"session_count":3}"#.into()), true, "3 sessions remain"),
        (Ok("{}".into()), true, "lacks session_count"),
        (Ok("invalid".into()), true, "invalid old daemon health JSON"),
        (Err("curl executable not found".into()), true, "health probe unavailable"),
        (Err("socket disappeared".into()), true, "socket disappeared"),
    ] {
        let runner = Arc::new(MockRunner::with_outputs(vec![
            Ok(CommandOutput { stdout: "default@2".into(), stderr: String::new(), exit_code: Some(0) }),
            Ok(CommandOutput { stdout: serde_json::json!({"daemon":{"git_sha":"new"},"generation":2}).to_string(), stderr: String::new(), exit_code: Some(0) }),
            Ok(CommandOutput { stdout: serde_json::json!([{"name":"default@1","runtime_root":"/state/crew cleat","alive":true,"drain_state":"draining","build":{"git_sha":"old"}}]).to_string(), stderr: String::new(), exit_code: Some(0) }),
            response.map(|stdout| CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) }),
        ]));
        let assessment = build_skew(Some("new"), &[target(runner.clone(), "default", true)], None).await;
        assert_eq!(!assessment.actionable.is_empty(), actionable);
        if actionable {
            assert!(assessment.actionable.join("; ").contains(diagnostic));
        }
        assert_eq!(
            runner.calls()[3],
            (
                "curl".into(),
                vec![
                    "--disable".into(),
                    "--noproxy".into(),
                    "*".into(),
                    "--max-time".into(),
                    "4".into(),
                    "--fail".into(),
                    "--silent".into(),
                    "--unix-socket".into(),
                    "/state/crew cleat/default@1/socket".into(),
                    "http://localhost/healthz".into()
                ]
            )
        );
        assert_eq!(runner.remaining(), 0);
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
    CommandOutput {
        stdout: record.stdout.replace("{root}", "/state/crew cleat"),
        stderr: record.stderr,
        exit_code: Some(if record.success { 0 } else { 1 }),
    }
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
        Ok(CommandOutput { stdout: "default@27\n".into(), stderr: String::new(), exit_code: Some(0) }),
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
fn recorded_current_observations_degrade_only_actionable_runtime_skew(tc: hegel::TestCase) {
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
    assert_eq!(!assessment.actionable.is_empty(), class == 1 && installed_known);
    let informational = !installed_known || class >= 2;
    assert_eq!(assessment.information.len(), usize::from(informational));
    let condition = assessment.into_condition(chrono::Utc::now());
    if class == 1 && installed_known {
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
        (Ok(CommandOutput { stdout: "[]".into(), stderr: String::new(), exit_code: Some(0) }), true),
        (
            Ok(CommandOutput {
                stdout: serde_json::json!([
                    {"name":"other@26", "runtime_root":"/state/crew cleat", "alive":true},
                    {"name":"default@26", "runtime_root":"/another/root", "alive":true},
                    {"name":"default@25", "runtime_root":"/state/crew cleat", "alive":false}
                ])
                .to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            }),
            true,
        ),
        (Ok(CommandOutput { stdout: "invalid".into(), stderr: String::new(), exit_code: Some(0) }), false),
        (Ok(CommandOutput { stdout: "{}".into(), stderr: String::new(), exit_code: Some(0) }), false),
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
                |value| Ok(CommandOutput { stdout: format!("{value}\n"), stderr: String::new(), exit_code: Some(0) }),
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
    // Pin the CLI's two supported absent/unserved socket error strings. The
    // current generation is default@27 (dead); @26 and @25 are older and live.
    for (state, stderr) in [
        ("serving", "connect daemon: No such file or directory (os error 2)\n"),
        ("draining", "connect daemon: No such file or directory (os error 2)\n"),
        ("serving", "connect daemon: Connection refused (os error 111)\n"),
        ("draining", "connect daemon: Connection refused (os error 111)\n"),
    ] {
        let listing = serde_json::json!([
            {"name":"default@27", "runtime_root":"/state/crew cleat", "alive":false},
            {"name":"default@26", "runtime_root":"/state/crew cleat", "alive":true, "drain_state":state},
            {"name":"default@25", "runtime_root":"/state/crew cleat", "alive":true, "drain_state":"draining"},
            {"name":"default@24", "runtime_root":"/state/crew cleat", "alive":false},
            {"name":"other@26", "runtime_root":"/state/crew cleat", "alive":true},
            {"name":"default@26", "runtime_root":"/another/root", "alive":true}
        ]);
        let runner = Arc::new(MockRunner::with_outputs(vec![
            Ok(CommandOutput { stdout: String::new(), stderr: stderr.into(), exit_code: Some(1) }),
            Ok(CommandOutput { stdout: listing.to_string(), stderr: String::new(), exit_code: Some(0) }),
        ]));
        let report =
            drain("host".into(), "gen".into(), Path::new("/incoming/cleat"), &[target(runner.clone(), "default", false)], vec![]).await;
        assert!(!report.failed(), "{report:?}");
        assert!(assess_drain(&report).actionable.is_empty());
        assert_eq!(report.information.len(), 2);
        for name in ["default@26", "default@25"] {
            let message = report.information.iter().find(|message| message.contains(name)).expect("live generation waiting state");
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
        exit_code: Some(1),
    })]));
    let report =
        drain("host".into(), "gen".into(), Path::new("/incoming/cleat"), &[target(runner.clone(), "default", false)], vec![]).await;
    assert!(report.failed());
    assert!(report.information.is_empty());
    assert_eq!(runner.remaining(), 0);
}

// #2706: daemon turnover must use the same controlled startup environment as
// ordinary terminal-pool calls, even if discovery contains agent contamination.
#[tokio::test]
async fn discovered_roll_targets_start_replacements_with_a_controlled_environment() {
    let runner = Arc::new(MockRunner::with_outputs(vec![Ok(recorded("success"))]));
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::binary("cleat", "/tools/cleat"))
        .with(EnvironmentAssertion::env_var("HOME", "/execution/home"))
        .with(EnvironmentAssertion::env_var("PATH", "/execution/bin:/usr/bin:/bin"))
        .with(EnvironmentAssertion::env_var("NO_COLOR", "1"))
        .with(EnvironmentAssertion::env_var("CLAUDE_CODE_MESSAGING_TOKEN", "fake-unrelated-token"));
    let environment =
        CleatEnvironment::builder().id(EnvironmentId::new("host-direct")).bag(bag).runner(runner.clone()).contained(false).build();
    let targets = crew_targets(&environment, &[]).expect("registered roll target");
    let report = drain("host".into(), "generation-2".into(), Path::new("/incoming/bin/cleat"), &targets, vec![]).await;
    assert!(!report.failed(), "{report:?}");
    assert_eq!(
        runner.calls(),
        vec![(
            "/usr/bin/env".into(),
            vec![
                "-i".into(),
                "HOME=/execution/home".into(),
                "PATH=/execution/bin:/usr/bin:/bin".into(),
                "/incoming/bin/cleat".into(),
                "--runtime-root".into(),
                "/execution/home/.local/state/cleat".into(),
                "--server".into(),
                "default".into(),
                "server".into(),
                "handover".into(),
                "--json".into(),
            ]
        )]
    );
}

// Streaming is the true process boundary: fail after writing a partial library,
// while letting ordinary commands and the independent environment execute.
#[cfg(target_os = "linux")]
struct InterruptedDelivery(std::sync::atomic::AtomicBool);

#[cfg(target_os = "linux")]
#[async_trait::async_trait]
impl CommandRunner for InterruptedDelivery {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        flotilla_provider_runtime::ProcessCommandRunner.run(cmd, args, cwd, label).await
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        flotilla_provider_runtime::ProcessCommandRunner.run_output(cmd, args, cwd, label).await
    }

    async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
        flotilla_provider_runtime::ProcessCommandRunner.exists(cmd, args).await
    }

    async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
        use std::sync::atomic::Ordering;
        if args.last().is_some_and(|path| path.ends_with("libghostty-vt.so.0")) && self.0.swap(false, Ordering::SeqCst) {
            flotilla_provider_runtime::ProcessCommandRunner.run_with_input(cmd, args, cwd, label, &input[..1]).await?;
            return Err("interrupted library stream".into());
        }
        flotilla_provider_runtime::ProcessCommandRunner.run_with_input(cmd, args, cwd, label, input).await
    }
}

// Failed delivery never publishes partial files or changes an older generation.
// Independent targets still run; only a verified retry clears current failure.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn interrupted_delivery_retains_old_payload_and_diagnostics_until_verified_retry() {
    use flotilla_provider_runtime::ProcessCommandRunner;
    let (directory, incoming) = incoming_fixture();
    let mut first = target(Arc::new(ProcessCommandRunner), "default", true);
    first.runtime_root = directory.path().join("first");
    let baseline = drain("host".into(), "old".into(), &incoming, std::slice::from_ref(&first), vec![]).await;
    assert!(!baseline.failed(), "{baseline:?}");
    let old = first.runtime_root.join(".fleet-bin/old/bin/cleat");
    let old_bytes = std::fs::read(&old).expect("old executable");
    first.runner = Arc::new(InterruptedDelivery(std::sync::atomic::AtomicBool::new(true)));
    let mut second = first.clone();
    second.environment = "independent".into();
    second.runtime_root = directory.path().join("second");
    second.runner = Arc::new(ProcessCommandRunner);
    let mut failed = drain("host".into(), "new".into(), &incoming, &[first.clone(), second], vec![]).await;
    assert!(failed.failed());
    assert!(failed.attempts[0].error.as_ref().expect("failure").contains("interrupted library stream"));
    assert!(failed.attempts[1].success);
    assert!(!first.runtime_root.join(".fleet-bin/new").exists());
    assert_eq!(std::fs::read(&old).expect("old still usable"), old_bytes);
    persist(&mut failed, &directory.path().join("diagnostics")).await;
    let historical = failed.diagnostics_path.clone().expect("failed diagnostics");
    let mut retry = drain("host".into(), "new".into(), &incoming, std::slice::from_ref(&first), vec![]).await;
    assert!(!retry.failed(), "{retry:?}");
    assert!(assess_drain(&retry).actionable.is_empty());
    persist(&mut retry, &directory.path().join("diagnostics")).await;
    let previous: RollReport = serde_json::from_slice(&std::fs::read(historical).expect("retained history")).expect("report");
    assert!(previous.failed());
    assert_ne!(previous.diagnostics_path, retry.diagnostics_path);
    let delivered = first.runtime_root.join(".fleet-bin/new/bin/cleat");
    let metadata = std::fs::metadata(&delivered).expect("published executable");
    first.runner = Arc::new(InterruptedDelivery(std::sync::atomic::AtomicBool::new(true)));
    let again = drain("host".into(), "new".into(), &incoming, std::slice::from_ref(&first), vec![]).await;
    assert!(!again.failed(), "same-generation retry must not stream again: {again:?}");
    assert_eq!(std::fs::metadata(&delivered).expect("same executable").modified().expect("mtime"), metadata.modified().expect("mtime"));
    first.runner = Arc::new(ProcessCommandRunner);
    let (left, right) =
        tokio::join!(incoming_for_target(&first, "concurrent", &incoming), incoming_for_target(&first, "concurrent", &incoming));
    assert_eq!(left.expect("first publisher"), right.expect("concurrent publisher"));
    let original = std::fs::read(&delivered).expect("published bytes");
    let mut different = original.clone();
    different.extend_from_slice(b"# different verified release\n");
    let guest = contained_incoming(&incoming).expect("verified guest");
    std::fs::write(&guest, different).expect("different incoming guest bytes");
    refresh_guest_catalog(incoming.parent().and_then(Path::parent).expect("release"));
    let collision = drain("host".into(), "new".into(), &incoming, std::slice::from_ref(&first), vec![]).await;
    assert!(collision.failed(), "same-generation different content must not replace the payload");
    assert_eq!(std::fs::read(&delivered).expect("unchanged generation"), original);
    std::fs::remove_file(guest.parent().expect("guest bin").join("../lib/libghostty-vt.so.0")).expect("remove incoming library");
    let missing = drain("host".into(), "later".into(), &incoming, std::slice::from_ref(&first), vec![]).await;
    assert!(missing.failed());
    assert!(!first.runtime_root.join(".fleet-bin/later").exists());
    assert_eq!(std::fs::read(old).expect("old preserved"), old_bytes);
}

// A generation is a single validated path component; invalid identifiers never
// reach the environment runner or create a selectable payload.
#[tokio::test]
async fn invalid_generation_is_rejected_before_delivery() {
    for generation in ["", ".", "..", "../escape", "a/b", "a@1", "line\nbreak"] {
        let runner = Arc::new(MockRunner::with_outputs(vec![]));
        let report =
            drain("host".into(), generation.into(), Path::new("/incoming/bin/cleat"), &[target(runner.clone(), "default", true)], vec![])
                .await;
        assert!(report.failed());
        assert!(report.attempts.is_empty());
        assert!(runner.calls().is_empty());
    }
}
