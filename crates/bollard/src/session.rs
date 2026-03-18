use std::{
    collections::VecDeque,
    ffi::CString,
    fs,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::{fs::PermissionsExt, net::UnixStream},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    protocol::Frame,
    runtime::{RuntimeLayout, SessionMetadata},
    vt::{passthrough::PassthroughVtEngine, VtEngine},
};

const SOCKET_NAME: &str = "socket";
const PID_NAME: &str = "daemon.pid";
const FOREGROUND_NAME: &str = "foreground";
const STRIP_ENV_VARS: &[&str] = &["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"];
const DEFAULT_TERMINAL_COLS: u16 = 80;
const DEFAULT_TERMINAL_ROWS: u16 = 24;

#[derive(Debug)]
pub struct ForegroundAttach {
    stream: Arc<Mutex<UnixStream>>,
}

impl ForegroundAttach {
    pub fn relay_stdio(self) -> Result<(), String> {
        let _tty_mode = TerminalModeGuard::activate()?;
        let read_handle = {
            let stream = self.stream.lock().map_err(|_| "attach stream lock poisoned".to_string())?;
            stream.try_clone().map_err(|err| format!("clone attach stream: {err}"))?
        };
        let mut read_stream = read_handle;
        let alive = Arc::new(AtomicBool::new(true));
        let alive_out = Arc::clone(&alive);
        let relay_out = thread::spawn(move || -> Result<(), String> {
            let mut stdout = std::io::stdout().lock();
            loop {
                match Frame::read(&mut read_stream) {
                    Ok(Frame::Output(bytes)) => {
                        stdout.write_all(&bytes).map_err(|err| format!("write stdout: {err}"))?;
                        stdout.flush().map_err(|err| format!("flush stdout: {err}"))?;
                    }
                    Ok(_) => {}
                    Err(err) => {
                        alive_out.store(false, Ordering::SeqCst);
                        if matches!(
                            err.kind(),
                            std::io::ErrorKind::UnexpectedEof
                                | std::io::ErrorKind::BrokenPipe
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) {
                            return Ok(());
                        }
                        return Err(format!("read attach frame: {err}"));
                    }
                }
            }
        });

        let write_stream = Arc::clone(&self.stream);
        let alive_resize = Arc::clone(&alive);
        let resize_loop = thread::spawn(move || -> Result<(), String> {
            let mut last = current_terminal_size();
            while alive_resize.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(100));
                let next = current_terminal_size();
                if next != last {
                    let mut stream = write_stream.lock().map_err(|_| "attach stream lock poisoned".to_string())?;
                    Frame::Resize { cols: next.0, rows: next.1 }.write(&mut *stream).map_err(|err| format!("write resize frame: {err}"))?;
                    last = next;
                }
            }
            Ok(())
        });

        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut stream = self.stream.lock().map_err(|_| "attach stream lock poisoned".to_string())?;
                    Frame::Input(buf[..n].to_vec()).write(&mut *stream).map_err(|err| format!("write input frame: {err}"))?;
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(format!("read stdin: {err}")),
            }
        }

        alive.store(false, Ordering::SeqCst);
        let out_result = relay_out.join().map_err(|_| "stdout relay thread panicked".to_string())?;
        let resize_result = resize_loop.join().map_err(|_| "resize thread panicked".to_string())?;
        out_result?;
        resize_result
    }
}

struct TerminalModeGuard {
    fd: RawFd,
    original: Option<libc::termios>,
}

impl TerminalModeGuard {
    fn activate() -> Result<Self, String> {
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: `isatty` only queries terminal state for the live stdin fd.
        if unsafe { libc::isatty(fd) } != 1 {
            return Ok(Self { fd, original: None });
        }

        let mut original =
            libc::termios { c_iflag: 0, c_oflag: 0, c_cflag: 0, c_lflag: 0, c_line: 0, c_cc: [0; libc::NCCS], c_ispeed: 0, c_ospeed: 0 };
        // SAFETY: `tcgetattr` initializes `original` for the live stdin tty fd.
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(format!("read terminal attrs: {}", std::io::Error::last_os_error()));
        }

