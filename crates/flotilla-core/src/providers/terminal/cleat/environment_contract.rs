//! #2827: one launch contract at the fake and actual CLI/process boundary.
use std::os::unix::fs::PermissionsExt;

use super::*;

#[derive(Clone, Copy, Debug)]
enum Scenario {
    HostBaseline,
    Vessel,
    EmptyVessel,
    PollutedDaemon,
    ManagedRefusal,
}

struct Harness {
    runner: Arc<dyn CommandRunner>,
    fake: Option<Arc<CleatProcessFake>>,
    binary: String,
    root: tempfile::TempDir,
    daemon_pids: Mutex<Vec<i32>>,
}

impl Harness {
    async fn new(supports_clear: bool, scenario: Scenario, real: bool) -> Self {
        // SUN_LEN-safe private root, independent of the caller's TMPDIR.
        let root = tempfile::Builder::new().prefix("fc-").tempdir_in("/tmp").expect("private cleat root");
        // Cleat execs SHELL with -lc. Replace that first exec with a probe so
        // login shell startup cannot alter the values we are trying to observe.
        let probe = root.path().join("probe");
        let dump = root.path().join("environment");
        let script = format!(
            "#!/usr/bin/python3\n\
             import os, time\n\
             with open({:?}, 'wb') as f:\n\
             \x20f.write(b'\\0'.join(k+b'='+v for k,v in os.environb.items())+b'\\0')\n\
             time.sleep(30)\n",
            dump.to_str().expect("dump path")
        );
        std::fs::write(&probe, script).expect("write first-exec probe");
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o700)).expect("executable probe");
        if real {
            #[cfg(feature = "cleat-environment-contract")]
            {
                let variable = if supports_clear { "FLOTILLA_TEST_CLEAT_NEW" } else { "FLOTILLA_TEST_CLEAT_OLD" };
                let binary =
                    std::env::var(variable).unwrap_or_else(|_| panic!("{variable} must select a pinned binary; see ci/cleat-environment"));
                assert!(Path::new(&binary).is_absolute(), "binary must be absolute");
                assert_eq!(
                    Path::new(&binary).file_name().and_then(|name| name.to_str()),
                    Some("cleat"),
                    "cleat respawns its sibling named cleat; keep versions in separate directories"
                );
                let runner: Arc<dyn CommandRunner> = Arc::new(RealProcessRunner);
                runner
                    .run("/usr/bin/python3", &["--version"], Path::new("/"), &ChannelLabel::Default)
                    .await
                    .expect("real launch contract requires /usr/bin/python3 for the first-exec probe");
                let harness = Self { runner, fake: None, binary, root, daemon_pids: Mutex::new(Vec::new()) };
                let version = harness.raw(&["version", "--json"]).await.expect("real version");
                let version: serde_json::Value = serde_json::from_str(&version).expect("version JSON");
                let pins = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../ci/cleat-environment/revisions.sh"));
                let prefix = if supports_clear { "CLEAT_NEW_REVISION=" } else { "CLEAT_OLD_REVISION=" };
                let revision = pins.lines().find_map(|line| line.strip_prefix(prefix)).expect("revision pin");
                assert_eq!(version["client"]["git_sha"].as_str(), Some(revision), "wrong binary source revision");
                let help = harness.raw(&["launch", "--help"]).await.expect("real help");
                assert_eq!(help.split_whitespace().any(|word| word == "--env-clear"), supports_clear, "wrong cleat revision");
                if matches!(scenario, Scenario::PollutedDaemon) {
                    // Inventory does not start an idle daemon in real Cleat. Seed a
                    // live daemon with an additive launch before the controlled client.
                    harness
                        .raw(&["launch", "--json", "seed", "--cmd", "unused-by-first-exec-probe"])
                        .await
                        .expect("start contaminated daemon");
                    harness.child().await;
                    harness.raw(&["kill", "seed"]).await.expect("stop seed child");
                    std::fs::remove_file(harness.root.path().join("environment")).expect("remove seed dump");
                }
                return harness;
            }
            #[cfg(not(feature = "cleat-environment-contract"))]
            panic!("real contract requires cleat-environment-contract");
        }
        let fake = Arc::new(CleatProcessFake {
            supports_clear,
            daemon_from_client: !matches!(scenario, Scenario::PollutedDaemon),
            clients: Mutex::new(Vec::new()),
            child: Mutex::new(None),
        });
        Self { runner: fake.clone(), fake: Some(fake), binary: "cleat".into(), root, daemon_pids: Mutex::new(Vec::new()) }
    }

    async fn raw(&self, args: &[&str]) -> Result<String, String> {
        let runtime = format!("CLEAT_RUNTIME_DIR={}", self.root.path().display());
        let shell = format!("SHELL={}/probe", self.root.path().display());
        let mut controlled = vec![runtime.as_str(), shell.as_str(), "CLEAT_DAEMON=contract", self.binary.as_str()];
        controlled.extend_from_slice(args);
        self.runner.run("/usr/bin/env", &controlled, Path::new("/"), &ChannelLabel::Default).await
    }

    async fn child(&self) -> BTreeMap<String, String> {
        if let Some(fake) = &self.fake {
            return fake.child.lock().expect("child").clone().expect("launched child");
        }
        // Assert the pinned layout after each actual launch; never silently skip cleanup.
        let pids = std::fs::read_dir(self.root.path())
            .expect("private runtime inventory")
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_to_string(entry.path().join("daemon.pid")).ok())
            .map(|pid| pid.trim().parse::<i32>().expect("daemon PID"))
            .collect::<Vec<_>>();
        self.daemon_pids.lock().expect("daemon PIDs").extend(pids.iter().copied().filter(|pid| *pid > 1));
        assert!(!pids.is_empty() && pids.iter().all(|pid| *pid > 1), "real launch must publish private daemon PID files");
        let dump = self.root.path().join("environment");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(bytes) = std::fs::read(&dump) {
                if bytes.ends_with(&[0]) {
                    return bytes
                        .split(|byte| *byte == 0)
                        .filter(|entry| !entry.is_empty())
                        .map(|entry| {
                            let entry = std::str::from_utf8(entry).expect("UTF8 environment");
                            let (key, value) = entry.split_once('=').expect("environment pair");
                            (key.to_string(), value.to_string())
                        })
                        .collect();
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "child did not write its environment");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Retain the discovered PIDs so removal of metadata cannot turn cleanup into a no-op.
        for &pid in self.daemon_pids.lock().expect("daemon PIDs").iter() {
            // A daemon may already have exited. Avoid signalling a reused PID
            // unless its current command still identifies this pinned daemon.
            let Ok(command) = std::fs::read(format!("/proc/{pid}/cmdline")) else { continue };
            let mut args = command.split(|byte| *byte == 0);
            if args.next() != Some(self.binary.as_bytes()) || !args.any(|arg| arg == b"serve") {
                continue;
            }
            // SAFETY: positive PIDs were read from this fixture's private runtime
            // after a real launch. kill takes scalar arguments, with no pointer access.
            let result = unsafe { libc::kill(pid, libc::SIGTERM) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    eprintln!("private cleat daemon {pid} cleanup failed: {error}");
                }
            }
        }
    }
}

