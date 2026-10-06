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

const SCENARIOS: &[Scenario] =
    &[Scenario::HostBaseline, Scenario::Vessel, Scenario::EmptyVessel, Scenario::PollutedDaemon, Scenario::ManagedRefusal];

struct Harness {
    runner: Arc<dyn CommandRunner>,
    fake: Option<Arc<CleatProcessFake>>,
    binary: String,
    root: tempfile::TempDir,
}

impl Harness {
    async fn new(supports_clear: bool, scenario: Scenario, real: bool) -> Self {
        // SUN_LEN-safe private root, independent of the caller's TMPDIR.
        let root = tempfile::Builder::new().prefix("fc-").tempdir_in("/tmp").expect("private cleat root");
        // Cleat execs SHELL with -lc. Replace that first exec with a probe so
        // login shell startup cannot alter the values we are trying to observe.
        let probe = root.path().join("probe");
        std::fs::write(&probe, format!("#!/usr/bin/python3\nimport os, time\nwith open({:?}, 'wb') as f:\n f.write(b'\\0'.join(k+b'='+v for k,v in os.environb.items())+b'\\0')\ntime.sleep(30)\n", root.path().join("environment").to_str().expect("dump path"))).expect("write first-exec probe");
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
                let harness = Self { runner, fake: None, binary, root };
                let version = harness.raw(&["version", "--json"]).await.expect("real version");
                let version: serde_json::Value = serde_json::from_str(&version).expect("version JSON");
                let pins = include_str!("../../../../../../ci/cleat-environment/revisions.sh");
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
        Self { runner: fake.clone(), fake: Some(fake), binary: "cleat".into(), root }
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
        // Only terminate daemons in this fixture's private runtime, including on panic.
        if self.fake.is_none() {
            if let Ok(entries) = std::fs::read_dir(self.root.path()) {
                for entry in entries.flatten() {
                    if let Ok(pid) = std::fs::read_to_string(entry.path().join("daemon.pid")) {
                        if let Ok(pid) = pid.trim().parse::<i32>() {
                            if pid > 1 {
                                unsafe { libc::kill(pid, libc::SIGTERM) };
                            }
                        }
                    }
                }
            }
        }
    }
}

// The finite scenario matrix covers both versions, clean/on-demand and polluted
// daemons, absent/empty vessel declarations, duplicate overrides, shell-special
// values, all managed names and case variants. No global environment is changed.
async fn contract(real: bool) {
    for supports_clear in [false, true] {
        for &scenario in SCENARIOS {
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
                continue;
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
        }
    }
}

#[tokio::test]
async fn fake_launch_environment_contract() {
    contract(false).await;
}

#[cfg(feature = "cleat-environment-contract")]
#[tokio::test]
async fn real_launch_environment_contract() {
    contract(true).await;
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