        let mut raw = original;
        // SAFETY: `cfmakeraw` mutates the local termios value before it is applied.
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        // SAFETY: `tcsetattr` applies the computed raw mode to the same stdin tty fd.
        if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) } != 0 {
            return Err(format!("set terminal raw mode: {}", std::io::Error::last_os_error()));
        }

        Ok(Self { fd, original: Some(original) })
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        if let Some(original) = self.original {
            // SAFETY: restore the previously captured terminal attributes to the same fd.
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSAFLUSH, &original);
            }
        }
    }
}

pub fn ensure_session_started(
    layout: &RuntimeLayout,
    name: Option<String>,
    cwd: Option<PathBuf>,
    cmd: Option<String>,
) -> Result<SessionMetadata, String> {
    let session = if let Some(existing) = name.as_deref().and_then(|value| load_session(layout.root(), value).ok().flatten()) {
        existing
    } else {
        layout.create_session(name, cwd, cmd)?.metadata
    };

    let socket_path = session_socket_path(layout.root(), &session.id);
    if !socket_path.exists() {
        spawn_daemon_process(layout.root(), &session)?;
        wait_for_socket(&socket_path)?;
    }

    Ok(session)
}

pub fn attach_foreground(layout: &RuntimeLayout, id: &str) -> Result<ForegroundAttach, String> {
    let socket_path = session_socket_path(layout.root(), id);
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        let mut stream = UnixStream::connect(&socket_path).map_err(|err| format!("connect {}: {err}", socket_path.display()))?;
        let (cols, rows) = current_terminal_size();
        Frame::AttachInit { cols, rows }.write(&mut stream).map_err(|err| format!("write attach init: {err}"))?;
        match Frame::read(&mut stream).map_err(|err| format!("read attach response: {err}"))? {
            Frame::Ack => return Ok(ForegroundAttach { stream: Arc::new(Mutex::new(stream)) }),
            Frame::Busy => {}
            other => return Err(format!("unexpected attach response: {other:?}")),
        }
        if Instant::now() >= deadline {
            return Err(format!("session {id} already has a foreground client"));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub fn session_socket_path(root: &Path, id: &str) -> PathBuf {
    root.join(id).join(SOCKET_NAME)
}

pub fn daemon_pid_path(root: &Path, id: &str) -> PathBuf {
    root.join(id).join(PID_NAME)
}

pub fn foreground_path(root: &Path, id: &str) -> PathBuf {
    root.join(id).join(FOREGROUND_NAME)
}

fn default_vt_engine() -> Box<dyn VtEngine> {
    Box::new(PassthroughVtEngine::new(DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS))
}

fn record_pty_output(engine: &mut dyn VtEngine, bytes: &[u8]) -> Result<(), String> {
    engine.feed(bytes)
}

fn apply_attach_state(engine: &mut dyn VtEngine, cols: u16, rows: u16) -> Result<Option<Vec<u8>>, String> {
    engine.resize(cols, rows)?;
    if engine.supports_replay() {
        engine.replay_payload()
    } else {
        Ok(None)
    }
}

#[cfg(unix)]
pub fn run_session_daemon(root: &Path, id: &str) -> Result<(), String> {
    let session = load_session(root, id)?.ok_or_else(|| format!("missing session metadata for {id}"))?;
    let socket_path = session_socket_path(root, id);
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    let listener =
        std::os::unix::net::UnixListener::bind(&socket_path).map_err(|err| format!("bind socket {}: {err}", socket_path.display()))?;
    listener.set_nonblocking(true).map_err(|err| format!("set listener nonblocking: {err}"))?;
    fs::write(daemon_pid_path(root, id), std::process::id().to_string()).map_err(|err| format!("write daemon pid: {err}"))?;

    let pty_fd = spawn_pty_child(&session)?;
    set_nonblocking(pty_fd)?;
    let mut vt_engine = default_vt_engine();

    let mut active_client: Option<UnixStream> = None;
    loop {
        let poll_result = poll_ready(listener.as_raw_fd(), active_client.as_ref().map(AsRawFd::as_raw_fd), pty_fd, 100)?;

        if poll_result.listener_readable {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_millis(10))).map_err(|err| format!("set client read timeout: {err}"))?;
                    if let Ok(Frame::AttachInit { cols, rows }) = Frame::read(&mut stream) {
                        if active_client.is_none() {
                            resize_pty(pty_fd, cols, rows)?;
                            let replay = apply_attach_state(vt_engine.as_mut(), cols, rows)?;
                            Frame::Ack.write(&mut stream).map_err(|err| format!("write attach ack: {err}"))?;
                            if let Some(payload) = replay {
                                if !payload.is_empty() {
                                    Frame::Output(payload).write(&mut stream).map_err(|err| format!("write replay output: {err}"))?;
                                }
                            }
                            stream.set_nonblocking(true).map_err(|err| format!("set client nonblocking: {err}"))?;
                            let _ = fs::write(foreground_path(root, id), b"1");
                            active_client = Some(stream);
                        } else {
                            let _ = Frame::Busy.write(&mut stream);
                        }
                    } else {
                        let _ = Frame::Busy.write(&mut stream);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(err) => return Err(format!("accept client: {err}")),
            }
        }

        if poll_result.client_readable {
            let mut client_disconnected = false;
            let mut pending = VecDeque::new();
            if let Some(stream) = active_client.as_mut() {
                loop {
                    match Frame::read(stream) {
                        Ok(frame) => pending.push_back(frame),
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(err)
                            if matches!(
                                err.kind(),
                                std::io::ErrorKind::UnexpectedEof
                                    | std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionAborted
                            ) =>
                        {
                            client_disconnected = true;
                            break;
                        }
                        Err(err) => return Err(format!("read client frame: {err}")),
                    }
                }
            }

            while let Some(frame) = pending.pop_front() {
                match frame {
                    Frame::Input(bytes) => write_fd_all(pty_fd, &bytes)?,
                    Frame::Resize { cols, rows } => {
                        resize_pty(pty_fd, cols, rows)?;
                        vt_engine.resize(cols, rows)?;
                    }
                    _ => {}
                }
            }

            if client_disconnected {
                let _ = fs::remove_file(foreground_path(root, id));
                active_client = None;
            }
        }

        if poll_result.pty_readable {
            loop {
                let mut buf = [0u8; 4096];
                match read_fd(pty_fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        record_pty_output(vt_engine.as_mut(), &buf[..n])?;
                        if let Some(stream) = active_client.as_mut() {
                            if Frame::Output(buf[..n].to_vec()).write(stream).is_err() {
                                let _ = fs::remove_file(foreground_path(root, id));
                                active_client = None;
                            }
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(err) => return Err(format!("read pty output: {err}")),
                }
            }
        }

        if child_exited()?.is_some() {
            break;
        }
    }

    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(daemon_pid_path(root, id));
    let _ = fs::remove_file(foreground_path(root, id));
    Ok(())
}

#[cfg(not(unix))]
pub fn run_session_daemon(_root: &Path, _id: &str) -> Result<(), String> {
    Err("session daemon is only supported on unix".into())
}

fn spawn_daemon_process(root: &Path, session: &SessionMetadata) -> Result<(), String> {
    let exe = resolve_bollard_executable()?;
    let mut command = Command::new(exe);
    command
        .arg("--runtime-root")
        .arg(root)
        .arg("serve")
        .arg("--id")
        .arg(&session.id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn().map_err(|err| format!("spawn session daemon for {}: {err}", session.id))?;
    fs::write(daemon_pid_path(root, &session.id), child.id().to_string()).map_err(|err| format!("write daemon pid: {err}"))?;
    Ok(())
}

fn resolve_bollard_executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_bollard").map(PathBuf::from) {
        return Ok(path);
    }

    let path_var = std::env::var_os("PATH").ok_or_else(|| "PATH is not set; cannot locate bollard executable".to_string())?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("bollard");
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }

    Err("unable to locate bollard executable in PATH".into())
}