async fn assert_daemons_stopped(pids: &[i32]) {
    // Successful real scenarios prove Drop terminated the private daemon, not just its session.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let alive = pids.iter().any(|pid| match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat.rsplit_once(") ").expect("process stat").1.split_whitespace().next() != Some("Z"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => panic!("inspect private daemon {pid}: {error}"),
        });
        if !alive {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "private cleat daemon survived fixture cleanup: {pids:?}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

// The finite scenario matrix covers both versions, clean/on-demand and polluted
// daemons, absent/empty vessel declarations, duplicate overrides, shell-special
// values, all managed names and case variants. No global environment is changed.
async fn contract(real: bool, supports_clear: bool, scenario: Scenario) {
    let harness = Harness::new(supports_clear, scenario, real).await;
    if matches!(scenario, Scenario::ManagedRefusal) {
        // Cleat refuses caller overrides of its minted coordinates, ignoring case.
        for key in ["CLEAT_RUNTIME_DIR", "CLEAT_DAEMON", "CLEAT_SESSION", "CLEAT_OUTPUT_DAEMON", "cleat_session"] {
            let value = format!("{key}=forbidden");
            let error = harness
                .raw(&["launch", "refused", "--cmd", "/bin/sleep 30", "--env", &value])
                .await
                .expect_err("managed coordinate refused");
            assert!(error.contains("managed by cleat"), "{key}: {error}");
        }
        return;
    }
    let runtime = harness.root.path().display().to_string();
    let shell = format!("{runtime}/probe");
    let mut bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("SHELL", &shell))
        .with(EnvironmentAssertion::env_var("HOME", "/crew/home"))
        .with(EnvironmentAssertion::env_var("CLEAT_RUNTIME_DIR", &runtime))
        .with(EnvironmentAssertion::env_var("CLEAT_DAEMON", "contract"))
        .with(EnvironmentAssertion::env_var("FLOTILLA_DAEMON_SOCKET", "/crew/daemon.sock"))
        .with(EnvironmentAssertion::env_var("FLOTILLA_CONTAINED_HOST_DAEMON", "1"))
        .with(EnvironmentAssertion::env_var("CARGO_PROFILE_DEV_DEBUG", "line-tables-only"))
        .with(EnvironmentAssertion::env_var("ARBITRARY_AMBIENT", "host"));
    let mut expected = BTreeMap::from([
        ("HOME".to_string(), "/crew/home".to_string()),
        ("PATH".to_string(), "/usr/local/bin:/usr/bin:/bin".to_string()),
        ("FLOTILLA_DAEMON_SOCKET".to_string(), "/crew/daemon.sock".to_string()),
        ("FLOTILLA_CONTAINED_HOST_DAEMON".to_string(), "1".to_string()),
        ("CARGO_PROFILE_DEV_DEBUG".to_string(), "line-tables-only".to_string()),
    ]);
    if matches!(scenario, Scenario::Vessel | Scenario::EmptyVessel) {
        use crate::providers::discovery::run_provisioned_host_detectors;
        let mut vessel = std::collections::HashMap::from([
            ("CLEAT_RUNTIME_DIR".into(), runtime.clone()),
            ("CLEAT_DAEMON".into(), "contract".into()),
            ("SHELL".into(), shell.clone()),
        ]);
        expected.clear();
        expected.insert("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into());
        if matches!(scenario, Scenario::Vessel) {
            for (key, value) in [
                ("HOME", "/vessel/home"),
                ("RUSTUP_HOME", "/vessel/rustup"),
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", "safe.directory"),
                ("GIT_CONFIG_VALUE_0", "/workspace"),
                ("FLOTILLA_CREW_SKILLS", "vessel skills"),
            ] {
                vessel.insert(key.into(), value.into());
                expected.insert(key.into(), value.into());
            }
            vessel.insert("PATH".into(), "".into());
            expected.insert("PATH".into(), "".into());
        }
        bag = bag.merge(&run_provisioned_host_detectors(&[], harness.runner.as_ref(), &vessel).await);
    }
    let pool = CleatTerminalPool::new(harness.runner.clone(), &harness.binary, &bag);
    let declared = vec![
        ("CLAUDE_CODE_ENTRYPOINT".into(), "declared-adapter".into()),
        ("EMPTY".into(), "".into()),
        ("TEXT".into(), "spaces = 'quotes' $() `literal`\nsecond line".into()),
        ("RUSTUP_HOME".into(), "superseded".into()),
        ("RUSTUP_HOME".into(), "session override".into()),
        // #2826: managed values in vessel or session declarations stay client-only.
        ("cleat_session".into(), "outer-session".into()),
    ];
    expected.insert("SHELL".into(), shell);
    expected.extend(declared.iter().filter(|(key, _)| key != "cleat_session").cloned());
    // The first-exec probe observes values before a login shell can
    // rewrite them, including an intentionally empty vessel PATH.
    let command = "unused-by-first-exec-probe";
    pool.ensure_session("crew", command, &ExecutionEnvironmentPath::new("/"), &declared, &[]).await.expect("contract launch");
    let child = harness.child().await;
    for (key, value) in &expected {
        assert_eq!(child.get(key), Some(value), "{scenario:?} clear={supports_clear}: {key}");
    }
    if matches!(scenario, Scenario::EmptyVessel) {
        assert!(!child.contains_key("HOME"), "host HOME imported into empty vessel");
    }
    // Only old cleat with an independently contaminated daemon may inherit ambient values.
    for key in ["NO_COLOR", "CLAUDECODE", "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDE_CODE_MESSAGING_TOKEN", "ARBITRARY_AMBIENT"] {
        assert_eq!(
            child.contains_key(key),
            !supports_clear && matches!(scenario, Scenario::PollutedDaemon),
            "{scenario:?} clear={supports_clear}: ambient {key}"
        );
    }
    assert!(!child.contains_key("cleat_session"));
    assert_eq!(child.get("CLEAT_SESSION").map(String::as_str), Some("crew"), "fresh session coordinate");
    assert_eq!(child.get("TERM_PROGRAM").map(String::as_str), Some("ghostty"));
    assert!(matches!(child.get("TERM").map(String::as_str), Some("xterm-ghostty" | "xterm-256color")));
    assert_eq!(child.get("COLORTERM").map(String::as_str), Some("truecolor"));
    pool.kill_session("crew").await.expect("stop child");
    let pids = harness.daemon_pids.lock().expect("daemon PIDs").clone();
    drop(harness);
    assert_daemons_stopped(&pids).await;
}

macro_rules! launch_cases {
    ($real:expr; $( $name:ident: $clear:expr, $scenario:ident; )*) => {
        $(
            #[tokio::test]
            async fn $name() {
                contract($real, $clear, Scenario::$scenario).await;
            }
        )*
    };
    ($real:expr) => {
        launch_cases!($real;
            old_host_baseline: false, HostBaseline;
            new_host_baseline: true, HostBaseline;
            old_vessel: false, Vessel;
            new_vessel: true, Vessel;
            old_empty_vessel: false, EmptyVessel;
            new_empty_vessel: true, EmptyVessel;
            old_polluted_daemon: false, PollutedDaemon;
            new_polluted_daemon: true, PollutedDaemon;
            old_managed_refusal: false, ManagedRefusal;
            new_managed_refusal: true, ManagedRefusal;
        );
    };
}

mod fake_launch_environment_contract {
    use super::*;
    launch_cases!(false);
}

#[cfg(feature = "cleat-environment-contract")]
mod real_launch_environment_contract {
    use super::*;
    launch_cases!(true);
}

// A retained PID can outlive its daemon. Cleanup must leave another process alone.
#[tokio::test]
async fn cleanup_does_not_signal_an_unrelated_process() {
    let mut child = tokio::process::Command::new("/bin/sleep").arg("30").kill_on_drop(true).spawn().expect("unrelated process");
    let harness = Harness::new(true, Scenario::HostBaseline, false).await;
    harness.daemon_pids.lock().expect("daemon PIDs").push(child.id().expect("child PID") as i32);
    drop(harness);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), child.wait()).await.is_err(),
        "cleanup signalled an unrelated process"
    );
    child.kill().await.expect("stop unrelated fixture process");
}

