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

// Process boundary: emulate Cleat's additive/default and declared
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
            Some("kill") => Ok(String::new()),
            Some("launch") if arguments.contains(&"--help") => Ok(if self.supports_clear { "--env-clear --env" } else { "--env" }.into()),
            Some("launch") => {
                let coordinates = environment.clone();
                let mut child = if self.daemon_from_client { environment } else { polluted_environment() };
                if arguments.contains(&"--env-clear") {
                    if !self.supports_clear {
                        return Err("unknown --env-clear".into());
                    }
                    child.clear();
                }
                // Cleat owns VT identity; use its portable terminfo fallback in this fake.
                child.insert("TERM".into(), "xterm-256color".into());
                child.insert("COLORTERM".into(), "truecolor".into());
                child.insert("TERM_PROGRAM".into(), "ghostty".into());
                // Cleat's validate_environment refuses its own session coordinates.
                const MANAGED: [&str; 4] = ["CLEAT_RUNTIME_DIR", "CLEAT_DAEMON", "CLEAT_SESSION", "CLEAT_OUTPUT_DAEMON"];
                for pair in arguments.windows(2).filter(|pair| pair[0] == "--env") {
                    let (key, value) = pair[1].split_once('=').expect("NAME=VALUE");
                    if MANAGED.iter().any(|managed| key.eq_ignore_ascii_case(managed)) {
                        return Err(format!(
                            "error: invalid value '{}' for '--env <NAME=VALUE>': {key} is managed by cleat and cannot be overridden",
                            pair[1]
                        ));
                    }
                    child.insert(key.into(), value.into());
                }
                // Mint coordinates after overrides, as the daemon does.
                for key in ["CLEAT_RUNTIME_DIR", "CLEAT_DAEMON"] {
                    if let Some(value) = coordinates.get(key) {
                        child.insert(key.into(), value.clone());
                    }
                }
                let id = arguments.iter().position(|arg| *arg == "--record").map(|index| arguments[index + 1]).unwrap_or("crew");
                child.insert("CLEAT_SESSION".into(), id.into());
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
    // An empty explicitly configured PATH is still a declaration: importing
    // host PATH here would change the contract. Executable selection is separate.
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
            ("HOME", "/home/crew"),
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
            // #2825: provisioning installs durable cleat state for the client.
            ("CLEAT_RUNTIME_DIR", "/var/lib/flotilla/cleat"),
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
            for (key, expected) in vessel.iter().filter(|(key, _)| key.as_str() != "CLEAT_RUNTIME_DIR") {
                let expected = declared.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v).unwrap_or(expected);
                assert_eq!(child.get(key), Some(expected), "lost vessel {key}");
            }
            if !declared.is_empty() {
                assert_eq!(child.get("RUSTUP_HOME").map(String::as_str), Some(value), "last session override wins");
            }
            assert!(!child.contains_key("ARBITRARY_AMBIENT"), "host ambient leaked");
            assert_eq!(child.get("HOME"), vessel.get("HOME"), "host HOME must not replace the vessel HOME");
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

#[cfg(target_os = "linux")]
#[path = "environment_contract.rs"]
mod environment_contract;
