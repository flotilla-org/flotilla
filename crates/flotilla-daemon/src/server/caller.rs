use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::{fs, path::Path};

#[cfg(not(target_os = "macos"))]
use flotilla_protocol::CallerProcess;
use flotilla_protocol::{CallerCrew, CommandCaller, PrincipalRef};

pub(super) struct PeerCredential {
    pid: Option<u32>,
    uid: u32,
}

pub(super) fn socket_peer_credential(stream: &tokio::net::UnixStream) -> Option<PeerCredential> {
    let credentials = stream.peer_cred().ok()?;
    Some(PeerCredential { pid: credentials.pid().and_then(|pid| u32::try_from(pid).ok()), uid: credentials.uid() })
}

pub(super) fn caller_from_peer(peer: Option<PeerCredential>, namespace: &str) -> CommandCaller {
    let principal_ref = PrincipalRef::implicit_for_namespace(namespace);
    let process = peer.and_then(|credentials| credentials.pid.map(|pid| process_identity(pid, credentials.uid)));
    let crew = process.as_ref().and_then(|process| read_environ(process.pid)).and_then(|environment| crew_identity(&environment));
    CommandCaller { principal_ref, process, crew }
}

pub(super) fn missing_crew_message(action: &str) -> String {
    missing_crew_message_for_os(action, std::env::consts::OS)
}

fn missing_crew_message_for_os(action: &str, os: &str) -> String {
    if matches!(os, "linux" | "macos") {
        format!("{action} requires a calling crew session")
    } else {
        format!("{action} requires a calling crew session: peer identity unavailable on {os}")
    }
}

#[cfg(target_os = "linux")]
fn process_identity(pid: u32, uid: u32) -> CallerProcess {
    let proc = Path::new("/proc").join(pid.to_string());
    let executable = fs::read_link(proc.join("exe")).ok().map(|path| path.to_string_lossy().into_owned());
    let argv = fs::read(proc.join("cmdline")).ok().map(|bytes| split_nul(&bytes)).unwrap_or_default();
    let container = fs::read_to_string(proc.join("cgroup")).ok().and_then(|contents| container_from_cgroup(&contents));
    CallerProcess::builder().pid(pid).uid(uid).maybe_executable(executable).argv(argv).maybe_container(container).build()
}

#[cfg(target_os = "linux")]
fn read_environ(pid: u32) -> Option<HashMap<String, String>> {
    let bytes = fs::read(Path::new("/proc").join(pid.to_string()).join("environ")).ok()?;
    Some(
        split_nul(&bytes)
            .into_iter()
            .filter_map(|entry| entry.split_once('=').map(|(key, value)| (key.to_string(), value.to_string())))
            .collect(),
    )
}

#[cfg(target_os = "linux")]
fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes.split(|byte| *byte == 0).filter(|part| !part.is_empty()).map(|part| String::from_utf8_lossy(part).into_owned()).collect()
}

fn crew_identity(environment: &HashMap<String, String>) -> Option<CallerCrew> {
    Some(
        CallerCrew::builder()
            .namespace(environment.get("FLOTILLA_NAMESPACE")?.clone())
            .convoy(environment.get("FLOTILLA_CONVOY")?.clone())
            .vessel(environment.get("FLOTILLA_VESSEL")?.clone())
            .role(environment.get("FLOTILLA_CREW_ROLE")?.clone())
            .crew_id(environment.get("FLOTILLA_CREW_ID")?.clone())
            .maybe_terminal_session(environment.get("FLOTILLA_TERMINAL_SESSION").cloned())
            .build(),
    )
}

