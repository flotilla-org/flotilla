//! The foreground CLI supervisor is a Linux child subreaper. An app-server
//! crash cannot leave detached tool commands running in the crew environment.
use std::time::Duration;

const CHILD_GRACE: Duration = Duration::from_secs(2);
const ORPHAN_SWEEPS: usize = 100;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
// Child flush + nominal orphan sweeps + scheduling/process-query margin.
const STOP_WAIT: Duration =
    Duration::from_millis(CHILD_GRACE.as_millis() as u64 + ORPHAN_SWEEPS as u64 * POLL_INTERVAL.as_millis() as u64 + 3000);
/// The caller also needs time to start the helper and transfer its result.
pub const STOP_COMMAND_TIMEOUT: Duration = Duration::from_secs(STOP_WAIT.as_secs() + 5);
#[cfg(target_os = "linux")]
pub async fn supervise(binary: &str, endpoint: &str) -> Result<(), String> {
    use sysinfo::Pid;
    // This runs only in the dedicated CLI process, never inside the daemon.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|error| error.to_string())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).map_err(|error| error.to_string())?;
    let pid_file = format!("{endpoint}.supervisor-pid");
    // Publish before spawn so stop can find a supervisor during startup; a
    // crash before spawn leaves a harmless stale receipt, discarded on recovery.
    tokio::fs::write(&pid_file, std::process::id().to_string()).await.map_err(|error| error.to_string())?;
    let mut child = match tokio::process::Command::new(binary)
        .args(["app-server", "--listen", &format!("unix://{endpoint}")])
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = tokio::fs::remove_file(&pid_file).await;
            return Err(error.to_string());
        }
    };
    let result = tokio::select! {
        result = child.wait() => result.map_err(|error| error.to_string()),
        _ = terminate.recv() => shutdown_child(&mut child).await,
        _ = interrupt.recv() => shutdown_child(&mut child).await,
    };
    // All surviving descendants now reparent to this supervisor, including
    // commands that started their own process group or session. Repeated scans
    // catch grandchildren reparenting as their intermediate parents exit.
    // Always attempt orphan cleanup, including after a failed child signal or
    // wait. Retain the receipt if cleanup failed rather than claiming success.
    let cleanup = cleanup_orphans(&SystemOrphans { own_pid: Pid::from_u32(std::process::id()) }).await;
    if let Err(error) = cleanup {
        return Err(match result {
            Err(original) => format!("{original}; orphan cleanup failed: {error}"),
            Ok(_) => error,
        });
    }
    let _ = tokio::fs::remove_file(endpoint).await;
    let _ = tokio::fs::remove_file(&pid_file).await;
    if let Some(parent) = std::path::Path::new(endpoint).parent() {
        let _ = tokio::fs::remove_dir(parent).await;
    }
    let status = result?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Codex app-server exited: {status}"))
    }
}