fn is_executable_file(path: &Path) -> bool {
    path.is_file() && fs::metadata(path).map(|metadata| metadata.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

struct PollResult {
    listener_readable: bool,
    client_readable: bool,
    pty_readable: bool,
}

fn poll_ready(listener_fd: RawFd, client_fd: Option<RawFd>, pty_fd: RawFd, timeout_ms: i32) -> Result<PollResult, String> {
    let mut fds = vec![libc::pollfd { fd: listener_fd, events: libc::POLLIN, revents: 0 }, libc::pollfd {
        fd: pty_fd,
        events: libc::POLLIN,
        revents: 0,
    }];
    let client_index = if let Some(fd) = client_fd {
        fds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
        Some(fds.len() - 1)
    } else {
        None
    };

    // SAFETY: `poll` reads and writes the provided pollfd array for valid fds owned by this process.
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
    if rc < 0 {
        return Err(format!("poll daemon fds: {}", std::io::Error::last_os_error()));
    }

    Ok(PollResult {
        listener_readable: fds[0].revents & libc::POLLIN != 0,
        pty_readable: fds[1].revents & libc::POLLIN != 0,
        client_readable: client_index.map(|index| fds[index].revents & libc::POLLIN != 0).unwrap_or(false),
    })
}

fn wait_for_socket(path: &Path) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(format!("timed out waiting for socket {}", path.display()))
}

fn current_terminal_size() -> (u16, u16) {
    #[cfg(unix)]
    {
        let fd = std::io::stdout().as_raw_fd();
        let mut winsize = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
        let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut winsize) };
        if rc == 0 && winsize.ws_col > 0 && winsize.ws_row > 0 {
            return (winsize.ws_col, winsize.ws_row);
        }
    }
    let cols = std::env::var("COLUMNS").ok().and_then(|value| value.parse::<u16>().ok()).unwrap_or(80);
    let rows = std::env::var("LINES").ok().and_then(|value| value.parse::<u16>().ok()).unwrap_or(24);
    (cols, rows)
}

