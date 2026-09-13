use std::{collections::HashMap, fs, path::Path};

use flotilla_protocol::{CallerCrew, CallerProcess, CommandCaller, PrincipalRef};

pub(super) fn socket_caller(stream: &tokio::net::UnixStream, namespace: &str) -> CommandCaller {
    let principal_ref = PrincipalRef::implicit_for_namespace(namespace);
    let process = stream.peer_cred().ok().and_then(|credentials| {
        credentials.pid().and_then(|pid| u32::try_from(pid).ok()).map(|pid| process_identity(pid, credentials.uid()))
    });
    let crew = process.as_ref().and_then(|process| read_environ(process.pid)).and_then(|environment| crew_identity(&environment));
    CommandCaller { principal_ref, process, crew }
}

fn process_identity(pid: u32, uid: u32) -> CallerProcess {
    let proc = Path::new("/proc").join(pid.to_string());
    let executable = fs::read_link(proc.join("exe")).ok().map(|path| path.to_string_lossy().into_owned());
    let argv = fs::read(proc.join("cmdline")).ok().map(|bytes| split_nul(&bytes)).unwrap_or_default();
    let container = fs::read_to_string(proc.join("cgroup")).ok().and_then(|contents| container_from_cgroup(&contents));
    CallerProcess::builder().pid(pid).uid(uid).maybe_executable(executable).argv(argv).maybe_container(container).build()
}

fn read_environ(pid: u32) -> Option<HashMap<String, String>> {
    let bytes = fs::read(Path::new("/proc").join(pid.to_string()).join("environ")).ok()?;
    Some(
        split_nul(&bytes)
            .into_iter()
            .filter_map(|entry| entry.split_once('=').map(|(key, value)| (key.to_string(), value.to_string())))
            .collect(),
    )
}

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

fn container_from_cgroup(contents: &str) -> Option<String> {
    contents
        .lines()
        .flat_map(|line| line.rsplit('/').next())
        .find(|component| component.len() >= 12 && component.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_container_id_from_cgroup() {
        assert_eq!(container_from_cgroup("0::/docker/0123456789abcdef\n"), Some("0123456789abcdef".to_string()));
    }

    #[tokio::test]
    async fn unix_socket_peer_credentials_identify_the_connecting_process() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("peer.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind unix socket");
        let connect = tokio::net::UnixStream::connect(&path);
        let (accepted, connected) = tokio::join!(listener.accept(), connect);
        let (stream, _) = accepted.expect("accept unix peer");
        connected.expect("connect unix peer");

        let caller = socket_caller(&stream, "flotilla");
        let process = caller.process.expect("peer process identity");
        assert_eq!(process.pid, std::process::id());
        assert_eq!(process.uid, unsafe { libc::geteuid() });
        assert!(process.executable.is_some());
        assert!(!process.argv.is_empty());
    }
}
