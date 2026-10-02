//! Process-exit receipts for agents hosted inside a persistent terminal shell.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use sha2::{Digest, Sha256};
use tokio::{sync::Mutex, time::Instant};

use crate::providers::{ChannelLabel, CommandRunner};

/// Each launch gets a distinct receipt, so an earlier process cannot mark a
/// replacement process as exited. Hashing also keeps the path shell-safe.
pub fn exit_receipt(crew_id: &str) -> String {
    format!(".flotilla/agent-exits/{:x}", Sha256::digest(crew_id.as_bytes()))
}

/// The parent shell records exit even when the agent cannot run its own hooks.
/// The terminal and checkout remain available for explicit or automatic resume.
pub fn monitored_command(command: &str, crew_id: &str) -> String {
    let receipt = exit_receipt(crew_id);
    format!("(\n{command}\n)\nflotilla_agent_exit_code=$?\nmkdir -p .flotilla/agent-exits\nprintf '%s\\n' \"$flotilla_agent_exit_code\" > {receipt}")
}

/// Remove only this launch's receipt through its execution environment.
pub async fn remove_exit_receipt(runner: &dyn CommandRunner, cwd: &Path, launch_id: &str) -> Result<(), String> {
    runner.run("rm", &["-f", "--", &exit_receipt(launch_id)], cwd, &ChannelLabel::Default).await?;
    Ok(())
}

/// One observation per launch per batch. Consuming entries means a repeated
/// reconcile gets fresh evidence, while expiry bounds delay for quiet sessions.
/// Errors from the transport are never cached. Environments have separate locks.
#[derive(Default)]
pub struct ExitReceiptObserver {
    environments: Mutex<BTreeMap<String, Arc<Mutex<ReceiptBatch>>>>,
}

#[derive(Default)]
struct ReceiptBatch {
    observed_at: Option<Instant>,
    observations: BTreeMap<PathBuf, Result<Option<i32>, String>>,
}

impl ExitReceiptObserver {
    pub async fn observe<F: std::future::Future<Output = Result<Vec<PathBuf>, String>> + Send>(
        &self,
        environment: &str,
        runner: &dyn CommandRunner,
        requested: PathBuf,
        candidates: impl FnOnce() -> F + Send,
    ) -> Result<Option<i32>, String> {
        let batch = self.environments.lock().await.entry(environment.to_string()).or_default().clone();
        let mut batch = batch.lock().await;
        if batch.observed_at.is_some_and(|at| at.elapsed() < Duration::from_secs(1)) {
            if let Some(observation) = batch.observations.remove(&requested) {
                return observation;
            }
        }
        batch.observations.clear();
        batch.observed_at = None;
        let mut paths = candidates().await?.into_iter().collect::<std::collections::BTreeSet<_>>();
        paths.insert(requested.clone());
        let paths = paths.into_iter().collect::<Vec<_>>();
        // Bound argv size and shell work: a promptly consumed batch takes
        // ceil(N/128) round trips. Paths travel as argv, not shell code.
        for chunk in paths.chunks(128) {
            let mut args = vec!["-c", RECEIPT_BATCH_SCRIPT, "flotilla-agent-exits"];
            let strings = chunk.iter().map(|path| path.to_str().ok_or("receipt path is not UTF-8")).collect::<Result<Vec<_>, _>>()?;
            args.extend(strings);
            let output = runner.run("sh", &args, Path::new("/"), &ChannelLabel::Default).await?;
            let lines = output.lines().collect::<Vec<_>>();
            if lines.len() != chunk.len() {
                return Err("incomplete agent exit receipt batch".to_string());
            }
            for (index, (path, line)) in chunk.iter().zip(lines).enumerate() {
                let (returned_index, value) = line.split_once('\t').ok_or("invalid agent exit receipt batch")?;
                if returned_index != index.to_string() {
                    return Err("misidentified agent exit receipt batch".to_string());
                }
                let observation = match value {
                    "-" => Ok(None),
                    "E" => Err("could not read agent exit receipt".to_string()),
                    value => value.parse().map(Some).map_err(|error| format!("invalid agent exit receipt: {error}")),
                };
                batch.observations.insert(path.clone(), observation);
            }
        }
        batch.observed_at = Some(Instant::now());
        batch.observations.remove(&requested).expect("requested receipt included in batch")
    }
}

const RECEIPT_BATCH_SCRIPT: &str = r#"index=0
for path do
    if [ -e "$path" ]; then
        if value=$(cat -- "$path"); then
            case "$value" in
                *[!0-9-]*|"") value=E ;;
            esac
        else
            value=E
        fi
    else
        value=-
    fi
    printf '%s\t%s\n' "$index" "$value"
    index=$((index + 1))
done"#;

