use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use super::CleatTerminalPool;
use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{
        discovery::{EnvironmentAssertion, EnvironmentBag},
        terminal::TerminalPool,
        ChannelLabel, CommandOutput, CommandRunner,
    },
};

// Process boundary: emulate Cleat's additive/default and proposed declared
// spawn contracts, with an already-running daemon polluted independently of
// the environment of the Flotilla client that sends the launch request.
struct CleatProcessFake {
    supports_clear: bool,
    daemon_from_client: bool,
    clients: Mutex<Vec<BTreeMap<String, String>>>,
    child: Mutex<Option<BTreeMap<String, String>>>,
}

fn polluted_environment() -> BTreeMap<String, String> {
    [
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/host-home"),
        ("NO_COLOR", "1"),
        ("CLAUDECODE", "1"),
        ("CLAUDE_CODE_MESSAGING_SOCKET", "/unrelated/socket"),
        ("CLAUDE_CODE_MESSAGING_TOKEN", "fake-unrelated-token"),
        ("ARBITRARY_AMBIENT", "unrelated"),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value.into()))
    .collect()
}

#[async_trait]
impl CommandRunner for CleatProcessFake {
    async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        let mut environment = polluted_environment();
        let mut arguments = args;
        if cmd == "/usr/bin/env" {
            if arguments.first() == Some(&"-i") {
                environment.clear();
                arguments = &arguments[1..];
            }
            while let Some((key, value)) = arguments.first().and_then(|arg| arg.split_once('=')) {
                environment.insert(key.into(), value.into());
                arguments = &arguments[1..];
            }
            assert_eq!(arguments.first(), Some(&"cleat"));
            arguments = &arguments[1..];
        } else {
            assert_eq!(cmd, "cleat");
        }
        self.clients.lock().expect("clients").push(environment.clone());
        match arguments.first().copied() {
            Some("list") => Ok("[]".into()),
            Some("launch") if arguments.contains(&"--help") => Ok(if self.supports_clear { "--env-clear --env" } else { "--env" }.into()),
            Some("launch") => {
                let mut child = if self.daemon_from_client { environment } else { polluted_environment() };
                if arguments.contains(&"--env-clear") {
                    if !self.supports_clear {
                        return Err("unknown --env-clear".into());
                    }
                    child.clear();
                }
                // Cleat, rather than the outer terminal, owns VT identity.
                child.insert("TERM".into(), "xterm-ghostty".into());
                child.insert("COLORTERM".into(), "truecolor".into());
                for pair in arguments.windows(2).filter(|pair| pair[0] == "--env") {
                    let (key, value) = pair[1].split_once('=').expect("NAME=VALUE");
                    child.insert(key.into(), value.into());
                }
                *self.child.lock().expect("child") = Some(child);
                Ok("{}".into())
            }
            _ => Err("unexpected cleat operation".into()),
        }
    }

    async fn run_output(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        Err("unused process boundary".into())
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }
}

// #2706: arbitrary ambient values and unrelated harness credentials must not
// reach the crew, even when the serving daemon is already contaminated.
#[tokio::test]
async fn crew_environment_excludes_the_daemons_ambient_variables() {
    let runner = Arc::new(CleatProcessFake {
        supports_clear: true,
        daemon_from_client: false,
        clients: Mutex::new(Vec::new()),
        child: Mutex::new(None),
    });
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("HOME", "/crew/home"))
        .with(EnvironmentAssertion::env_var("FLOTILLA_DAEMON_SOCKET", "/run/flotilla-daemon/daemon.sock"))
        .with(EnvironmentAssertion::env_var("FLOTILLA_CONTAINED_HOST_DAEMON", "1"))
        .with(EnvironmentAssertion::env_var("CARGO_PROFILE_DEV_DEBUG", "line-tables-only"));
    let pool = CleatTerminalPool::new(runner.clone(), "cleat", &bag);
    let declared = vec![
        ("CLAUDE_CODE_ENTRYPOINT".into(), "declared-adapter".into()),
        ("EMPTY".into(), "".into()),
        ("TEXT".into(), "spaces = 'quotes'".into()),
    ];
    pool.ensure_session("crew", "codex", &ExecutionEnvironmentPath::new("/repo"), &declared, &[]).await.expect("launch");
    let child = runner.child.lock().expect("child");
    let child = child.as_ref().expect("launched crew");
    for key in ["NO_COLOR", "CLAUDECODE", "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDE_CODE_MESSAGING_TOKEN", "ARBITRARY_AMBIENT"] {
        assert!(!child.contains_key(key), "ambient {key} leaked to crew");
    }
    assert_eq!(child.get("HOME").map(String::as_str), Some("/crew/home"));
    assert_eq!(child.get("FLOTILLA_DAEMON_SOCKET").map(String::as_str), Some("/run/flotilla-daemon/daemon.sock"));
    assert_eq!(child.get("FLOTILLA_CONTAINED_HOST_DAEMON").map(String::as_str), Some("1"));
    assert_eq!(child.get("CARGO_PROFILE_DEV_DEBUG").map(String::as_str), Some("line-tables-only"));
    for (key, value) in declared {
        assert_eq!(child.get(&key), Some(&value));
    }
    assert_eq!(child.get("TERM").map(String::as_str), Some("xterm-ghostty"));
    assert_eq!(child.get("COLORTERM").map(String::as_str), Some("truecolor"));
}

