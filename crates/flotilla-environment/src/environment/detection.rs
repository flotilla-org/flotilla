//! Provider-owned observation execution with no inherited authentication or
//! configuration. The bounded owner finishes cleanup even if its caller leaves.
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner};

pub(super) const OBSERVATION_SCRIPT: &str = r#"
set -eu
# Monitor mode gives each owned background job a distinct process group.
# Its supervisor stays alive after the harness exits, keeping group ownership
# until every descendant is stopped and reaped by cleanup.
set -m
root=$1
shift
umask 077
mkdir -m 700 "$root"
child=
watchdog=
cleanup() {
    if [ -n "$watchdog" ]; then kill -KILL -- "-$watchdog" 2>/dev/null || :; wait "$watchdog" 2>/dev/null || :; fi
    if [ -n "$child" ]; then kill -KILL -- "-$child" 2>/dev/null || :; wait "$child" 2>/dev/null || :; fi
    rm -rf -- "$root"
}
trap cleanup EXIT
trap 'exit 143' HUP INT TERM
mkdir -p "$root/home" "$root/config" "$root/cache" "$root/state" "$root/runtime" "$root/claude" "$root/codex" "$root/tmp" "$root/cwd"
cd "$root/cwd"
(
    set +m
    status=0
    env -i PATH="$PATH" HOME="$root/home" XDG_CONFIG_HOME="$root/config" XDG_CACHE_HOME="$root/cache" XDG_STATE_HOME="$root/state" XDG_RUNTIME_DIR="$root/runtime" CLAUDE_CONFIG_DIR="$root/claude" CODEX_HOME="$root/codex" TMPDIR="$root/tmp" CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 "$@" || status=$?
    printf '%s' "$status" > "$root/status.tmp"
    mv "$root/status.tmp" "$root/status"
    while :; do sleep 1; done
) &
child=$!
# A target-local fallback survives a disconnected SSH client or a hard local
# owner exit. The live supervisor reserves its group until this cleanup runs.
(sleep 11; kill -KILL -- "-$child" 2>/dev/null || :; rm -rf -- "$root") </dev/null >/dev/null 2>&1 &
watchdog=$!
deadline=$((SECONDS + 10))
while [ ! -f "$root/status" ]; do
    [ "$SECONDS" -lt "$deadline" ] || exit 143
    sleep 0.01
done
status=$(cat "$root/status")
exit "$status"
"#;

pub(super) fn observation_args(root: &str, binary: &str, args: &[&str]) -> Vec<String> {
    ["-u", "BASH_ENV", "-u", "ENV", "bash", "--noprofile", "--norc", "-p", "-c", OBSERVATION_SCRIPT, "flotilla-observation", root, binary]
        .into_iter()
        .chain(args.iter().copied())
        .map(str::to_string)
        .collect()
}

pub(super) async fn host_detection(
    runner: Arc<dyn CommandRunner>,
    binary: &str,
    args: &[&str],
    _scratch: &Path,
) -> Result<CommandOutput, String> {
    let root = format!("/tmp/flotilla-observation-{}", uuid::Uuid::new_v4());
    let command = observation_args(&root, binary, args);
    run_with_cleanup(runner, "env", command, PathBuf::from("/"), "rm", vec!["-rf".into(), "--".into(), root], true).await
}

