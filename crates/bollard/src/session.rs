use std::{
    collections::VecDeque,
    ffi::CString,
    fs,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, BorrowedFd, IntoRawFd, RawFd},
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

use nix::{
    fcntl::{fcntl, FcntlArg, OFlag},
    poll::{poll, PollFd, PollFlags, PollTimeout},
    pty::{forkpty, ForkptyResult},
    sys::{
        termios::{self, SetArg},
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::{chdir, execvp, isatty, read as nix_read, write as nix_write, Pid},
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
    original: Option<termios::Termios>,
}

impl TerminalModeGuard {
    fn activate() -> Result<Self, String> {
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: stdin remains open for the lifetime of the guard; we only borrow its fd.
        let borrowed_fd = unsafe { BorrowedFd::borrow_raw(fd) };
        if !isatty(borrowed_fd).map_err(|err| format!("detect terminal stdin: {err}"))? {
            return Ok(Self { fd, original: None });
        }

        let original = termios::tcgetattr(borrowed_fd).map_err(|err| format!("read terminal attrs: {err}"))?;
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(borrowed_fd, SetArg::TCSAFLUSH, &raw).map_err(|err| format!("set terminal raw mode: {err}"))?;

        Ok(Self { fd, original: Some(original) })
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        if let Some(original) = self.original.as_ref() {
            // SAFETY: stdin remains open for the lifetime of the guard; we only borrow its fd.
            let borrowed_fd = unsafe { BorrowedFd::borrow_raw(self.fd) };
            let _ = termios::tcsetattr(borrowed_fd, SetArg::TCSAFLUSH, original);
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

    if let Some(path) = current_exe_sibling("bollard") {
        return Ok(path);
    }

    Err("unable to locate bollard executable in PATH".into())
}

fn current_exe_sibling(name: &str) -> Option<PathBuf> {
    let current_exe = std::env::current_exe().ok()?;
    let current_dir = current_exe.parent()?;
    let candidates = [current_dir.join(name), current_dir.parent().map(|parent| parent.join(name))?];
    candidates.into_iter().find(|candidate| is_executable_file(candidate))
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
    let listener_borrowed = borrow_fd(listener_fd);
    let pty_borrowed = borrow_fd(pty_fd);
    let mut fds = vec![PollFd::new(listener_borrowed, PollFlags::POLLIN), PollFd::new(pty_borrowed, PollFlags::POLLIN)];
    let client_index = if let Some(fd) = client_fd {
        fds.push(PollFd::new(borrow_fd(fd), PollFlags::POLLIN));
        Some(fds.len() - 1)
    } else {
        None
    };

    poll(&mut fds, PollTimeout::try_from(timeout_ms).map_err(|err| format!("invalid poll timeout: {err}"))?)
        .map_err(|err| format!("poll daemon fds: {err}"))?;

    Ok(PollResult {
        listener_readable: has_pollin(&fds[0]),
        pty_readable: has_pollin(&fds[1]),
        client_readable: client_index.map(|index| has_pollin(&fds[index])).unwrap_or(false),
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

fn has_pollin(fd: &PollFd<'_>) -> bool {
    fd.revents().map(|flags| flags.contains(PollFlags::POLLIN)).unwrap_or(false)
}

fn borrow_fd(fd: RawFd) -> BorrowedFd<'static> {
    // SAFETY: callers only pass live fds owned by this process, and the borrowed view is used immediately.
    unsafe { BorrowedFd::borrow_raw(fd) }
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
    // SAFETY: `forkpty` creates a child attached to a new PTY; parent receives the owned master fd.
    let result = unsafe { forkpty(None, None) }.map_err(|err| format!("forkpty failed: {err}"))?;
    match result {
        ForkptyResult::Parent { master, .. } => Ok(master.into_raw_fd()),
        ForkptyResult::Child => {
            if let Some(cwd) = &session.cwd {
                let _ = chdir(cwd);
            }
            for key in STRIP_ENV_VARS {
                // SAFETY: child process is single-threaded here, before exec, so environment mutation is safe.
                unsafe {
                    std::env::remove_var(key);
                }
            }
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            let shell_c = CString::new(shell.clone()).map_err(|_| "shell contains interior nul".to_string())?;
            let mut args = vec![shell_c.clone()];
            if let Some(cmd) = &session.cmd {
                args.push(CString::new("-lc").map_err(|_| "invalid -lc".to_string())?);
                args.push(CString::new(cmd.as_str()).map_err(|_| "cmd contains interior nul".to_string())?);
            }
            let _ = execvp(&shell_c, &args);
            std::process::exit(127);
        }
    }
}

#[cfg(unix)]
fn set_nonblocking(fd: RawFd) -> Result<(), String> {
    let borrowed = borrow_fd(fd);
    let flags = fcntl(borrowed, FcntlArg::F_GETFL).map_err(|err| format!("fcntl F_GETFL failed: {err}"))?;
    let mut oflags = OFlag::from_bits_retain(flags);
    oflags.insert(OFlag::O_NONBLOCK);
    fcntl(borrowed, FcntlArg::F_SETFL(oflags)).map_err(|err| format!("fcntl F_SETFL failed: {err}"))?;
    Ok(())
}

#[cfg(unix)]
fn read_fd(fd: RawFd, buf: &mut [u8]) -> Result<usize, std::io::Error> {
    nix_read(borrow_fd(fd), buf).map_err(std::io::Error::from)
}

#[cfg(unix)]
fn write_fd_all(fd: RawFd, mut bytes: &[u8]) -> Result<(), String> {
    while !bytes.is_empty() {
        match nix_write(borrow_fd(fd), bytes) {
            Ok(written) => bytes = &bytes[written..],
            Err(err) => {
                let err = std::io::Error::from(err);
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                return Err(format!("write pty input: {err}"));
            }
        }
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
    match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::StillAlive) => Ok(None),
        Ok(_) => Ok(Some(1)),
        Err(nix::errno::Errno::ECHILD) => Ok(None),
        Err(err) => Err(format!("waitpid failed: {err}")),
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