// #2706: any Flotilla client operation can start a daemon on demand. Every
// such invocation must use the controlled host baseline, with no ambient leak.
#[tokio::test]
async fn on_demand_daemon_starts_without_the_clients_ambient_variables() {
    let runner = Arc::new(CleatProcessFake {
        supports_clear: true,
        daemon_from_client: false,
        clients: Mutex::new(Vec::new()),
        child: Mutex::new(None),
    });
    let pool = CleatTerminalPool::new(runner.clone(), "cleat", &EnvironmentBag::new());
    pool.list_sessions().await.expect("list starts daemon");
    let clients = runner.clients.lock().expect("clients");
    assert_eq!(clients.len(), 1);
    for key in ["NO_COLOR", "CLAUDECODE", "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDE_CODE_MESSAGING_TOKEN", "ARBITRARY_AMBIENT"] {
        assert!(!clients[0].contains_key(key), "ambient {key} leaked to daemon startup");
    }
    assert!(clients[0].contains_key("PATH"));
}

// Until cleat#318 ships, old binaries launch without --env-clear rather than
// refusing every crew (flotilla#2756). Declared variables are still passed,
// and the session inherits only the daemon's environment. Restore the refusal
// when the fleet's cleat supports --env-clear.
#[tokio::test]
async fn old_cleat_launches_without_env_clear() {
    let runner = Arc::new(CleatProcessFake {
        supports_clear: false,
        daemon_from_client: false,
        clients: Mutex::new(Vec::new()),
        child: Mutex::new(None),
    });
    let pool = CleatTerminalPool::new(runner.clone(), "cleat", &EnvironmentBag::new());
    pool.ensure_session("crew", "codex", &ExecutionEnvironmentPath::new("/repo"), &vec![], &[])
        .await
        .expect("old cleat launches without --env-clear");
    let child = runner.child.lock().expect("child").clone().expect("session launched");
    assert_eq!(child.get("TERM").map(String::as_str), Some("xterm-ghostty"));
}

// Process boundary: run real commands with injected contamination on just
// those subprocesses, without mutating the test process's global environment.
#[cfg(unix)]
struct PollutedProcessRunner;

#[cfg(unix)]
#[async_trait]
impl CommandRunner for PollutedProcessRunner {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
        let output = self.run_output(cmd, args, cwd, label).await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(output.stderr)
        }
    }

    async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
        let mut polluted = vec![
            "-i",
            "PATH=/usr/bin:/bin",
            "NO_COLOR=1",
            "CLAUDECODE=1",
            "CLAUDE_CODE_MESSAGING_TOKEN=fake-unrelated-token",
            "ARBITRARY_AMBIENT=unrelated",
            cmd,
        ];
        polluted.extend_from_slice(args);
        crate::providers::ProcessCommandRunner.run_output("/usr/bin/env", &polluted, cwd, label).await
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }
}

// #2706: the actual env utility clears inherited variables and preserves the
// declared values as argv, even when they contain shell syntax and newlines.
#[cfg(unix)]
#[tokio::test]
async fn controlled_runner_replaces_the_real_process_environment() {
    use crate::providers::{discovery::EnvironmentAssertion, terminal::environment::ControlledTerminalEnvironment};

    let value = "/host with 'quotes' $() `literal`\nsecond-line";
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("HOME", value))
        .with(EnvironmentAssertion::env_var("PATH", "/usr/bin:/bin"))
        .with(EnvironmentAssertion::env_var("NO_COLOR", "1"));
    let runner = ControlledTerminalEnvironment::from_bag(&bag).runner(Arc::new(PollutedProcessRunner));
    let output = runner.run("/usr/bin/env", &[], Path::new("/"), &ChannelLabel::Default).await.expect("real cleared process");
    assert_eq!(output, format!("HOME={value}\nPATH=/usr/bin:/bin\n"));
    let output = runner.run_output("/usr/bin/env", &[], Path::new("/"), &ChannelLabel::Default).await.expect("real output path");
    assert!(output.success());
    assert_eq!(output.stdout, format!("HOME={value}\nPATH=/usr/bin:/bin\n"));
    let output = runner
        .run_with_timeout("/usr/bin/env", &[], Path::new("/"), &ChannelLabel::Default, std::time::Duration::from_secs(5))
        .await
        .expect("real deadline path");
    assert_eq!(output, format!("HOME={value}\nPATH=/usr/bin:/bin\n"));
}

