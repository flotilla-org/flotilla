//! The foreground CLI supervisor is a Linux child subreaper. An app-server
//! crash cannot leave detached tool commands running in the crew environment.
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
        _ = terminate.recv() => { child.kill().await.map_err(|error| error.to_string())?; child.wait().await.map_err(|error| error.to_string()) },
        _ = interrupt.recv() => { child.kill().await.map_err(|error| error.to_string())?; child.wait().await.map_err(|error| error.to_string()) },
    };
    // All surviving descendants now reparent to this supervisor, including
    // commands that started their own process group or session. Repeated scans
    // catch grandchildren reparenting as their intermediate parents exit.
    cleanup_orphans(&SystemOrphans { own_pid: Pid::from_u32(std::process::id()) }).await?;
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

#[cfg(not(target_os = "linux"))]
pub async fn supervise(_binary: &str, _endpoint: &str) -> Result<(), String> {
    Err("supervised Codex app-server currently requires Linux child-subreaper support".into())
}

/// Request graceful supervisor shutdown before the terminal pool removes its
/// shell. Validate the exact incarnation endpoint before signalling a PID.
#[cfg(target_os = "linux")]
pub async fn stop(endpoint: &str) -> Result<(), String> {
    use std::time::Duration;

    use sysinfo::{Pid, Signal, System};
    let pid_file = format!("{endpoint}.supervisor-pid");
    let content = match tokio::fs::read_to_string(&pid_file).await {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let pid = content.parse::<u32>().map_err(|error| error.to_string())?;
    let system = System::new_all();
    if let Some(process) = system.process(Pid::from_u32(pid)) {
        let command = process.cmd();
        if !command.iter().any(|arg| arg == "codex-app-server")
            || !command.windows(2).any(|args| args[0] == "--socket" && args[1] == endpoint)
        {
            return Err("refusing to signal a reused Codex supervisor PID".into());
        }
        if process.kill_with(Signal::Term) != Some(true) {
            return Err("could not terminate Codex supervisor".into());
        }
        for _ in 0..120 {
            if !tokio::fs::try_exists(&pid_file).await.map_err(|error| error.to_string())? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        return Err("Codex supervisor cleanup is still pending".into());
    }
    Err("Codex supervisor disappeared without confirming orphan cleanup".into())
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
    for _ in 0..100 {
        if !orphans.terminate_and_reap()? {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