// The spawned owner is deliberately not aborted with a dropped JoinHandle:
// cancellation must not strand writable state on an SSH host or in an image.
// Both execution and cleanup are bounded and use the same injected endpoint.
pub(super) async fn run_with_cleanup(
    runner: Arc<dyn CommandRunner>,
    command: &str,
    args: Vec<String>,
    cwd: PathBuf,
    cleanup_command: &str,
    cleanup_args: Vec<String>,
    cleanup_always: bool,
) -> Result<CommandOutput, String> {
    let command = command.to_string();
    let cleanup_command = cleanup_command.to_string();
    tokio::spawn(async move {
        let argv = args.iter().map(String::as_str).collect::<Vec<_>>();
        let result = tokio::time::timeout(Duration::from_secs(12), runner.run_output(&command, &argv, &cwd, &ChannelLabel::Default)).await;
        if cleanup_always || result.is_err() || result.as_ref().is_ok_and(Result::is_err) {
            let argv = cleanup_args.iter().map(String::as_str).collect::<Vec<_>>();
            let cleanup =
                tokio::time::timeout(Duration::from_secs(2), runner.run_output(&cleanup_command, &argv, &cwd, &ChannelLabel::Default))
                    .await;
            if !cleanup.is_ok_and(|result| result.is_ok_and(|output| output.success())) {
                // Never include command output: a misbehaving harness can put
                // inherited secrets or profile contents in diagnostics.
                tracing::warn!("observation state cleanup failed or exceeded its deadline");
                return Err("observation state cleanup failed or exceeded its deadline".into());
            }
        }
        result.map_err(|_| "observation exceeded its deadline".to_string())?
    })
    .await
    .map_err(|_| "observation owner failed".to_string())?
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::environment::{command_provider, EnvironmentInput};
    use flotilla_provider_runtime::{ssh_runner::SshCommandRunner, ProcessCommandRunner};
    use std::sync::Mutex;

    // Process/SSH/container transport stand-in. It executes the real isolation
    // shell while replacing only the SSH connection or container namespace.
    struct Endpoint {
        operator: PathBuf,
        image_root: PathBuf,
        roots: Mutex<Vec<PathBuf>>,
    }
    #[async_trait::async_trait]
    impl CommandRunner for Endpoint {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            let output = self.run_output(cmd, args, cwd, label).await?;
            if output.success() {
                Ok(output.stdout)
            } else {
                Err(output.stderr)
            }
        }
        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            let mut command = cmd.to_string();
            let mut argv = args.iter().map(|arg| (*arg).to_string()).collect::<Vec<_>>();
            if cmd == "ssh" {
                command = "sh".into();
                argv = vec!["-c".into(), args.last().expect("remote command").to_string()];
            } else if cmd == "docker" {
                if args[0] == "rm" {
                    let _ = std::fs::remove_dir_all(&self.image_root);
                    return Ok(CommandOutput { stdout: String::new(), stderr: String::new(), exit_code: Some(0) });
                }
                assert!(args.contains(&"--network=none"));
                assert!(args.contains(&"--pull=never"));
                assert!(args.windows(2).any(|pair| pair == ["--entrypoint", "env"]));
                command = "env".into();
                argv = args[args.iter().position(|arg| *arg == "-u").expect("isolation shell")..].iter().map(|arg| (*arg).into()).collect();
                let root_index = argv.iter().position(|arg| arg == "flotilla-observation").expect("observation label") + 1;
                argv[root_index] = self.image_root.to_string_lossy().into_owned();
            }
            if command == "env" {
                if let Some(index) = argv.iter().position(|arg| arg == "flotilla-observation") {
                    self.roots.lock().expect("roots").push(PathBuf::from(&argv[index + 1]));
                }
            }
            // Supply hostile ambient configuration without process-global env.
            let mut outer = vec![
                "HOME=".to_string() + &self.operator.to_string_lossy(),
                "ANTHROPIC_API_KEY=fake-ambient".into(),
                "CLAUDE_CODE_OAUTH_TOKEN=fake-unrelated".into(),
                "BASH_ENV=".to_string() + &self.operator.join(".profile").to_string_lossy(),
                "BASH_FUNC_env%%=() { exit 98; }".into(),
                "XDG_CONFIG_HOME=".to_string() + &self.operator.to_string_lossy(),
                command,
            ];
            outer.extend(argv);
            ProcessCommandRunner.run_output("env", &outer.iter().map(String::as_str).collect::<Vec<_>>(), cwd, label).await
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            false
        }
    }

    async fn sentinel_case(target: usize, mode: &str) {
        let dir = tempfile::tempdir().expect("tempdir");
        let operator = dir.path().join("operator");
        std::fs::create_dir(&operator).expect("operator");
        std::fs::write(operator.join(".claude.json"), "unchanged").expect("sentinel");
        std::fs::write(operator.join(".profile"), "exit 99\n").expect("profile");
        let project = dir.path().join("project");
        std::fs::create_dir(&project).expect("project");
        std::fs::write(project.join("CLAUDE.md"), "operator project config").expect("project config");
        let harness = dir.path().join("sentinel");
        std::fs::write(
            &harness,
            r#"#!/bin/sh
set -eu
[ "${ANTHROPIC_API_KEY-unset}" = unset ]
[ "${CLAUDE_CODE_OAUTH_TOKEN-unset}" = unset ]
for variable in HOME XDG_CONFIG_HOME XDG_CACHE_HOME XDG_STATE_HOME XDG_RUNTIME_DIR CLAUDE_CONFIG_DIR CODEX_HOME TMPDIR; do
    eval 'directory=$'"$variable"
    [ -d "$directory" ]
    [ "$directory" != "$3" ]
    printf written > "$directory/sentinel"
done
[ "$PWD" != "$4" ]
[ ! -f ../CLAUDE.md ]
printf written > "$HOME/.claude.json"
printf '%s' "$HOME" > "$2"
case "$1" in success|failure)
    (sleep 1; mkdir -p "$HOME"; printf leaked > "$HOME/late-writer"; printf leaked > "${2}.leak") &
    [ "$1" = success ] && echo '2.1.300' && exit 0
    exit 7
    ;; cancel)
    (sleep 11; mkdir -p "$HOME"; printf leaked > "$HOME/late-writer"; printf leaked > "${2}.leak") &
    wait
    ;; esac