#[cfg(test)]
mod tests {
    use std::{
        process::Command,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use async_trait::async_trait;

    use super::*;
    use crate::providers::{CommandOutput, ProcessCommandRunner};

    // The real shell is the process boundary: exit receipts must work for both
    // successful and failed child processes, without an agent-provided hook.
    #[test]
    fn parent_records_agent_exit_and_keeps_launches_independent() {
        let cwd = tempfile::tempdir().expect("checkout");
        for code in [0, 1, 42, 130, 137] {
            let crew = format!("crew-{code}");
            let result = Command::new("sh")
                .arg("-c")
                .arg(monitored_command(&format!("exit {code}"), &crew))
                .current_dir(cwd.path())
                .status()
                .expect("shell");
            assert!(result.success(), "the parent remains usable after child failure");
            assert_eq!(std::fs::read_to_string(cwd.path().join(exit_receipt(&crew))).expect("receipt").trim(), code.to_string());
            assert!(!cwd.path().join(exit_receipt("next launch")).exists(), "an old launch cannot mark its replacement exited");
        }
    }
    // Fake only the environment subprocess boundary; files remain real-backed.
    #[derive(Default)]
    struct CountingRunner {
        calls: AtomicUsize,
        fail: AtomicBool,
    }

    #[async_trait]
    impl CommandRunner for CountingRunner {
        async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
            ProcessCommandRunner.exists(cmd, args).await
        }
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.swap(false, Ordering::SeqCst) {
                return Err("transient transport error".to_string());
            }
            ProcessCommandRunner.run(cmd, args, cwd, label).await
        }
        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            ProcessCommandRunner.run_output(cmd, args, cwd, label).await
        }
    }

    // Measure the old per-session observation and the batch with the same real
    // receipts and injected runner. Counts capture the remote round-trip cost.
    #[tokio::test]
    async fn many_sessions_share_environment_round_trips() {
        for count in [1usize, 16, 128, 129] {
            let cwd = tempfile::tempdir().expect("checkout");
            std::fs::create_dir_all(cwd.path().join(".flotilla/agent-exits")).expect("receipt directory");
            let paths = (0..count).map(|id| cwd.path().join(exit_receipt(&id.to_string()))).collect::<Vec<_>>();
            for path in &paths {
                std::fs::write(path, "42\n").expect("receipt");
            }
            let runner = CountingRunner::default();
            let baseline = Instant::now();
            for path in &paths {
                assert_eq!(
                    runner
                        .run(
                            "sh",
                            &["-c", "if [ -f \"$1\" ]; then cat -- \"$1\"; fi", "exit", path.to_str().expect("path")],
                            Path::new("/"),
                            &ChannelLabel::Default
                        )
                        .await
                        .expect("old observation")
                        .trim(),
                    "42"
                );
            }
            let baseline_time = baseline.elapsed();
            assert_eq!(runner.calls.swap(0, Ordering::SeqCst), count);
            let observer = ExitReceiptObserver::default();
            let batched = Instant::now();
            for path in &paths {
                assert_eq!(observer.observe("env", &runner, path.clone(), || async { Ok(paths.clone()) }).await.expect("batch"), Some(42));
            }
            let calls = runner.calls.load(Ordering::SeqCst);
            eprintln!(
                "sessions={count} baseline_calls={count} baseline={baseline_time:?} batch_calls={calls} batch={:?}",
                batched.elapsed()
            );
            assert_eq!(calls, count.div_ceil(128));
        }
    }

    // Only positive evidence stops a launch; restart, replacement, malformed
    // files and transport failure must not borrow another session's evidence.
    #[tokio::test]
    async fn observations_retry_and_remain_independent_across_restart_and_relaunch() {
        let cwd = tempfile::tempdir().expect("checkout with spaces");
        std::fs::create_dir_all(cwd.path().join(".flotilla/agent-exits")).expect("directory");
        let old = cwd.path().join(exit_receipt("old"));
        let next = cwd.path().join(exit_receipt("replacement"));
        let other = cwd.path().join(exit_receipt("other role"));
        std::fs::write(&old, "130\n").expect("old exit");
        std::fs::write(&other, "invalid\nreceipt\n").expect("corrupt other receipt");
        let paths = vec![old.clone(), next.clone(), other.clone()];
        let runner = CountingRunner::default();
        let observer = ExitReceiptObserver::default();
        assert_eq!(observer.observe("env", &runner, old.clone(), || async { Ok(paths.clone()) }).await.expect("old"), Some(130));
        assert!(observer.observe("env", &runner, other.clone(), || async { Ok(paths.clone()) }).await.is_err());
        assert_eq!(observer.observe("env", &runner, next.clone(), || async { Ok(paths.clone()) }).await.expect("new launch"), None);
        std::fs::write(&next, "0\n").expect("replacement exit");
        runner.fail.store(true, Ordering::SeqCst);
        assert!(observer.observe("env", &runner, next.clone(), || async { Ok(paths.clone()) }).await.is_err());
        assert_eq!(observer.observe("env", &runner, next.clone(), || async { Ok(paths.clone()) }).await.expect("retry"), Some(0));
        let restarted = ExitReceiptObserver::default();
        assert_eq!(restarted.observe("env", &runner, next.clone(), || async { Ok(paths.clone()) }).await.expect("restart"), Some(0));
        remove_exit_receipt(&runner, cwd.path(), "old").await.expect("retire only old");
        assert!(!old.exists());
        assert!(next.exists());
        assert!(other.exists());
        remove_exit_receipt(&runner, cwd.path(), "replacement").await.expect("teardown");
        remove_exit_receipt(&runner, cwd.path(), "replacement").await.expect("idempotent teardown");
        assert!(!next.exists());
        assert!(other.exists());
    }
    // Fake the environment subprocess boundary, preserving launch-path lookup.
    struct MemoryReceiptRunner {
        values: BTreeMap<PathBuf, Option<i32>>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl CommandRunner for MemoryReceiptRunner {
        async fn run(&self, _: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(args[3..]
                .iter()
                .enumerate()
                .map(|(index, path)| {
                    let value =
                        self.values.get(Path::new(path)).expect("known launch").map_or_else(|| "-".to_string(), |code| code.to_string());
                    format!("{index}\t{value}\n")
                })
                .collect())
        }
        async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
            unreachable!("observer uses run")
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            true
        }
    }

    // Empty through multiple chunks, duplicate candidates, zero and signal
    // exits, absent evidence, and shared paths in different environments.
    // Every launch gets exactly its own evidence, with ceil(N/128) round trips.
    #[hegel::test]
    fn batch_preserves_launch_evidence_and_bounds_round_trips(tc: hegel::TestCase) {
        let count = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(513));
        let values = (0..count)
            .map(|index| {
                (PathBuf::from(format!("/checkout-{index}/{}", exit_receipt(&index.to_string()))), match index % 4 {
                    0 => None,
                    1 => Some(0),
                    2 => Some(42),
                    _ => Some(130),
                })
            })
            .collect::<BTreeMap<_, _>>();
        let runner = MemoryReceiptRunner { values: values.clone(), calls: AtomicUsize::new(0) };
        let paths = values.keys().cloned().collect::<Vec<_>>();
        let candidate_calls = AtomicUsize::new(0);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let observer = ExitReceiptObserver::default();
            for (path, expected) in &values {
                assert_eq!(
                    observer
                        .observe("env-a", &runner, path.clone(), || async {
                            candidate_calls.fetch_add(1, Ordering::SeqCst);
                            Ok(paths.iter().chain(paths.iter()).cloned().collect())
                        })
                        .await
                        .expect("observation"),
                    *expected
                );
            }
            assert_eq!(runner.calls.load(Ordering::SeqCst), count.div_ceil(128));
            assert_eq!(candidate_calls.load(Ordering::SeqCst), usize::from(count > 0), "list candidates only on a batch miss");
            let other_runner =
                MemoryReceiptRunner { values: values.keys().cloned().map(|path| (path, Some(137))).collect(), calls: AtomicUsize::new(0) };
            for path in &paths {
                assert_eq!(
                    observer
                        .observe("env-b", &other_runner, path.clone(), || async { Ok(paths.clone()) })
                        .await
                        .expect("other environment"),
                    Some(137)
                );
            }
            assert_eq!(other_runner.calls.load(Ordering::SeqCst), count.div_ceil(128));
        });
    }
    // A cached absence supplies no positive exit evidence. It expires even if
    // a quiet session has not consumed it, and subsequent reads are fresh.
    #[tokio::test(start_paused = true)]
    async fn unconsumed_absence_expires_without_stopping_the_session() {
        let first = PathBuf::from("/checkout/first");
        let quiet = PathBuf::from("/checkout/quiet");
        let paths = vec![first.clone(), quiet.clone()];
        let runner = MemoryReceiptRunner { values: [(first.clone(), None), (quiet.clone(), None)].into(), calls: AtomicUsize::new(0) };
        let observer = ExitReceiptObserver::default();
        assert_eq!(observer.observe("env", &runner, first, || async { Ok(paths.clone()) }).await.expect("initial batch"), None);
        let exited =
            MemoryReceiptRunner { values: [(paths[0].clone(), None), (quiet.clone(), Some(0))].into(), calls: AtomicUsize::new(0) };
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            observer.observe("env", &exited, quiet.clone(), || async { Ok(paths.clone()) }).await.expect("expired absence"),
            Some(0)
        );
        assert_eq!(exited.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            observer.observe("env", &exited, quiet, || async { Ok(paths.clone()) }).await.expect("consumed observation refreshed"),
            Some(0)
        );
        assert_eq!(exited.calls.load(Ordering::SeqCst), 2);
    }
}