#[cfg(target_os = "linux")]
fn container_from_cgroup(contents: &str) -> Option<String> {
    contents.lines().flat_map(|line| line.rsplit('/').next()).find_map(|component| {
        let without_scope = component.strip_suffix(".scope").unwrap_or(component);
        let candidate = without_scope
            .strip_prefix("docker-")
            .or_else(|| without_scope.strip_prefix("cri-containerd-"))
            .or_else(|| without_scope.strip_prefix("libpod-"))
            .unwrap_or(without_scope);
        (candidate.len() == 64 && candidate.bytes().all(|byte| byte.is_ascii_hexdigit())).then(|| candidate.to_string())
    })
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::HashMap;

    use flotilla_protocol::CallerProcess;
    use tracing::debug;

    pub(super) fn process_identity(pid: u32, uid: u32) -> CallerProcess {
        let mut path = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: proc_pidpath writes at most path.len() bytes to the valid buffer.
        let path_len = unsafe { libc::proc_pidpath(pid as libc::c_int, path.as_mut_ptr().cast(), path.len() as u32) };
        let executable = if path_len > 0 {
            Some(String::from_utf8_lossy(path[..path_len as usize].split(|byte| *byte == 0).next().unwrap_or_default()).into_owned())
        } else {
            debug!(%pid, error = %std::io::Error::last_os_error(), "macOS peer executable path unavailable");
            None
        };
        let argv = proc_args(pid).map(|(argv, _)| argv).unwrap_or_default();
        CallerProcess::builder().pid(pid).uid(uid).maybe_executable(executable).argv(argv).build()
    }

    pub(super) fn read_environ(pid: u32) -> Option<HashMap<String, String>> {
        proc_args(pid).map(|(_, environment)| environment)
    }

    fn proc_args(pid: u32) -> Option<(Vec<String>, HashMap<String, String>)> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, libc::c_int::try_from(pid).ok()?];
        for attempt in 0..2 {
            let mut size = 0;
            // SAFETY: sysctl writes the required buffer size through the valid size pointer.
            if unsafe { libc::sysctl(mib.as_mut_ptr(), mib.len() as _, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) } != 0 {
                debug!(%pid, error = %std::io::Error::last_os_error(), "macOS peer argument size unavailable");
                return None;
            }
            let mut bytes = vec![0_u8; size];
            // SAFETY: bytes is initialized and sysctl receives its capacity and valid pointer.
            if unsafe { libc::sysctl(mib.as_mut_ptr(), mib.len() as _, bytes.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) } == 0
            {
                bytes.truncate(size);
                let parsed = parse_proc_args(&bytes);
                if parsed.is_none() {
                    debug!(%pid, "macOS peer arguments malformed");
                }
                return parsed;
            }
            let error = std::io::Error::last_os_error();
            if attempt == 0 && error.raw_os_error() == Some(libc::ENOMEM) {
                continue;
            }
            debug!(%pid, %error, "macOS peer arguments unavailable");
            return None;
        }
        None
    }

    fn parse_proc_args(bytes: &[u8]) -> Option<(Vec<String>, HashMap<String, String>)> {
        let argc = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
        if argc < 0 {
            return None;
        }
        let mut cursor = 4;
        read_nul(bytes, &mut cursor)?; // Executable path precedes argv.
        skip_nul(bytes, &mut cursor);
        let mut argv = Vec::new();
        for _ in 0..argc {
            argv.push(read_nul(bytes, &mut cursor)?);
        }
        skip_nul(bytes, &mut cursor);
        let mut environment = HashMap::new();
        while cursor < bytes.len() {
            let entry = read_nul(bytes, &mut cursor)?;
            if entry.is_empty() {
                break;
            }
            if let Some((key, value)) = entry.split_once('=') {
                environment.insert(key.to_string(), value.to_string());
            }
        }
        Some((argv, environment))
    }

    fn read_nul(bytes: &[u8], cursor: &mut usize) -> Option<String> {
        let end = bytes.get(*cursor..)?.iter().position(|byte| *byte == 0)? + *cursor;
        let value = String::from_utf8_lossy(&bytes[*cursor..end]).into_owned();
        *cursor = end + 1;
        Some(value)
    }

    fn skip_nul(bytes: &[u8], cursor: &mut usize) {
        while bytes.get(*cursor) == Some(&0) {
            *cursor += 1;
        }
    }
}