"#,
        )
        .expect("script");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&harness, std::fs::Permissions::from_mode(0o700)).expect("executable");
        let endpoint =
            Arc::new(Endpoint { operator: operator.clone(), image_root: dir.path().join("image"), roots: Mutex::new(Vec::new()) });
        let runner: Arc<dyn CommandRunner> =
            if target == 1 { Arc::new(SshCommandRunner::new("fake-remote", false, endpoint.clone())) } else { endpoint.clone() };
        let provider = command_provider(if target == 2 { EnvironmentInput::Docker } else { EnvironmentInput::HostDirect }, runner);
        let marker = dir.path().join("started");
        let args = [
            mode.to_string(),
            marker.to_string_lossy().into_owned(),
            operator.to_string_lossy().into_owned(),
            project.to_string_lossy().into_owned(),
        ];
        let binary = harness.to_string_lossy().into_owned();
        let workdir = project.clone();
        let task = tokio::spawn(async move {
            provider
                .detection()
                .expect("detection")
                .detect((target == 2).then_some("crew:test"), &binary, &args.iter().map(String::as_str).collect::<Vec<_>>(), &workdir)
                .await
        });
        if mode == "cancel" {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !marker.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("harness started");
            task.abort();
            let _ = task.await;
        } else {
            let output = task.await.expect("join").expect("detection");
            assert_eq!(output.exit_code, Some(if mode == "success" { 0 } else { 7 }));
        }
        tokio::time::timeout(Duration::from_secs(16), async {
            loop {
                let gone = if target == 1 {
                    marker.exists() && !PathBuf::from(std::fs::read_to_string(&marker).expect("home")).exists()
                } else {
                    {
                        let roots = endpoint.roots.lock().expect("roots");
                        !roots.is_empty() && roots.iter().all(|root| !root.exists())
                    }
                };
                if gone {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("disposable state cleaned even after cancellation");
        assert_eq!(std::fs::read(operator.join(".claude.json")).expect("operator sentinel"), b"unchanged");
        assert_eq!(std::fs::read_dir(&operator).expect("operator directory").count(), 2);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!marker.with_extension("leak").exists(), "no surviving child may recreate state");
    }

    // The same sentinel contract exercises local, agentless SSH, and image
    // targets, including nonzero exits and cancellation during a live process.
    #[tokio::test]
    async fn detection_isolates_and_cleans_every_provider_target() {
        for mode in ["success", "failure", "cancel"] {
            let ((), (), ()) = tokio::join!(sentinel_case(0, mode), sentinel_case(1, mode), sentinel_case(2, mode));
        }
    }
}
