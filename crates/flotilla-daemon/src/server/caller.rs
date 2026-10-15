use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::{fs, path::Path};

#[cfg(not(target_os = "macos"))]
use flotilla_protocol::CallerProcess;
use flotilla_protocol::{CallerCrew, CommandCaller, PrincipalRef};

#[derive(Clone, Copy)]
pub(super) struct PeerCredential {
    pid: Option<u32>,
    uid: u32,
}

pub(super) fn socket_peer_credential(stream: &tokio::net::UnixStream) -> Option<PeerCredential> {
    let credentials = stream.peer_cred().ok()?;
    Some(PeerCredential { pid: credentials.pid().and_then(|pid| u32::try_from(pid).ok()), uid: credentials.uid() })
}

pub(super) fn caller_from_peer(peer: Option<PeerCredential>, namespace: &str) -> Result<CommandCaller, String> {
    let principal_ref = PrincipalRef::implicit_for_namespace(namespace);
    let credentials = peer.ok_or("CallerIdentityUnavailable: socket peer credentials unavailable")?;
    #[cfg(target_os = "linux")]
    if credentials.pid.is_none() {
        return Err("CallerIdentityUnavailable: socket peer PID unavailable".into());
    }
    let process = credentials.pid.map(|pid| process_identity(pid, credentials.uid)).transpose()?;
    Ok(CommandCaller { principal_ref, process, crew: None })
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
fn process_identity(pid: u32, uid: u32) -> Result<CallerProcess, String> {
    let proc = Path::new("/proc").join(pid.to_string());
    let executable = fs::read_link(proc.join("exe")).ok().map(|path| path.to_string_lossy().into_owned());
    let argv = fs::read(proc.join("cmdline")).ok().map(|bytes| split_nul(&bytes)).unwrap_or_default();
    let contents =
        fs::read_to_string(proc.join("cgroup")).map_err(|error| format!("CallerIdentityUnavailable: cannot read peer cgroup: {error}"))?;
    let container = container_from_cgroup(&contents);
    Ok(CallerProcess::builder().pid(pid).uid(uid).maybe_executable(executable).argv(argv).maybe_container(container).build())
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

pub(super) fn claims_from_peer(peer: Option<PeerCredential>) -> HashMap<String, String> {
    peer.and_then(|peer| peer.pid).and_then(read_environ).unwrap_or_default()
}

/// Claims can only narrow identities already established by placement.
pub(super) fn check_claims(crews: Vec<CallerCrew>, claims: &HashMap<String, String>) -> Result<CallerCrew, String> {
    if crews.is_empty() {
        return Err("EnvironmentIdentityUnavailable: no placed crew".into());
    }
    let mut matching = crews.into_iter().filter(|crew| {
        [
            ("FLOTILLA_NAMESPACE", Some(crew.namespace.as_str())),
            ("FLOTILLA_CONVOY", Some(crew.convoy.as_str())),
            ("FLOTILLA_VESSEL", Some(crew.vessel.as_str())),
            ("FLOTILLA_CREW_ROLE", Some(crew.role.as_str())),
            ("FLOTILLA_CREW_ID", Some(crew.crew_id.as_str())),
            ("FLOTILLA_TERMINAL_SESSION", crew.terminal_session.as_deref()),
        ]
        .into_iter()
        .all(|(key, actual)| claims.get(key).is_none_or(|claim| Some(claim.as_str()) == actual))
    });
    let crew = matching.next().ok_or("CallerIdentityMismatch: claims disagree with placed crew identity")?;
    if matching.next().is_some() {
        return Err("CallerIdentityAmbiguous: multiple placed crew sessions".into());
    }
    Ok(crew)
}

#[derive(Debug)]
pub(super) struct CallerResolution {
    pub caller: CommandCaller,
    pub transport_bound: bool,
}

fn legacy_crew_identity(environment: &HashMap<String, String>) -> Option<CallerCrew> {
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

/// Read kernel mount evidence independently of controller adoption readiness.
#[cfg(target_os = "linux")]
pub(super) fn mountinfo(pid: u32) -> Result<String, String> {
    fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("mountinfo"))
        .map_err(|error| format!("CallerIdentityUnavailable: cannot read peer mounts: {error}"))
}

#[cfg(not(target_os = "linux"))]
pub(super) fn mountinfo(_pid: u32) -> Result<String, String> {
    Err("CallerIdentityUnavailable: container mount evidence unavailable".into())
}

#[derive(Debug)]
pub(super) struct PeerMounts {
    pub mountinfo: String,
    pub legacy_main_directory: bool,
}

#[cfg(target_os = "linux")]
fn same_directory(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.is_dir() && right.is_dir() && left.dev() == right.dev() && left.ino() == right.ino()
}

pub(super) fn peer_mounts(pid: u32, socket_path: Option<&std::path::Path>) -> Result<PeerMounts, String> {
    let mountinfo = mountinfo(pid)?;
    #[cfg(target_os = "linux")]
    let legacy_main_directory = match socket_path.and_then(std::path::Path::parent) {
        Some(directory) => {
            let directory = if directory.as_os_str().is_empty() { Path::new(".") } else { directory };
            let peer_directory = Path::new("/proc")
                .join(pid.to_string())
                .join("root")
                .join(flotilla_environment::environment::CONTAINED_DAEMON_SOCKET_DIRECTORY.trim_start_matches('/'));
            let peer =
                fs::metadata(peer_directory).map_err(|e| format!("CallerIdentityUnavailable: cannot identify peer socket mount: {e}"))?;
            let host =
                fs::metadata(directory).map_err(|e| format!("CallerIdentityUnavailable: cannot identify main socket directory: {e}"))?;
            same_directory(&peer, &host)
        }
        None => false,
    };
    #[cfg(not(target_os = "linux"))]
    let legacy_main_directory = {
        let _ = socket_path;
        false
    };
    Ok(PeerMounts { mountinfo, legacy_main_directory })
}

fn environment_socket_is_mounted(environment: &flotilla_protocol::EnvironmentId, mounts: &str) -> Result<bool, String> {
    if mounts.trim().is_empty() {
        return Err("CallerIdentityUnavailable: empty peer mount table".into());
    }
    let directory = super::environment_sockets::directory_name(environment);
    for line in mounts.lines() {
        let (fields, _) = line.split_once(" - ").ok_or("CallerIdentityUnavailable: malformed peer mount table")?;
        let fields: Vec<_> = fields.split_whitespace().collect();
        if fields.len() < 6 {
            return Err("CallerIdentityUnavailable: malformed peer mount fields".into());
        }
        // Docker's immutable bind mount source is the daemon-created dedicated
        // directory. The main socket used the daemon run directory instead.
        if fields[4] == flotilla_environment::environment::CONTAINED_DAEMON_SOCKET_DIRECTORY
            && std::path::Path::new(fields[3]).file_name().and_then(|name| name.to_str()) == Some(directory.as_str())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) async fn resolve_main_caller(
    daemon: &flotilla_core::in_process::InProcessDaemon,
    mut caller: CommandCaller,
    claims: &HashMap<String, String>,
    mounts: Option<&PeerMounts>,
) -> Result<CallerResolution, String> {
    use flotilla_resources::Environment;
    use flotilla_store::StoreQuery;
    use std::collections::BTreeMap;
    let Some(container) = caller.process.as_ref().and_then(|process| process.container.as_ref()) else {
        // Host-direct has no isolation boundary and is explicitly out of scope.
        // Preserve its existing variable-derived identity, without a new override.
        caller.crew = legacy_crew_identity(claims);
        return Ok(CallerResolution { caller, transport_bound: false });
    };
    let mounts = mounts.ok_or("CallerIdentityUnavailable: peer mount evidence unavailable")?;
    if mounts.legacy_main_directory {
        // REMOVE this compatibility branch once no pre-#3270 vessels remain.
        // A kernel device/inode comparison proves the peer still mounts this
        // daemon's main run directory, not an environment socket directory.
        // It works before adoption and without a prior immutable-ID observation.
        tracing::info!(%container, "using pre-environment-socket vessel identity compatibility");
        caller.crew = legacy_crew_identity(claims);
        return Ok(CallerResolution { caller, transport_bound: false });
    }
    let namespace = daemon.provisioning_namespace().await;
    let environments = daemon
        .resource_backend()
        .query_declared::<Environment>(
            &namespace,
            &StoreQuery {
                fields: BTreeMap::from([("status.runtime_observation.container_id".into(), container.clone())]),
                ..Default::default()
            },
        )
        .await
        .map_err(|error| format!("CallerIdentityUnavailable: {error}"))?;
    // Host-direct records cannot establish container authority, even if an
    // obsolete observation still carries a Docker container ID.
    let mut environments = environments.items.into_iter().filter(|item| item.object.spec.docker.is_some());
    let environment = environments.next().ok_or("CallerIdentityUnavailable: container has no placed environment")?;
    if environments.next().is_some() {
        return Err("CallerIdentityAmbiguous: container maps to multiple environments".into());
    }
    let environment = flotilla_protocol::EnvironmentId::new(environment.object.metadata.name);
    if !environment_socket_is_mounted(&environment, &mounts.mountinfo)? {
        return Err("CallerIdentityMismatch: container has no matching environment socket mount".into());
    }
    let crews = super::environment_connection::environment_crews(daemon, &environment).await?;
    caller.crew = Some(check_claims(crews, claims)?);
    Ok(CallerResolution { caller, transport_bound: true })
}

#[cfg(target_os = "linux")]
fn container_from_cgroup(contents: &str) -> Option<String> {
    contents.lines().flat_map(|line| line.split('/').skip(1)).find_map(|component| {
        let without_scope = component.strip_suffix(".scope").unwrap_or(component);
        let candidate = without_scope
            .strip_prefix("docker-")
            .or_else(|| without_scope.strip_prefix("cri-containerd-"))
            .or_else(|| without_scope.strip_prefix("libpod-"))
            .unwrap_or(without_scope);
        (candidate.len() == 64 && candidate.bytes().all(|byte| byte.is_ascii_hexdigit())).then(|| candidate.to_ascii_lowercase())
    })
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::HashMap;

    use flotilla_protocol::CallerProcess;
    use tracing::debug;

    pub(super) fn process_identity(pid: u32, uid: u32) -> Result<CallerProcess, String> {
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
        Ok(CallerProcess::builder().pid(pid).uid(uid).maybe_executable(executable).argv(argv).build())
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
fn process_identity(pid: u32, uid: u32) -> Result<CallerProcess, String> {
    Ok(CallerProcess::builder().pid(pid).uid(uid).argv(Vec::new()).build())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_environ(_pid: u32) -> Option<HashMap<String, String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every field independently rejects forged and empty claims; absence and
    // matching claims accept exactly one placed session, never duplicates.
    #[hegel::test]
    fn claims_only_narrow_placed_identity(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let crew = CallerCrew::builder()
            .namespace("ns".into())
            .convoy("convoy".into())
            .vessel("vessel".into())
            .role("coder".into())
            .crew_id("crew".into())
            .terminal_session("session".into())
            .build();
        for (key, value) in [
            ("FLOTILLA_NAMESPACE", "ns"),
            ("FLOTILLA_CONVOY", "convoy"),
            ("FLOTILLA_VESSEL", "vessel"),
            ("FLOTILLA_CREW_ROLE", "coder"),
            ("FLOTILLA_CREW_ID", "crew"),
            ("FLOTILLA_TERMINAL_SESSION", "session"),
        ] {
            for claim in [None, Some(value), Some("forged"), Some("")] {
                let claims = claim.map(|value| HashMap::from([(key.into(), value.into())])).unwrap_or_default();
                assert_eq!(
                    check_claims(vec![crew.clone(); count], &claims).is_ok(),
                    count == 1 && (claim.is_none() || claim == Some(value))
                );
            }
        }
    }

    // Device/inode identity, rather than a directory name or an agent claim,
    // proves the actual legacy main-directory mount. Symlink aliases preserve
    // identity; a distinct socket directory and non-directory never match.
    #[cfg(target_os = "linux")]
    #[test]
    fn main_mount_comparison_requires_directory_identity() {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("run");
        let bound = root.path().join("environment");
        let alias = root.path().join("alias");
        fs::create_dir(&main).unwrap();
        fs::create_dir(&bound).unwrap();
        std::os::unix::fs::symlink(&main, &alias).unwrap();
        fs::write(root.path().join("file"), "not a directory").unwrap();
        let metadata = fs::metadata(&main).unwrap();
        assert!(same_directory(&metadata, &fs::metadata(alias).unwrap()));
        assert!(!same_directory(&metadata, &fs::metadata(bound).unwrap()));
        assert!(!same_directory(&metadata, &fs::metadata(root.path().join("file")).unwrap()));
    }

    // Unavailable kernel evidence must refuse, never downgrade a container to host.
    #[cfg(target_os = "linux")]
    #[test]
    fn missing_peer_cgroup_refuses_identity() {
        assert!(caller_from_peer(Some(PeerCredential { pid: Some(u32::MAX), uid: 0 }), "ns")
            .unwrap_err()
            .starts_with("CallerIdentityUnavailable:"));
        assert!(caller_from_peer(None, "ns").is_err());
        assert!(mountinfo(u32::MAX).unwrap_err().starts_with("CallerIdentityUnavailable:"));
    }

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
        for path in [
            format!("/system.slice/docker-{id}.scope"),
            format!("/docker/{id}/nested"),
            format!("/kubepods/cri-containerd-{id}.scope/child"),
            format!("/libpod-{id}.scope"),
        ] {
            assert_eq!(container_from_cgroup(&format!("0::{path}\n")), Some(id.to_string()));
        }
        assert_eq!(container_from_cgroup(&format!("0::/docker/{}\n", id.to_ascii_uppercase())), Some(id.into()));
        assert_eq!(container_from_cgroup("0::/user.slice/host.scope\n"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn child_socket_peer_exposes_claims_without_crew_authority() {
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
            .env("FLOTILLA_TERMINAL_SESSION", "test-terminal")
            .spawn()
            .expect("spawn crew child");
        let (stream, _) = listener.accept().expect("accept crew child");
        thread::sleep(Duration::from_millis(650));
        assert!(child.try_wait().expect("check crew child").is_none(), "crew child must remain alive until identity is read");
        stream.set_nonblocking(true).expect("set nonblocking");
        let stream = tokio::net::UnixStream::from_std(stream).expect("tokio stream");
        let peer = socket_peer_credential(&stream).expect("peer credentials");
        assert_eq!(peer.pid, Some(child.id()));
        let caller = caller_from_peer(Some(peer), "test-namespace").expect("peer identity");
        let process = caller.process.expect("peer process identity");
        assert_eq!(process.pid, child.id());
        assert!(process.executable.is_some());
        assert!(!process.argv.is_empty());
        assert!(caller.crew.is_none(), "variables never create crew identity");
        let claims = claims_from_peer(socket_peer_credential(&stream));
        let crew = check_claims(
            vec![CallerCrew::builder()
                .namespace("test-namespace".into())
                .convoy("test-convoy".into())
                .vessel("test-vessel".into())
                .role("coder".into())
                .crew_id("test-crew".into())
                .terminal_session("test-terminal".into())
                .build()],
            &claims,
        )
        .expect("matching claims");
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

        let caller = caller_from_peer(socket_peer_credential(&stream), "flotilla").expect("peer identity");
        let process = caller.process.expect("peer process identity");
        assert_eq!(process.pid, std::process::id());
        assert_eq!(process.uid, unsafe { libc::geteuid() });
        assert!(process.executable.is_some());
        assert!(!process.argv.is_empty());
    }
}