#[cfg(target_os = "macos")]
use macos::{process_identity, read_environ};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_identity(pid: u32, uid: u32) -> CallerProcess {
    CallerProcess::builder().pid(pid).uid(uid).argv(Vec::new()).build()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_environ(_pid: u32) -> Option<HashMap<String, String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_platform_refusal_explains_missing_peer_identity() {
        assert_eq!(
            missing_crew_message_for_os("artifact put", "freebsd"),
            "artifact put requires a calling crew session: peer identity unavailable on freebsd"
        );
        assert_eq!(
            missing_crew_message_for_os("artifact get", "freebsd"),
            "artifact get requires a calling crew session: peer identity unavailable on freebsd"
        );
        for os in ["linux", "macos"] {
            assert_eq!(missing_crew_message_for_os("artifact put", os), "artifact put requires a calling crew session");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn extracts_container_id_from_cgroup() {
        let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(container_from_cgroup(&format!("0::/system.slice/docker-{id}.scope\n")), Some(id.to_string()));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn child_socket_peer_exposes_crew_identity() {
        use std::{os::unix::net::UnixListener, process::Command, thread, time::Duration};

        let directory = flotilla_test_support::TestSocketDir::new();
        let path = directory.socket_path("peer.sock");
        let listener = UnixListener::bind(&path).expect("bind unix socket");
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", "server::caller::tests::connect_as_crew_child", "--nocapture"])
            .env("FLOTILLA_TEST_SOCKET", &path)
            .env("FLOTILLA_NAMESPACE", "test-namespace")
            .env("FLOTILLA_CONVOY", "test-convoy")
            .env("FLOTILLA_VESSEL", "test-vessel")
            .env("FLOTILLA_CREW_ROLE", "coder")
            .env("FLOTILLA_CREW_ID", "test-crew")
            .spawn()
            .expect("spawn crew child");
        let (stream, _) = listener.accept().expect("accept crew child");
        thread::sleep(Duration::from_millis(650));
        assert!(child.try_wait().expect("check crew child").is_none(), "crew child must remain alive until identity is read");
        stream.set_nonblocking(true).expect("set nonblocking");
        let stream = tokio::net::UnixStream::from_std(stream).expect("tokio stream");
        let peer = socket_peer_credential(&stream).expect("peer credentials");
        assert_eq!(peer.pid, Some(child.id()));
        let caller = caller_from_peer(Some(peer), "test-namespace");
        let process = caller.process.expect("peer process identity");
        assert_eq!(process.pid, child.id());
        assert!(process.executable.is_some());
        assert!(!process.argv.is_empty());
        let crew = caller.crew.expect("crew identity from child environment");
        assert_eq!(crew.namespace, "test-namespace");
        assert_eq!(crew.convoy, "test-convoy");
        assert_eq!(crew.vessel, "test-vessel");
        assert_eq!(crew.role, "coder");
        assert_eq!(crew.crew_id, "test-crew");
        drop(stream);
        assert!(child.wait().expect("wait for crew child").success());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn connect_as_crew_child() {
        use std::{io::Read, os::unix::net::UnixStream};

        let Ok(path) = std::env::var("FLOTILLA_TEST_SOCKET") else { return };
        let mut stream = UnixStream::connect(path).expect("connect to test listener");
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).expect("wait for parent to finish"), 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn unix_socket_peer_credentials_identify_the_connecting_process() {
        let directory = flotilla_test_support::TestSocketDir::new();
        let path = directory.socket_path("peer.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind unix socket");
        let connect = tokio::net::UnixStream::connect(&path);
        let (accepted, connected) = tokio::join!(listener.accept(), connect);
        let (stream, _) = accepted.expect("accept unix peer");
        connected.expect("connect unix peer");

        let caller = caller_from_peer(socket_peer_credential(&stream), "flotilla");
        let process = caller.process.expect("peer process identity");
        assert_eq!(process.pid, std::process::id());
        assert_eq!(process.uid, unsafe { libc::geteuid() });
        assert!(process.executable.is_some());
        assert!(!process.argv.is_empty());
    }
}
