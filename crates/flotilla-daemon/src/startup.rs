//! Timings for startup awaits, including restoration that runs after listening.

use std::future::Future;

use tracing::info;

struct PhaseTiming {
    phase: &'static str,
    started: tokio::time::Instant,
    completed: bool,
}

impl Drop for PhaseTiming {
    fn drop(&mut self) {
        info!(
            phase = self.phase,
            duration_ms = self.started.elapsed().as_secs_f64() * 1000.0,
            status = if self.completed { "completed" } else { "cancelled" },
            "daemon startup phase finished"
        );
    }
}

pub(crate) async fn phase<T>(phase: &'static str, operation: impl Future<Output = T>) -> T {
    info!(phase, "daemon startup phase started");
    let mut timing = PhaseTiming { phase, started: tokio::time::Instant::now(), completed: false };
    let result = operation.await;
    timing.completed = true;
    result
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tracing::instrument::WithSubscriber;

    use super::phase;

    // Deferred restoration remains pending before readiness, starts after
    // readiness, and is cancelled when the server closes without listening.
    #[tokio::test]
    async fn listening_gate_observes_readiness_and_closed_server_without_clock_deadlines() {
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let gate = crate::runtime::wait_for_listening(Some(receiver));
        tokio::pin!(gate);
        assert!(futures::poll!(gate.as_mut()).is_pending());
        sender.send(true).expect("server ready");
        assert!(gate.await.is_ok());
        let (sender, receiver) = tokio::sync::watch::channel(false);
        drop(sender);
        assert!(crate::runtime::wait_for_listening(Some(receiver)).await.is_err());
        assert!(crate::runtime::wait_for_listening(None).await.is_ok());
    }

    // #2487: even a failed or interrupted await must leave its phase and elapsed
    // duration in the JSON log. Virtual time makes the 38-second gap inexpensive.
    #[tokio::test(start_paused = true)]
    async fn startup_phase_records_duration_for_success_error_and_cancellation() {
        for outcome in ["success", "error", "cancelled"] {
            let log = tempfile::NamedTempFile::new().expect("log file");
            let subscriber = tracing_subscriber::fmt().json().with_ansi(false).with_writer(log.reopen().expect("log writer")).finish();
            let operation = phase("reconcile_work_credentials", async {
                tokio::time::sleep(Duration::from_millis(38_650)).await;
                if outcome == "error" {
                    Err("staging failed")
                } else {
                    Ok(())
                }
            });
            async {
                if outcome == "cancelled" {
                    assert!(tokio::time::timeout(Duration::from_millis(1234), operation).await.is_err());
                } else {
                    assert_eq!(operation.await.is_err(), outcome == "error");
                }
            }
            .with_subscriber(subscriber)
            .await;
            let records = std::fs::read_to_string(log.path())
                .expect("JSON log")
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON record"))
                .collect::<Vec<_>>();
            assert_eq!(records.len(), 2);
            let fields = &records[1]["fields"];
            assert_eq!(fields["phase"], "reconcile_work_credentials");
            assert_eq!(fields["status"], if outcome == "cancelled" { "cancelled" } else { "completed" });
            assert_eq!(fields["duration_ms"], if outcome == "cancelled" { 1234.0 } else { 38_650.0 });
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        path::Path,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use flotilla_core::providers::{ChannelLabel, CommandOutput, CommandRunner};
    use tokio::sync::{Notify, Semaphore};

    // Represents the credential consumer's process/HTTP boundary. No actual
    // credential or network is used; the caller controls preflight completion.
    pub(crate) struct GatedCredentialPreflight {
        pub entered: Notify,
        pub release: Semaphore,
        pub completed: AtomicUsize,
    }

    impl GatedCredentialPreflight {
        pub fn new() -> Self {
            Self { entered: Notify::new(), release: Semaphore::new(0), completed: AtomicUsize::new(0) }
        }
    }

    #[async_trait]
    impl CommandRunner for GatedCredentialPreflight {
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            if cmd == "git" && args == ["--version"] {
                return Ok("git version 2.43.0".into());
            }
            if cmd == "rm" && args.first() == Some(&"-f") && args.last().is_some_and(|path| path.ends_with("gitconfig")) {
                self.completed.fetch_add(1, Ordering::SeqCst);
                return Ok(String::new());
            }
            Err(format!("unexpected test process command: {cmd}"))
        }
        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            Ok(CommandOutput { stdout: self.run(cmd, args, cwd, label).await?, stderr: String::new(), success: true })
        }
        async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
            cmd == "claude" && args == ["--version"]
        }
        async fn run_with_input(
            &self,
            cmd: &str,
            _args: &[&str],
            _cwd: &Path,
            _label: &ChannelLabel,
            _input: &[u8],
        ) -> Result<String, String> {
            assert_eq!(cmd, "curl", "only credential HTTP preflight uses stdin");
            self.entered.notify_one();
            self.release.acquire().await.expect("release credential preflight").forget();
            Ok(String::new())
        }
    }
}