fn load_session(root: &Path, id: &str) -> Result<Option<SessionMetadata>, String> {
    let path = root.join(id).join("meta.json");
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path).map_err(|err| format!("read metadata {}: {err}", path.display()))?;
    serde_json::from_str(&contents).map(Some).map_err(|err| format!("parse metadata {}: {err}", path.display()))
}

#[cfg(unix)]
fn spawn_pty_child(session: &SessionMetadata) -> Result<RawFd, String> {
    let mut master_fd: libc::c_int = -1;
    // SAFETY: forkpty creates a child attached to a new PTY and initializes master_fd on success.
    let result = unsafe { libc::forkpty(&mut master_fd, std::ptr::null_mut(), std::ptr::null(), std::ptr::null()) };
    if result < 0 {
        return Err("forkpty failed".into());
    }
    if result == 0 {
        if let Some(cwd) = &session.cwd {
            let cwd_c = CString::new(cwd.as_os_str().as_encoded_bytes().to_vec()).map_err(|_| "cwd contains interior nul".to_string())?;
            // SAFETY: chdir uses a valid nul-terminated path in the child process before exec.
            unsafe {
                libc::chdir(cwd_c.as_ptr());
            }
        }
        for key in STRIP_ENV_VARS {
            let key_c = CString::new(*key).map_err(|_| format!("invalid env key {key}"))?;
            // SAFETY: unsetenv receives a valid nul-terminated environment variable name in the child process.
            unsafe {
                libc::unsetenv(key_c.as_ptr());
            }
        }
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let shell_c = CString::new(shell.clone()).map_err(|_| "shell contains interior nul".to_string())?;
        let arg0 = CString::new(shell).map_err(|_| "shell contains interior nul".to_string())?;
        if let Some(cmd) = &session.cmd {
            let dash_lc = CString::new("-lc").map_err(|_| "invalid -lc".to_string())?;
            let cmd_c = CString::new(cmd.as_str()).map_err(|_| "cmd contains interior nul".to_string())?;
            // SAFETY: execl replaces the child process image with the requested shell command; arguments are valid C strings.
            unsafe {
                libc::execl(shell_c.as_ptr(), arg0.as_ptr(), dash_lc.as_ptr(), cmd_c.as_ptr(), std::ptr::null::<i8>());
                libc::_exit(127);
            }
        } else {
            // SAFETY: execl replaces the child process image with the requested shell; arguments are valid C strings.
            unsafe {
                libc::execl(shell_c.as_ptr(), arg0.as_ptr(), std::ptr::null::<i8>());
                libc::_exit(127);
            }
        }
    }
    Ok(master_fd)
}

