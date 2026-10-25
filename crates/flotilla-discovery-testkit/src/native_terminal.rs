//! Native-only Cleat acceptance facilities. No installed tools are used until
//! explicitly constructed by an opt-in integration test.
use flotilla_protocol::result_set::CleatEndpoint;
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    os::{fd::FromRawFd, unix::process::CommandExt},
    path::Path,
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::native_process::{self, Process};

const DEADLINE: Duration = Duration::from_secs(10);

/// Reenter an ignored native test as the crew, avoiding another interpreter.
/// Call only from a test guarded by the private fixture environment variable.
pub fn crew_probe() {
    let root = std::env::var_os("FLOTILLA_NATIVE_CREW_ROOT").expect("private crew root");
    let root = Path::new(&root);
    std::fs::write(root.join("crew.pid"), std::process::id().to_string()).expect("crew PID");
    println!("preserved conversation");
    std::io::stdout().flush().expect("crew transcript");
    // A forgotten fixture cannot leave an indefinitely running crew.
    thread::sleep(Duration::from_secs(120));
}

/// A private runtime and its daemon. Ownership starts before launch so failed
/// setup is cleaned up too. Never discovers or signals a fleet daemon.
pub struct NativeTerminal {
    root: tempfile::TempDir,
    pub endpoint: CleatEndpoint,
    pub baseline: HashMap<String, String>,
    crew_pid: String,
    daemon_pid: Option<i32>,
}
impl NativeTerminal {
    pub fn new(probe_test: &str) -> Self {
        let root = tempfile::Builder::new().prefix("fa-").tempdir_in("/tmp").expect("short private runtime");
        let baseline = HashMap::from([
            ("PATH".into(), std::env::var("PATH").expect("tool path")),
            ("CLEAT_RUNTIME_DIR".into(), root.path().to_string_lossy().into_owned()),
            ("CLEAT_DAEMON".into(), "provider-attach".into()),
            ("SHELL".into(), "/bin/sh".into()),
            ("FLOTILLA_NATIVE_CREW_ROOT".into(), root.path().to_string_lossy().into_owned()),
        ]);
        let mut fixture = Self {
            endpoint: CleatEndpoint {
                runtime_root: root.path().to_string_lossy().into_owned(),
                daemon: String::new(),
                session: "session-crew".into(),
            },
            root,
            baseline,
            crew_pid: String::new(),
            daemon_pid: None,
        };
        std::fs::write(fixture.root.path().join("agent-home"), "conversation identity").expect("agent home");
        let executable = std::env::current_exe().expect("native test executable");
        let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        let command =
            format!("exec {} --exact {} --ignored --nocapture", quote(executable.to_str().expect("executable path")), quote(probe_test));
        fixture.raw(&["launch", "session-crew", "--cwd", fixture.root.path().to_str().expect("root"), "--cmd", &command]);
        fixture.endpoint.daemon = std::fs::read_dir(fixture.root.path())
            .expect("generations")
            .filter_map(Result::ok)
            .find(|entry| entry.path().join("daemon.pid").is_file())
            .expect("private daemon")
            .file_name()
            .to_string_lossy()
            .into_owned();
        let pid = std::fs::read_to_string(fixture.root.path().join(&fixture.endpoint.daemon).join("daemon.pid"))
            .expect("private daemon PID")
            .trim()
            .parse::<i32>()
            .expect("daemon PID");
        assert!(pid > 1, "private daemon PID must be positive");
        fixture.daemon_pid = Some(pid);
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Ok(pid) = std::fs::read_to_string(fixture.root.path().join("crew.pid")) {
                if !pid.is_empty() {
                    fixture.crew_pid = pid;
                    break;
                }
            }
            assert!(Instant::now() < deadline, "crew did not start");
            thread::sleep(Duration::from_millis(10));
        }
        fixture
    }
    pub fn raw(&self, args: &[&str]) -> Output {
        let mut command = Command::new("cleat");
        command.args(args).env_clear().envs(&self.baseline);
        let output = native_process::output(&mut command);
        assert!(output.status.success(), "cleat {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        output
    }
    pub fn attach(&self, commands: &[Vec<String>]) {
        let mut clients = Vec::new();
        let mut controller = None;
        for (index, args) in commands.iter().enumerate() {
            clients.push(PtyClient::new(args, &self.baseline));
            let deadline = Instant::now() + DEADLINE;
            loop {
                let output = self.raw(&["inspect", &self.endpoint.session, "--json"]);
                let inspection: serde_json::Value = serde_json::from_slice(&output.stdout).expect("inspect JSON");
                let seats = inspection["attachments"].as_array().expect("attachments");
                if seats.len() == index + 1 {
                    let controllers: Vec<_> = seats.iter().filter(|seat| seat["role"] == "controller").collect();
                    assert!(!controllers.is_empty(), "controller must remain");
                    let original = controller.get_or_insert_with(|| controllers[0]["identity"].clone());
                    assert!(controllers.iter().any(|seat| seat["identity"] == *original), "original controller displaced");
                    break;
                }
                assert!(clients[index].process.0.try_wait().expect("attachment status").is_none(), "attachment exited");
                assert!(Instant::now() < deadline, "attachment deadline");
                thread::sleep(Duration::from_millis(10));
            }
        }
        drop(clients);
        // This module is Linux-only: /proc is part of the acceptance contract.
        assert!(Path::new(&format!("/proc/{}", self.crew_pid)).exists(), "crew survived");
        assert_eq!(std::fs::read_to_string(self.root.path().join("crew.pid")).expect("crew PID"), self.crew_pid);
        assert_eq!(std::fs::read_link(format!("/proc/{}/cwd", self.crew_pid)).expect("checkout"), self.root.path());
        assert_eq!(std::fs::read_to_string(self.root.path().join("agent-home")).expect("agent home"), "conversation identity");
        let transcript = self.raw(&["transcript", &self.endpoint.session, "--since", "0"]);
        assert!(String::from_utf8_lossy(&transcript.stdout).contains("preserved conversation"), "same conversation");
    }
}
impl NativeTerminal {
    /// Explicit successful-scenario teardown; Drop is the failed-setup backstop.
    pub fn shutdown(&mut self) {
        self.try_shutdown().expect("private runtime cleanup");
    }
    fn try_shutdown(&mut self) -> Result<(), String> {
        // Retain the launched PID even if Cleat removes its metadata during exit.
        let mut candidates: Vec<i32> = self.daemon_pid.into_iter().collect();
        for entry in std::fs::read_dir(self.root.path()).into_iter().flatten().flatten() {
            if let Ok(text) = std::fs::read_to_string(entry.path().join("daemon.pid")) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    candidates.push(pid);
                }
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        let mut owned = Vec::new();
        for pid in candidates {
            if pid <= 1 {
                continue;
            }
            let Ok(environment) = std::fs::read(format!("/proc/{pid}/environ")) else { continue };
            let coordinate = format!("CLEAT_RUNTIME_DIR={}", self.root.path().display());
            if !environment.split(|b| *b == 0).any(|v| v == coordinate.as_bytes()) {
                continue;
            }
            owned.push(pid);
            // SAFETY: scalar PID validated against this private runtime above.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        let deadline = Instant::now() + DEADLINE;
        // Teardown must also stop the synthetic crew after preservation checks.
        if let Ok(pid) = self.crew_pid.parse::<i32>() {
            owned.push(pid);
        }
        for pid in owned {
            loop {
                match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                    Ok(stat) if stat.rsplit_once(") ").is_some_and(|(_, tail)| tail.starts_with("Z ")) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH) => break,
                    Err(error) => return Err(format!("private daemon inspection: {error}")),
                    _ => {}
                }
                if Instant::now() >= deadline {
                    return Err(format!("private daemon {pid} survived cleanup"));
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(())
    }
}
impl Drop for NativeTerminal {
    fn drop(&mut self) {
        if let Err(error) = self.try_shutdown() {
            eprintln!("{error}");
        }
    }
}

struct PtyClient {
    process: Process,
    stop: Arc<AtomicBool>,
    drain: Option<JoinHandle<()>>,
}
impl PtyClient {
    fn new(args: &[String], environment: &HashMap<String, String>) -> Self {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: valid output pointers; null optional terminal settings.
        assert_eq!(
            unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), std::ptr::null()) },
            0,
            "openpty"
        );
        // SAFETY: openpty returned fresh owned descriptors.
        let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        let mut command = Command::new(&args[0]);
        command
            .args(&args[1..])
            .env_clear()
            .envs(environment)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().expect("PTY stdin")))
            .stdout(Stdio::from(slave.try_clone().expect("PTY stdout")))
            .stderr(Stdio::from(slave));
        // Do not leak the master into the attachment process.
        use std::os::fd::AsRawFd;
        let master_fd = master.as_raw_fd();
        // SAFETY: close only the inherited master descriptor in the child.
        unsafe {
            command.pre_exec(move || {
                libc::close(master_fd);
                Ok(())
            });
        }
        let process = Process::spawn(&mut command);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let drain = thread::spawn(move || {
            let mut buffer = [0; 65536];
            while !stopping.load(Ordering::SeqCst) {
                use std::os::fd::AsRawFd;
                let mut poll = libc::pollfd { fd: master.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                // SAFETY: one valid pollfd, bounded wait.
                if unsafe { libc::poll(&mut poll, 1, 20) } > 0 && !matches!(master.read(&mut buffer), Ok(n) if n > 0) {
                    break;
                }
            }
        });
        Self { process, stop, drain: Some(drain) }
    }
}
impl Drop for PtyClient {
    fn drop(&mut self) {
        self.process.stop();
        self.stop.store(true, Ordering::SeqCst);
        if let Some(drain) = self.drain.take() {
            drain.join().expect("PTY drain");
        }
    }
}
