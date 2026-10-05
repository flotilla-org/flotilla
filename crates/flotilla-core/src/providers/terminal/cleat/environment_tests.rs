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
        self.clients.lock().expect("clients").push(environment);
        match arguments.first().copied() {
            Some("list") => Ok("[]".into()),
            Some("launch") if arguments.contains(&"--help") => Ok(if self.supports_clear { "--env-clear --env" } else { "--env" }.into()),
            Some("launch") => {
                let mut child = polluted_environment();
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
    let runner = Arc::new(CleatProcessFake { supports_clear: true, clients: Mutex::new(Vec::new()), child: Mutex::new(None) });
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
    let runner = Arc::new(CleatProcessFake { supports_clear: true, clients: Mutex::new(Vec::new()), child: Mutex::new(None) });
    let pool = CleatTerminalPool::new(runner.clone(), "cleat", &EnvironmentBag::new());
    pool.list_sessions().await.expect("list starts daemon");
    let clients = runner.clients.lock().expect("clients");
    assert_eq!(clients.len(), 1);
    for key in ["NO_COLOR", "CLAUDECODE", "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDE_CODE_MESSAGING_TOKEN", "ARBITRARY_AMBIENT"] {
        assert!(!clients[0].contains_key(key), "ambient {key} leaked to daemon startup");
    }
    assert!(clients[0].contains_key("PATH"));
}

// Old binaries must refuse unsafe launches instead of silently falling back
// to inherited environments. Listing an existing daemon remains available.
#[tokio::test]
async fn old_cleat_refuses_new_crew_launches() {
    let runner = Arc::new(CleatProcessFake { supports_clear: false, clients: Mutex::new(Vec::new()), child: Mutex::new(None) });
    let pool = CleatTerminalPool::new(runner.clone(), "cleat", &EnvironmentBag::new());
    let error = pool
        .ensure_session("crew", "codex", &ExecutionEnvironmentPath::new("/repo"), &vec![], &[])
        .await
        .expect_err("unsafe launch refused");
    assert!(error.contains("--env-clear"));
    assert!(runner.child.lock().expect("child").is_none());
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
