use std::{
    collections::VecDeque,
    ffi::CString,
    fs,
    io::{Read, Write},
    net::Shutdown,
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
    errno::Errno,
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
    vt::{self, VtEngine, VtEngineKind},
};

const SOCKET_NAME: &str = "socket";
const PID_NAME: &str = "daemon.pid";
const FOREGROUND_NAME: &str = "foreground";
const STRIP_ENV_VARS: &[&str] = &["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"];
const DEFAULT_TERMINAL_COLS: u16 = 80;
const DEFAULT_TERMINAL_ROWS: u16 = 24;
const DETACH_CLEANUP_SEQUENCE: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[?1049l\x1b[<u\x1b[?25h";
const PTY_READ_BUFFER_SIZE: usize = 64 * 1024;
static ATTACH_SIGNAL_EXIT: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub struct ForegroundAttach {
    stream: Arc<Mutex<UnixStream>>,
}

impl ForegroundAttach {
    pub fn relay_stdio(self) -> Result<(), String> {
        let cleanup = AttachCleanupGuard::stdout();
        let _tty_mode = TerminalModeGuard::activate()?;
        let _signal_handlers = AttachSignalHandlers::install()?;
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
                        if is_graceful_socket_shutdown(&err) {
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
        let stdin_fd = stdin.as_raw_fd();
        let mut buf = [0u8; 4096];
        let stdin_result = loop {
            if !alive.load(Ordering::SeqCst) || ATTACH_SIGNAL_EXIT.load(Ordering::SeqCst) {
                break Ok(());
            }
            match poll_fd_readable(stdin_fd, 100) {
                Ok(false) => continue,
                Ok(true) => {}
                Err(_err) if ATTACH_SIGNAL_EXIT.load(Ordering::SeqCst) => break Ok(()),
                Err(err) => break Err(err),
            }
            match stdin.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => {
                    let mut stream = self.stream.lock().map_err(|_| "attach stream lock poisoned".to_string())?;
                    if let Err(err) = Frame::Input(buf[..n].to_vec()).write(&mut *stream) {
                        if is_graceful_socket_shutdown(&err) {
                            break Ok(());
                        }
                        break Err(format!("write input frame: {err}"));
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => break Err(format!("read stdin: {err}")),
            }
        };

        let signal_exit = ATTACH_SIGNAL_EXIT.load(Ordering::SeqCst);
        alive.store(false, Ordering::SeqCst);
        if let Ok(stream) = self.stream.lock() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        let out_result = relay_out.join().map_err(|_| "stdout relay thread panicked".to_string())?;
        let resize_result = resize_loop.join().map_err(|_| "resize thread panicked".to_string())?;
        cleanup.emit()?;
        if signal_exit {
            return Ok(());
        }
        stdin_result?;
        out_result?;
        resize_result
    }
}

fn is_graceful_socket_shutdown(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
    )
}

enum AttachCleanupTarget {
    Stdout,
    #[cfg(test)]
    Buffer(Arc<Mutex<Vec<u8>>>),
}

struct AttachCleanupGuard {
    target: AttachCleanupTarget,
    enabled: bool,
}

struct AttachSignalHandlers {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl AttachSignalHandlers {
    fn install() -> Result<Self, String> {
        ATTACH_SIGNAL_EXIT.store(false, Ordering::SeqCst);
        let mut previous = Vec::new();
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let mut old = unsafe { std::mem::zeroed::<libc::sigaction>() };
            let mut new = unsafe { std::mem::zeroed::<libc::sigaction>() };
            new.sa_sigaction = attach_signal_handler as *const () as usize;
            new.sa_flags = 0;
            unsafe {
                libc::sigemptyset(&mut new.sa_mask);
            }
            let rc = unsafe { libc::sigaction(signal, &new, &mut old) };
            if rc != 0 {
                return Err(format!("install signal handler {signal}: {}", std::io::Error::last_os_error()));
            }
            previous.push((signal, old));
        }
        Ok(Self { previous })
    }
}

impl Drop for AttachSignalHandlers {
    fn drop(&mut self) {
        ATTACH_SIGNAL_EXIT.store(false, Ordering::SeqCst);
        for (signal, action) in self.previous.drain(..).rev() {
            unsafe {
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    }
}

extern "C" fn attach_signal_handler(_signal: libc::c_int) {
    ATTACH_SIGNAL_EXIT.store(true, Ordering::SeqCst);
}

impl AttachCleanupGuard {
    fn stdout() -> Self {
        Self { target: AttachCleanupTarget::Stdout, enabled: stdout_is_tty().unwrap_or(false) }
    }

    #[cfg(test)]
    fn test_buffer(buffer: Arc<Mutex<Vec<u8>>>) -> Self {
        Self { target: AttachCleanupTarget::Buffer(buffer), enabled: true }
    }

    #[cfg(test)]
    fn test_buffer_disabled(buffer: Arc<Mutex<Vec<u8>>>) -> Self {
        Self { target: AttachCleanupTarget::Buffer(buffer), enabled: false }
    }

    fn emit(self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        match self.target {
            AttachCleanupTarget::Stdout => {
                let mut stdout = std::io::stdout().lock();
                write_detach_cleanup(&mut stdout)
            }
            #[cfg(test)]
            AttachCleanupTarget::Buffer(buffer) => {
                if let Ok(mut buffer) = buffer.lock() {
                    write_detach_cleanup(&mut *buffer)
                } else {
                    Err("cleanup buffer lock poisoned".to_string())
                }
            }
        }
    }
}

fn write_detach_cleanup<W: Write>(writer: &mut W) -> Result<(), String> {
    writer.write_all(DETACH_CLEANUP_SEQUENCE).map_err(|err| format!("write detach cleanup: {err}"))?;
    writer.flush().map_err(|err| format!("flush detach cleanup: {err}"))
}

fn stdout_is_tty() -> Result<bool, String> {
    let fd = std::io::stdout().as_raw_fd();
    // SAFETY: stdout remains open for the duration of this check; we only borrow its fd.
    let borrowed_fd = unsafe { BorrowedFd::borrow_raw(fd) };
    isatty(borrowed_fd).map_err(|err| format!("detect terminal stdout: {err}"))
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
    vt_engine: Option<VtEngineKind>,
    cwd: Option<PathBuf>,
    cmd: Option<String>,
) -> Result<SessionMetadata, String> {
    let session = if let Some(existing) = name.as_deref().and_then(|value| load_session(layout.root(), value).ok().flatten()) {
        existing
    } else {
        let vt_engine = vt_engine.unwrap_or_else(vt::default_vt_engine_kind);
        vt_engine.ensure_available()?;
        layout.create_session(name, vt_engine, cwd, cmd)?.metadata
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
        Frame::AttachInit { cols, rows, capabilities: attach_init_capabilities() }
            .write(&mut stream)
            .map_err(|err| format!("write attach init: {err}"))?;
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

fn default_vt_engine(kind: VtEngineKind) -> Result<Box<dyn VtEngine>, String> {
    if std::env::var_os("CARGO_BIN_EXE_cleat").is_some()
        && std::env::var_os("CLEAT_TEST_VT_ENGINE").as_deref() == Some(std::ffi::OsStr::new("replay-probe"))
    {
        return Ok(Box::new(TestReplayProbeVtEngine::new(DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS)));
    }
    vt::make_vt_engine(kind, DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS)
}

#[derive(Debug)]
struct TestReplayProbeVtEngine {
    cols: u16,
    rows: u16,
}

impl TestReplayProbeVtEngine {
    fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }
}

impl VtEngine for TestReplayProbeVtEngine {
    fn feed(&mut self, _bytes: &[u8]) -> Result<(), String> {
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    fn supports_replay(&self) -> bool {
        true
    }

    fn replay_payload(&self, capabilities: &vt::ClientCapabilities) -> Result<Option<Vec<u8>>, String> {
        let payload = format!("{:?}:{}", capabilities.color_level, capabilities.kitty_keyboard);
        Ok(Some(payload.into_bytes()))
    }

    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }
}

fn record_pty_output(engine: &mut dyn VtEngine, bytes: &[u8]) -> Result<(), String> {
    engine.feed(bytes)
}

fn attach_init_capabilities() -> vt::ClientCapabilities {
    vt::ClientCapabilities::conservative_fallback()
}

fn apply_attach_state(
    engine: &mut dyn VtEngine,
    cols: u16,
    rows: u16,
    capabilities: &vt::ClientCapabilities,
) -> Result<Option<Vec<u8>>, String> {
    engine.resize(cols, rows)?;
    if engine.supports_replay() {
        engine.replay_payload(capabilities)
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

    let pty_child = spawn_pty_child(&session)?;
    let pty_fd = pty_child.master_fd;
    set_nonblocking(pty_fd)?;
    let mut vt_engine = default_vt_engine(session.vt_engine)?;

    let mut active_client: Option<ActiveClient> = None;
    loop {
        let poll_result = poll_ready(
            listener.as_raw_fd(),
            active_client.as_ref().map(|client| client.stream.as_raw_fd()),
            active_client.as_ref().map(|client| !client.pending_output.is_empty()).unwrap_or(false),
            pty_fd,
            100,
        )?;

        if poll_result.listener_readable {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_millis(10))).map_err(|err| format!("set client read timeout: {err}"))?;
                    match Frame::read(&mut stream) {
                        Ok(Frame::AttachInit { cols, rows, capabilities }) => {
                            if active_client.is_none() {
                                resize_pty(pty_fd, cols, rows)?;
                                let replay = apply_attach_state(vt_engine.as_mut(), cols, rows, &capabilities)?;
                                Frame::Ack.write(&mut stream).map_err(|err| format!("write attach ack: {err}"))?;
                                stream.set_nonblocking(true).map_err(|err| format!("set client nonblocking: {err}"))?;
                                let mut client = ActiveClient::new(stream);
                                if let Some(payload) = replay {
                                    if !payload.is_empty() {
                                        client.enqueue_frame(&Frame::Output(payload))?;
                                    }
                                }
                                let _ = fs::write(foreground_path(root, id), b"1");
                                active_client = Some(client);
                            } else {
                                let _ = Frame::Busy.write(&mut stream);
                            }
                        }
                        Ok(Frame::Detach) => {
                            let _ = fs::remove_file(foreground_path(root, id));
                            active_client = None;
                        }
                        Ok(_) => {
                            let _ = Frame::Busy.write(&mut stream);
                        }
                        Err(_) => {
                            let _ = Frame::Busy.write(&mut stream);
                        }
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
                    match Frame::read(&mut stream.stream) {
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

        if poll_result.client_writable {
            let client_writable = match active_client.as_mut() {
                Some(client) => client.flush_pending_output()?,
                None => true,
            };
            if !client_writable {
                let _ = fs::remove_file(foreground_path(root, id));
                active_client = None;
            }
        }

        if poll_result.pty_readable {
            loop {
                let mut buf = [0u8; PTY_READ_BUFFER_SIZE];
                match read_fd(pty_fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        record_pty_output(vt_engine.as_mut(), &buf[..n])?;
                        if let Some(client) = active_client.as_mut() {
                            if client.enqueue_frame(&Frame::Output(buf[..n].to_vec())).is_err() {
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

        if child_exited(pty_child.pid)?.is_some() {
            break;
        }
    }

    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(daemon_pid_path(root, id));
    let _ = fs::remove_file(foreground_path(root, id));
    let _ = fs::remove_dir_all(root.join(id));
    Ok(())
}

#[cfg(not(unix))]
pub fn run_session_daemon(_root: &Path, _id: &str) -> Result<(), String> {
    Err("session daemon is only supported on unix".into())
}

fn spawn_daemon_process(root: &Path, session: &SessionMetadata) -> Result<(), String> {
    let exe = resolve_cleat_executable()?;
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

fn resolve_cleat_executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_cleat").map(PathBuf::from) {
        return Ok(path);
    }

    let path_var = std::env::var_os("PATH").ok_or_else(|| "PATH is not set; cannot locate cleat executable".to_string())?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("cleat");
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }

    if let Some(path) = current_exe_sibling("cleat") {
        return Ok(path);
    }

    Err("unable to locate cleat executable in PATH".into())
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
    client_writable: bool,
    pty_readable: bool,
}

struct ActiveClient {
    stream: UnixStream,
    pending_output: Vec<u8>,
}

impl ActiveClient {
    fn new(stream: UnixStream) -> Self {
        Self { stream, pending_output: Vec::new() }
    }

    fn enqueue_frame(&mut self, frame: &Frame) -> Result<(), String> {
        frame.write(&mut self.pending_output).map_err(|err| format!("buffer client frame: {err}"))
    }

    fn flush_pending_output(&mut self) -> Result<bool, String> {
        while !self.pending_output.is_empty() {
            match self.stream.write(&self.pending_output) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    self.pending_output.drain(..n);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) if is_graceful_socket_shutdown(&err) => return Ok(false),
                Err(err) => return Err(format!("flush client output: {err}")),
            }
        }
        Ok(true)
    }
}

fn poll_fd_readable(fd: RawFd, timeout_ms: i32) -> Result<bool, String> {
    // SAFETY: the fd remains open for the duration of the poll call; we only borrow it temporarily.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
    match poll(&mut fds, PollTimeout::try_from(timeout_ms).map_err(|err| format!("invalid poll timeout: {err}"))?) {
        Ok(_) => {}
        Err(Errno::EINTR) => return Ok(false),
        Err(err) => return Err(format!("poll readable fd: {err}")),
    }
    Ok(has_pollin(&fds[0]))
}

struct PtyChild {
    master_fd: RawFd,
    pid: Pid,
}

fn poll_ready(
    listener_fd: RawFd,
    client_fd: Option<RawFd>,
    client_needs_write: bool,
    pty_fd: RawFd,
    timeout_ms: i32,
) -> Result<PollResult, String> {
    // SAFETY: the fds are owned by this process and remain open for the duration of the poll call.
    let listener_borrowed = unsafe { BorrowedFd::borrow_raw(listener_fd) };
    // SAFETY: the fd is owned by this process and remains open for the duration of the poll call.
    let pty_borrowed = unsafe { BorrowedFd::borrow_raw(pty_fd) };
    let mut fds = vec![PollFd::new(listener_borrowed, PollFlags::POLLIN), PollFd::new(pty_borrowed, PollFlags::POLLIN)];
    let client_index = if let Some(fd) = client_fd {
        // SAFETY: the client fd is owned by this process and remains open for the duration of the poll call.
        let client_borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let mut flags = PollFlags::POLLIN;
        if client_needs_write {
            flags |= PollFlags::POLLOUT;
        }
        fds.push(PollFd::new(client_borrowed, flags));
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
        client_writable: client_index.map(|index| has_pollout(&fds[index])).unwrap_or(false),
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

fn has_pollout(fd: &PollFd<'_>) -> bool {
    fd.revents().map(|flags| flags.contains(PollFlags::POLLOUT)).unwrap_or(false)
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
fn spawn_pty_child(session: &SessionMetadata) -> Result<PtyChild, String> {
    // SAFETY: `forkpty` creates a child attached to a new PTY; parent receives the owned master fd.
    let result = unsafe { forkpty(None, None) }.map_err(|err| format!("forkpty failed: {err}"))?;
    match result {
        ForkptyResult::Parent { master, child } => Ok(PtyChild { master_fd: master.into_raw_fd(), pid: child }),
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
    // SAFETY: the fd is owned by this process and remains open for the duration of these fcntl calls.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let flags = fcntl(borrowed, FcntlArg::F_GETFL).map_err(|err| format!("fcntl F_GETFL failed: {err}"))?;
    let mut oflags = OFlag::from_bits_retain(flags);
    oflags.insert(OFlag::O_NONBLOCK);
    fcntl(borrowed, FcntlArg::F_SETFL(oflags)).map_err(|err| format!("fcntl F_SETFL failed: {err}"))?;
    Ok(())
}

#[cfg(unix)]
fn read_fd(fd: RawFd, buf: &mut [u8]) -> Result<usize, std::io::Error> {
    // SAFETY: the fd is owned by this process and remains open for the duration of the read call.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    nix_read(borrowed, buf).map_err(std::io::Error::from)
}

#[cfg(unix)]
fn write_fd_all(fd: RawFd, mut bytes: &[u8]) -> Result<(), String> {
    while !bytes.is_empty() {
        // SAFETY: the fd is owned by this process and remains open for the duration of the write call.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        match nix_write(borrowed, bytes) {
            Ok(written) => bytes = &bytes[written..],
            Err(err) => {
                let err = std::io::Error::from(err);
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    wait_for_writable(fd)?;
                    continue;
                }
                return Err(format!("write pty input: {err}"));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn wait_for_writable(fd: RawFd) -> Result<(), String> {
    // SAFETY: the fd is owned by this process and remains open for the duration of the poll call.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut fds = [PollFd::new(borrowed, PollFlags::POLLOUT)];
    poll(&mut fds, PollTimeout::NONE).map_err(|err| format!("poll writable pty fd: {err}"))?;
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
fn child_exited(child_pid: Pid) -> Result<Option<i32>, String> {
    match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::StillAlive) => Ok(None),
        Ok(_) => Ok(Some(1)),
        Err(nix::errno::Errno::ECHILD) => Ok(None),
        Err(err) => Err(format!("waitpid failed: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::{Arc, Mutex, OnceLock},
    };

    use super::{
        apply_attach_state, attach_init_capabilities, default_vt_engine, is_executable_file, record_pty_output, resolve_cleat_executable,
        AttachCleanupGuard, TestReplayProbeVtEngine,
    };
    use crate::vt::{self, VtEngine};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn cleanup_only_writes_when_explicitly_emitted() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let guard = AttachCleanupGuard::test_buffer(Arc::clone(&output));

        drop(guard);

        assert!(output.lock().expect("lock output").is_empty());
    }

    #[test]
    fn cleanup_writes_fixed_reset_sequence_when_emitted() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let guard = AttachCleanupGuard::test_buffer(Arc::clone(&output));

        guard.emit().expect("emit cleanup");

        assert_eq!(
            *output.lock().expect("lock output"),
            b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[?1049l\x1b[<u\x1b[?25h"
        );
    }

    #[test]
    fn cleanup_does_not_write_when_target_is_disabled() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let guard = AttachCleanupGuard::test_buffer_disabled(Arc::clone(&output));

        guard.emit().expect("emit cleanup");

        assert!(output.lock().expect("lock output").is_empty());
    }

    #[test]
    fn graceful_socket_shutdown_classifies_broken_pipe_disconnects() {
        let err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        assert!(super::is_graceful_socket_shutdown(&err));
    }

    #[test]
    fn default_vt_engine_starts_with_default_size() {
        let engine = default_vt_engine(vt::default_vt_engine_kind()).expect("create default vt engine");
        assert_eq!(engine.size(), (super::DEFAULT_TERMINAL_COLS, super::DEFAULT_TERMINAL_ROWS));
        #[cfg(feature = "ghostty-vt")]
        assert!(engine.supports_replay());
        #[cfg(not(feature = "ghostty-vt"))]
        assert!(!engine.supports_replay());
        #[cfg(feature = "ghostty-vt")]
        assert!(engine.replay_payload(&vt::ClientCapabilities::conservative_fallback()).expect("replay payload").is_some());
        #[cfg(not(feature = "ghostty-vt"))]
        assert_eq!(engine.replay_payload(&vt::ClientCapabilities::conservative_fallback()).expect("replay payload"), None);
    }

    #[test]
    fn vt_engine_helpers_feed_and_resize_default_engine() {
        let mut engine = default_vt_engine(vt::default_vt_engine_kind()).expect("create default vt engine");
        record_pty_output(engine.as_mut(), b"hello").expect("feed output");
        let replay =
            apply_attach_state(engine.as_mut(), 132, 40, &vt::ClientCapabilities::conservative_fallback()).expect("apply attach state");

        assert_eq!(engine.size(), (132, 40));
        #[cfg(feature = "ghostty-vt")]
        assert!(replay.is_some());
        #[cfg(not(feature = "ghostty-vt"))]
        assert_eq!(replay, None);
    }

    #[test]
    fn lifecycle_attach_init_capabilities_use_conservative_terminal_assumptions() {
        assert_eq!(attach_init_capabilities(), vt::ClientCapabilities::conservative_fallback());
    }

    #[test]
    fn lifecycle_apply_attach_state_uses_attach_capabilities_for_replay() {
        let mut engine = TestReplayProbeVtEngine::new(80, 24);
        let capabilities = vt::ClientCapabilities::new(vt::ColorLevel::Ansi256, true);

        let replay = apply_attach_state(&mut engine, 100, 30, &capabilities).expect("apply attach state");

        assert_eq!(engine.size(), (100, 30));
        assert_eq!(replay, Some(b"Ansi256:true".to_vec()));
    }

    #[cfg(not(feature = "ghostty-vt"))]
    #[test]
    fn vt_engine_helpers_compile_without_ghostty_feature() {
        let mut engine = vt::make_default_vt_engine(80, 24);

        record_pty_output(engine.as_mut(), b"hello").expect("feed output");
        let replay =
            apply_attach_state(engine.as_mut(), 100, 30, &vt::ClientCapabilities::conservative_fallback()).expect("apply attach state");

        assert_eq!(engine.size(), (100, 30));
        assert_eq!(replay, None);
    }

    #[test]
    fn resolve_cleat_executable_prefers_cargo_bin_env() {
        let _lock = env_lock().lock().expect("env lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let cleat = temp.path().join("cleat");
        fs::write(&cleat, b"#!/bin/sh\n").expect("write fake cleat");
        let original = std::env::var_os("CARGO_BIN_EXE_cleat");
        std::env::set_var("CARGO_BIN_EXE_cleat", &cleat);

        let resolved = resolve_cleat_executable().expect("resolve cleat");

        match original {
            Some(value) => std::env::set_var("CARGO_BIN_EXE_cleat", value),
            None => std::env::remove_var("CARGO_BIN_EXE_cleat"),
        }
        assert_eq!(resolved, cleat);
    }

    #[test]
    fn resolve_cleat_executable_falls_back_to_path() {
        let _lock = env_lock().lock().expect("env lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let bin_dir = temp.path().join("bin");
        fs::create_dir_all(&bin_dir).expect("create bin dir");
        let cleat = bin_dir.join("cleat");
        fs::write(&cleat, b"#!/bin/sh\n").expect("write fake cleat");
        let mut perms = fs::metadata(&cleat).expect("metadata").permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            perms.set_mode(0o755);
            fs::set_permissions(&cleat, perms).expect("set executable");
        }

        let original_bin = std::env::var_os("CARGO_BIN_EXE_cleat");
        let original_path = std::env::var_os("PATH");
        std::env::remove_var("CARGO_BIN_EXE_cleat");
        std::env::set_var("PATH", PathBuf::from(&bin_dir).into_os_string());

        let resolved = resolve_cleat_executable().expect("resolve from path");

        match original_bin {
            Some(value) => std::env::set_var("CARGO_BIN_EXE_cleat", value),
            None => std::env::remove_var("CARGO_BIN_EXE_cleat"),
        }
        match original_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
        assert_eq!(resolved, cleat);
        assert!(is_executable_file(&cleat));
    }
}