// #2790: Docker's configured environment is the baseline for both the Cleat
// client/daemon and its launched crew, with or without --env-clear. Host facts
// added by other discovery remain excluded; explicit session values win.
#[hegel::test]
fn docker_crew_preserves_vessel_environment(tc: hegel::TestCase) {
    use hegel::generators as gs;

    use crate::providers::discovery::run_provisioned_host_detectors;

    // Generate both Cleat versions, empty/nonempty vessel maps, absent/present PATH,
    // both bag merge orders, and empty, ordinary and shell-special values.
    // Sequential launches cover session overrides; no global environment is mutated.
    let supports_clear = tc.draw(gs::booleans());
    let configured_path = tc.draw(gs::booleans());
    let empty_vessel = tc.draw(gs::booleans());
    let reverse_merge = tc.draw(gs::booleans());
    let values = ["", "vessel value", "quotes'\" = $() `literal`\nsecond line"];
    let value = values[tc.draw(gs::integers::<usize>().min_value(0).max_value(values.len() - 1))];
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let runner = Arc::new(CleatProcessFake {
            supports_clear,
            daemon_from_client: true,
            clients: Mutex::new(Vec::new()),
            child: Mutex::new(None),
        });
        let mut vessel: std::collections::HashMap<String, String> = [
            ("RUSTUP_HOME", "/usr/local/rustup"),
            ("CARGO_HOME", "/tmp/flotilla-config/cargo"),
            ("GIT_CONFIG_COUNT", "3"),
            ("GIT_CONFIG_KEY_0", "safe.directory"),
            ("GIT_CONFIG_VALUE_0", "/workspace"),
            ("GIT_CONFIG_KEY_1", "user.name"),
            ("GIT_CONFIG_VALUE_1", "crew"),
            ("GIT_CONFIG_KEY_2", "user.email"),
            ("GIT_CONFIG_VALUE_2", "crew@example.test"),
            ("FLOTILLA_CREW_SKILLS", value),
            ("DISABLE_AUTOUPDATER", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        if empty_vessel {
            vessel.clear();
        }
        if configured_path {
            vessel.insert("PATH".into(), value.into());
        }
        let provisioned = run_provisioned_host_detectors(&[], &*runner, &vessel).await;
        let host = EnvironmentBag::new()
            .with(EnvironmentAssertion::env_var("ARBITRARY_AMBIENT", "host"))
            .with(EnvironmentAssertion::env_var("HOME", "/host-home"))
            .with(EnvironmentAssertion::env_var("PATH", "/host-bin"));
        let bag = if reverse_merge { host.merge(&provisioned) } else { provisioned.merge(&host) };
        let pool = CleatTerminalPool::new(runner.clone(), "cleat", &bag);
        for declared in [vec![], vec![("RUSTUP_HOME".into(), "session override".into()), ("RUSTUP_HOME".into(), value.into())]] {
            pool.ensure_session("crew", "codex", &ExecutionEnvironmentPath::new("/repo"), &declared, &[]).await.expect("launch");
            let child = runner.child.lock().expect("child").clone().expect("launched crew");
            for (key, expected) in &vessel {
                let expected = declared.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v).unwrap_or(expected);
                assert_eq!(child.get(key), Some(expected), "lost vessel {key}");
            }
            if !declared.is_empty() {
                assert_eq!(child.get("RUSTUP_HOME").map(String::as_str), Some(value), "last session override wins");
            }
            assert!(!child.contains_key("ARBITRARY_AMBIENT"), "host ambient leaked");
            assert!(!child.contains_key("HOME"), "host baseline leaked");
            assert_eq!(child.get("PATH").map(String::as_str), Some(if configured_path { value } else { "/usr/local/bin:/usr/bin:/bin" }));
        }
        for client in runner.clients.lock().expect("clients").iter() {
            for (key, expected) in &vessel {
                assert_eq!(client.get(key), Some(expected), "lost daemon baseline {key}");
            }
            assert!(!client.contains_key("ARBITRARY_AMBIENT"));
        }
    });
}
