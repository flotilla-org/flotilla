//! Bounded subprocess ownership shared by opt-in terminal acceptance fixtures.
use std::{
    io::{Read, Seek, SeekFrom},
    process::{Child, Command, Output},
    thread,
    time::{Duration, Instant},
};
const DEADLINE: Duration = Duration::from_secs(10);
/// Owns a subprocess from spawn through bounded wait or explicit kill/reap.
pub(crate) struct Process(pub(crate) Child);
impl Process {
    pub(crate) fn spawn(command: &mut Command) -> Self {
        Self(command.spawn().expect("native subprocess"))
    }
    pub(crate) fn wait(&mut self) {
        self.wait_until(Instant::now() + DEADLINE);
    }
    fn wait_until(&mut self, deadline: Instant) {
        loop {
            if self.0.try_wait().expect("wait subprocess").is_some() {
                return;
            }
            assert!(Instant::now() < deadline, "native subprocess deadline");
            thread::sleep(Duration::from_millis(10));
        }
    }
    pub(crate) fn stop(&mut self) {
        let _ = self.0.kill();
        let deadline = Instant::now() + DEADLINE;
        loop {
            match self.0.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                _ => {
                    eprintln!("native subprocess cleanup deadline");
                    return;
                }
            }
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Capture via files so pipe backpressure cannot defeat the process deadline.
pub fn output(command: &mut Command) -> Output {
    let stdout = tempfile::tempfile().expect("stdout");
    let stderr = tempfile::tempfile().expect("stderr");
    command.stdout(stdout.try_clone().expect("stdout clone")).stderr(stderr.try_clone().expect("stderr clone"));
    let mut process = Process::spawn(command);
    process.wait();
    let read = |mut file: std::fs::File| {
        file.seek(SeekFrom::Start(0)).expect("rewind");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("output");
        bytes
    };
    Output { status: process.0.try_wait().expect("status").expect("exited"), stdout: read(stdout), stderr: read(stderr) }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    // Native command capture must retain nonzero outcomes for adapter diagnostics.
    #[test]
    fn capture_preserves_failure_output() {
        let result = output(Command::new("/bin/sh").args(["-c", "printf detail >&2; exit 7"]));
        assert_eq!(result.status.code(), Some(7));
        assert_eq!(result.stderr, b"detail");
    }

    // Deadline failure must still kill and reap the subprocess on unwinding.
    #[test]
    fn deadline_reaps_owned_child() {
        let mut process = Process::spawn(Command::new("/bin/sleep").arg("30"));
        let pid = process.0.id() as i32;
        let started = Instant::now();
        let deadline = started + Duration::from_millis(20);
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| process.wait_until(deadline)));
        assert!(failure.is_err(), "deadline must reject a stalled process");
        drop(process);
        assert!(started.elapsed() < Duration::from_secs(2), "cleanup must be bounded");
        let mut status = 0;
        // SAFETY: pid identifies this test's own child; status is a valid output pointer.
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        let error = std::io::Error::last_os_error();
        if result == 0 {
            // Mutation failures must not leak the intentionally stalled child.
            // SAFETY: still this test's unreaped child.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
        }
        assert!(result == -1 && error.raw_os_error() == Some(libc::ECHILD), "owned child must already be reaped");
    }
}