// Inventory uses the controlled client envelope even when no discovery facts exist.
#[tokio::test]
async fn bare_list_uses_controlled_client_environment() {
    let harness = Harness::new(true, Scenario::HostBaseline, false).await;
    let pool = CleatTerminalPool::new(harness.runner.clone(), "cleat", &EnvironmentBag::new());
    pool.list_sessions().await.expect("bare list");
    let fake = harness.fake.as_ref().expect("process fake");
    let clients = fake.clients.lock().expect("clients");
    assert_eq!(clients.len(), 1);
    assert!(clients[0].contains_key("PATH"));
    for key in ["NO_COLOR", "CLAUDECODE", "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDE_CODE_MESSAGING_TOKEN", "ARBITRARY_AMBIENT"] {
        assert!(!clients[0].contains_key(key), "ambient {key} leaked to inventory client");
    }
}

#[cfg(feature = "cleat-environment-contract")]
struct RealProcessRunner;

#[cfg(feature = "cleat-environment-contract")]
#[async_trait]
impl CommandRunner for RealProcessRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        let output = self.run_output(cmd, args, cwd, label).await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(output.stderr)
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        // Actual process boundary: pollution is subprocess-local, never test-global.
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tokio::process::Command::new(cmd)
                .args(args)
                .current_dir(cwd)
                .env_clear()
                .envs(polluted_environment())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| "cleat command deadline".to_string())?
        .map_err(|error| error.to_string())?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code(),
        })
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }
}