/// Give the app-server a bounded chance to flush its own state before force.
#[cfg(target_os = "linux")]
async fn shutdown_child(child: &mut tokio::process::Child) -> Result<std::process::ExitStatus, String> {
    if let Some(pid) = child.id() {
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error.to_string());
            }
        }
    }
    match tokio::time::timeout(CHILD_GRACE, child.wait()).await {
        Ok(result) => result.map_err(|error| error.to_string()),
        Err(_) => {
            child.start_kill().map_err(|error| error.to_string())?;
            child.wait().await.map_err(|error| error.to_string())
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn supervise(_binary: &str, _endpoint: &str) -> Result<(), String> {
    Err("supervised Codex app-server currently requires Linux child-subreaper support".into())
}

/// Request graceful supervisor shutdown before the terminal pool removes its
/// shell. Validate the exact incarnation endpoint before signalling a PID.
#[cfg(target_os = "linux")]
pub async fn stop(endpoint: &str) -> Result<(), String> {
    use sysinfo::{Pid, Signal, System};
    let pid_file = format!("{endpoint}.supervisor-pid");
    let content = match tokio::fs::read_to_string(&pid_file).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let pid = content.parse::<u32>().map_err(|error| error.to_string())?;
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    refresh_supervisor(&mut system, pid);
    if let Some(process) = system.process(pid) {
        if live_supervisor(process.status() == sysinfo::ProcessStatus::Zombie, process.cmd(), endpoint)? {
            if process.kill_with(Signal::Term) != Some(true) {
                // The process may have exited between inspection and signal.
                refresh_supervisor(&mut system, pid);
                if supervisor_running(&system, pid, endpoint)? {
                    return Err("could not terminate Codex supervisor".into());
                }
            } else if wait_for_cleanup(
                || async { tokio::fs::try_exists(&pid_file).await.map_err(|error| error.to_string()) },
                || {
                    refresh_supervisor(&mut system, pid);
                    supervisor_running(&system, pid, endpoint)
                },
            )
            .await?
            {
                return Ok(());
            }
        }
    }
    // A reboot or SIGKILL can leave the receipt after the process is gone.
    // It cannot prove orphan cleanup, but it must not permanently block vessel
    // teardown/relaunch. The enclosing vessel remains the cleanup boundary.
    tracing::warn!(%endpoint, %pid, "Codex supervisor is absent; discarding stale shutdown receipt");
    tokio::fs::remove_file(&pid_file).await.map_err(|error| error.to_string())?;
    match tokio::fs::remove_file(endpoint).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    Ok(())
}
#[cfg(target_os = "linux")]
fn refresh_supervisor(system: &mut sysinfo::System, pid: sysinfo::Pid) {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, UpdateKind};
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always));
}
#[cfg(target_os = "linux")]
fn supervisor_running(system: &sysinfo::System, pid: sysinfo::Pid, endpoint: &str) -> Result<bool, String> {
    match system.process(pid) {
        Some(process) => live_supervisor(process.status() == sysinfo::ProcessStatus::Zombie, process.cmd(), endpoint),
        None => Ok(false),
    }
}
#[cfg(any(target_os = "linux", test))]
fn live_supervisor(zombie: bool, command: &[std::ffi::OsString], endpoint: &str) -> Result<bool, String> {
    if zombie {
        return Ok(false);
    }
    if !command.iter().any(|arg| arg == "codex-app-server") || !command.windows(2).any(|args| args[0] == "--socket" && args[1] == endpoint)
    {
        return Err("refusing to signal a reused or uninspectable Codex supervisor PID".into());
    }
    Ok(true)
}
// The process-query closure is the process boundary; tests inject exit and
// continued liveness without signalling another process or waiting wall time.
#[cfg(any(target_os = "linux", test))]
async fn wait_for_cleanup<F: std::future::Future<Output = Result<bool, String>>>(
    mut receipt_exists: impl FnMut() -> F,
    mut alive: impl FnMut() -> Result<bool, String>,
) -> Result<bool, String> {
    tokio::time::timeout(STOP_WAIT, async {
        loop {
            if !receipt_exists().await? {
                return Ok(true);
            }
            if !alive()? {
                return Ok(false);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .map_err(|_| "Codex supervisor cleanup is still pending".to_string())?
}
#[cfg(not(target_os = "linux"))]
pub async fn stop(_endpoint: &str) -> Result<(), String> {
    Err("Codex supervisor requires Linux".into())
}

#[cfg(any(target_os = "linux", test))]
trait Orphans {
    /// Return whether children were present before killing and reaping them.
    fn terminate_and_reap(&self) -> Result<bool, String>;
}

#[cfg(any(target_os = "linux", test))]
async fn cleanup_orphans(orphans: &dyn Orphans) -> Result<(), String> {
    for _ in 0..ORPHAN_SWEEPS {
        if !orphans.terminate_and_reap()? {
            return Ok(());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Err("Codex orphan cleanup did not settle within five seconds".into())
}
#[cfg(target_os = "linux")]
struct SystemOrphans {
    own_pid: sysinfo::Pid,
}
#[cfg(any(target_os = "linux", test))]
fn owned_orphan(pid: sysinfo::Pid, parent: Option<sysinfo::Pid>, thread: Option<sysinfo::ThreadKind>, own_pid: sysinfo::Pid) -> bool {
    // sysinfo includes /proc/<pid>/task entries whose reported parent is the
    // process itself. Signalling such a TID would kill this supervisor too.
    pid != own_pid && parent == Some(own_pid) && thread.is_none()
}
#[cfg(target_os = "linux")]
impl Orphans for SystemOrphans {
    fn terminate_and_reap(&self) -> Result<bool, String> {
        let system = sysinfo::System::new_all();
        let children = system
            .processes()
            .values()
            .filter(|process| owned_orphan(process.pid(), process.parent(), process.thread_kind(), self.own_pid))
            .collect::<Vec<_>>();
        for process in &children {
            if !process.kill() {
                return Err("could not terminate an orphaned Codex tool process".into());
            }
        }
        loop {
            let reaped = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
            if reaped <= 0 {
                break;
            }
        }
        Ok(!children.is_empty())
    }
}
#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use hegel::generators as gs;

    use super::*;
    // Zombie snapshots (including empty commands) are gone; live inaccessible
    // and reused PIDs remain protected, while exact live incarnations may stop.
    #[hegel::test]
    fn zombie_receipts_do_not_block_but_live_identity_is_required(tc: hegel::TestCase) {
        let zombie = tc.draw(gs::booleans());
        let kind = tc.draw(gs::integers::<u8>().min_value(0).max_value(2));
        let command = match kind {
            0 => vec!["flotilla", "codex-app-server", "--socket", "/private/socket"],
            1 => vec!["unrelated-process"],
            _ => Vec::new(),
        }
        .into_iter()
        .map(std::ffi::OsString::from)
        .collect::<Vec<_>>();
        let result = live_supervisor(zombie, &command, "/private/socket");
        if zombie {
            assert_eq!(result, Ok(false));
        } else if kind == 0 {
            assert_eq!(result, Ok(true));
        } else {
            assert!(result.is_err());
        }
    }

    // A supervisor that exits with a retained receipt is stale immediately;
    // a healthy-but-stuck process still waits to the bounded stop deadline.
    #[tokio::test(start_paused = true)]
    async fn shutdown_poll_detects_exit_without_waiting_for_the_deadline() {
        let started = tokio::time::Instant::now();
        let mut queries = 0;
        let result = wait_for_cleanup(
            || std::future::ready(Ok(true)),
            || {
                queries += 1;
                Ok(queries < 3)
            },
        )
        .await
        .expect("process gone");
        assert!(!result, "caller must discard the stale receipt");
        assert!(started.elapsed() < STOP_WAIT);
        assert!(wait_for_cleanup(|| std::future::ready(Ok(true)), || Ok(true)).await.is_err());
        assert!(wait_for_cleanup(|| std::future::ready(Ok(false)), || panic!("clean receipt needs no process query"))
            .await
            .expect("confirmed cleanup"));
        assert!(STOP_COMMAND_TIMEOUT > STOP_WAIT);
    }

    // A retained receipt after reboot/SIGKILL must not block another stop or
    // relaunch. This process-boundary fixture uses an impossible Linux PID.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn absent_supervisor_discards_stale_receipt() {
        let dir = tempfile::tempdir().expect("socket dir");
        let endpoint = dir.path().join("control.sock").to_string_lossy().into_owned();
        let receipt = format!("{endpoint}.supervisor-pid");
        tokio::fs::write(&receipt, u32::MAX.to_string()).await.expect("stale receipt");
        tokio::fs::write(&endpoint, "stale socket").await.expect("stale endpoint");
        stop(&endpoint).await.expect("already stopped");
        assert!(!tokio::fs::try_exists(&receipt).await.expect("receipt existence"));
        assert!(!tokio::fs::try_exists(&endpoint).await.expect("endpoint existence"));
        stop(&endpoint).await.expect("idempotent stop");
    }

    // A real child receives SIGTERM and can flush before exit. A child that
    // ignores it is force-stopped after the bounded grace period.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn child_shutdown_allows_flush_and_bounds_unresponsive_child() {
        for responsive in [true, false] {
            let dir = tempfile::tempdir().expect("child fixture");
            let ready = dir.path().join("ready");
            let flushed = dir.path().join("flushed");
            let script = if responsive {
                r#"trap 'printf flushed > "$2"; exit 0' TERM; printf ready > "$1"; while :; do sleep 0.05; done"#
            } else {
                r#"trap '' TERM; printf ready > "$1"; while :; do sleep 0.05; done"#
            };
            let mut child = tokio::process::Command::new("sh")
                .args(["-c", script, "fixture"])
                .arg(&ready)
                .arg(&flushed)
                .kill_on_drop(true)
                .spawn()
                .expect("child");
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !tokio::fs::try_exists(&ready).await.expect("ready") {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("fixture ready");
            let status = shutdown_child(&mut child).await.expect("child stopped");
            assert_eq!(status.success(), responsive);
            assert_eq!(tokio::fs::try_exists(&flushed).await.expect("flush"), responsive);
        }
    }

    // A process-table snapshot includes our own Tokio threads. Only real child
    // processes belong to orphan cleanup; threads and self must never be killed.
    #[hegel::test]
    fn only_child_processes_are_cleanup_targets(tc: hegel::TestCase) {
        // Real adopted children, unrelated processes, self, orphan-parentless
        // records, and both thread kinds vary across positive PID identities.
        let own = sysinfo::Pid::from_u32(tc.draw(gs::integers::<u32>().min_value(1).max_value(65530)));
        let child = sysinfo::Pid::from_u32(own.as_u32() + 1);
        let unrelated = sysinfo::Pid::from_u32(own.as_u32() + 2);
        let case = tc.draw(gs::integers::<u8>().min_value(0).max_value(5));
        let (pid, parent, thread) = match case {
            0 => (child, Some(own), None),
            1 => (own, Some(unrelated), None),
            2 => (child, Some(unrelated), None),
            3 => (child, Some(own), Some(sysinfo::ThreadKind::Userland)),
            4 => (child, Some(own), Some(sysinfo::ThreadKind::Kernel)),
            _ => (child, None, None),
        };
        assert_eq!(owned_orphan(pid, parent, thread, own), case == 0);
    }

    // Reparenting can reveal several waves of children. Cleanup reports success
    // only after an empty scan, and bounds persistent orphan or signal failure.
    #[hegel::test]
    fn orphan_cleanup_waits_for_empty_and_bounds_failure(tc: hegel::TestCase) {
        // Empty, one wave, multiple generations, last permitted scan, exhausted
        // cleanup, and a failed signal all pass through the same process seam.
        let waves = tc.draw(gs::integers::<usize>().min_value(0).max_value(105));
        let rejected = tc.draw(gs::booleans());
        struct Fake {
            remaining: Mutex<usize>,
            rejected: bool,
        }
        impl Orphans for Fake {
            fn terminate_and_reap(&self) -> Result<bool, String> {
                if self.rejected {
                    return Err("signal refused".into());
                }
                let mut remaining = self.remaining.lock().expect("waves");
                let present = *remaining > 0;
                *remaining = remaining.saturating_sub(1);
                Ok(present)
            }
        }
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().expect("runtime");
        rt.block_on(async {
            let result = cleanup_orphans(&Fake { remaining: Mutex::new(waves), rejected }).await;
            assert_eq!(result.is_ok(), !rejected && waves < 100);
        });
    }
}