#[cfg(unix)]
fn set_nonblocking(fd: RawFd) -> Result<(), String> {
    // SAFETY: fcntl reads the current descriptor flags for a valid PTY master fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err("fcntl F_GETFL failed".into());
    }
    // SAFETY: fcntl updates the descriptor flags for the same valid PTY master fd.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err("fcntl F_SETFL failed".into());
    }
    Ok(())
}

#[cfg(unix)]
fn read_fd(fd: RawFd, buf: &mut [u8]) -> Result<usize, std::io::Error> {
    // SAFETY: read writes into the provided mutable buffer for a valid PTY master fd.
    let rc = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if rc >= 0 {
        Ok(rc as usize)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn write_fd_all(fd: RawFd, mut bytes: &[u8]) -> Result<(), String> {
    while !bytes.is_empty() {
        // SAFETY: write reads from the provided byte slice for a valid PTY master fd.
        let rc = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            return Err(format!("write pty input: {err}"));
        }
        bytes = &bytes[rc as usize..];
    }
    Ok(())
}

#[cfg(unix)]
fn resize_pty(fd: RawFd, cols: u16, rows: u16) -> Result<(), String> {
    let winsize = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: ioctl updates the window size for a valid PTY master fd using a properly initialized winsize.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &winsize) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("resize pty: {}", std::io::Error::last_os_error()))
    }
}

#[cfg(unix)]
fn child_exited() -> Result<Option<i32>, String> {
    let mut status = 0;
    // SAFETY: waitpid with WNOHANG queries child exit state without blocking and writes into `status`.
    let rc = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ECHILD) {
            return Ok(None);
        }
        return Err(format!("waitpid failed: {err}"));
    }
    if rc == 0 {
        Ok(None)
    } else {
        Ok(Some(status))
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::{apply_attach_state, default_vt_engine, is_executable_file, record_pty_output, resolve_bollard_executable};

    #[test]
    fn default_vt_engine_starts_with_default_size() {
        let engine = default_vt_engine();
        assert_eq!(engine.size(), (super::DEFAULT_TERMINAL_COLS, super::DEFAULT_TERMINAL_ROWS));
    }

    #[test]
    fn vt_engine_helpers_feed_and_resize_passthrough_engine() {
        let mut engine = default_vt_engine();
        record_pty_output(engine.as_mut(), b"hello").expect("feed output");
        let replay = apply_attach_state(engine.as_mut(), 132, 40).expect("apply attach state");

        assert_eq!(engine.size(), (132, 40));
        assert_eq!(replay, None);
    }

    #[test]
    fn resolve_bollard_executable_prefers_cargo_bin_env() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bollard = temp.path().join("bollard");
        fs::write(&bollard, b"#!/bin/sh\n").expect("write fake bollard");
        let original = std::env::var_os("CARGO_BIN_EXE_bollard");
        std::env::set_var("CARGO_BIN_EXE_bollard", &bollard);

        let resolved = resolve_bollard_executable().expect("resolve bollard");

        match original {
            Some(value) => std::env::set_var("CARGO_BIN_EXE_bollard", value),
            None => std::env::remove_var("CARGO_BIN_EXE_bollard"),
        }
        assert_eq!(resolved, bollard);
    }

    #[test]
    fn resolve_bollard_executable_falls_back_to_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bin_dir = temp.path().join("bin");
        fs::create_dir_all(&bin_dir).expect("create bin dir");
        let bollard = bin_dir.join("bollard");
        fs::write(&bollard, b"#!/bin/sh\n").expect("write fake bollard");
        let mut perms = fs::metadata(&bollard).expect("metadata").permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            perms.set_mode(0o755);
            fs::set_permissions(&bollard, perms).expect("set executable");
        }

        let original_bin = std::env::var_os("CARGO_BIN_EXE_bollard");
        let original_path = std::env::var_os("PATH");
        std::env::remove_var("CARGO_BIN_EXE_bollard");
        std::env::set_var("PATH", PathBuf::from(&bin_dir).into_os_string());

        let resolved = resolve_bollard_executable().expect("resolve from path");

        match original_bin {
            Some(value) => std::env::set_var("CARGO_BIN_EXE_bollard", value),
            None => std::env::remove_var("CARGO_BIN_EXE_bollard"),
        }
        match original_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
        assert_eq!(resolved, bollard);
        assert!(is_executable_file(&bollard));
    }
}
