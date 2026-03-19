use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::{
    protocol::{Frame, SessionInfo, SessionStatus},
    runtime::{RuntimeLayout, SessionRecord},
    session::{attach_foreground, ensure_session_started, foreground_path, run_session_daemon, session_socket_path, ForegroundAttach},
    vt::VtEngineKind,
};

#[derive(Debug, Clone)]
pub struct SessionService {
    layout: RuntimeLayout,
}

impl SessionService {
    pub fn new(layout: RuntimeLayout) -> Self {
        Self { layout }
    }

    pub fn discover() -> Self {
        Self::new(RuntimeLayout::discover())
    }

    pub fn create(
        &self,
        name: Option<String>,
        vt_engine: Option<VtEngineKind>,
        cwd: Option<std::path::PathBuf>,
        cmd: Option<String>,
    ) -> Result<SessionInfo, String> {
        let session = ensure_session_started(&self.layout, name, vt_engine, cwd, cmd)?;
        Ok(SessionInfo {
            id: session.id,
            name: session.name,
            vt_engine: session.vt_engine,
            cwd: session.cwd,
            cmd: session.cmd,
            status: SessionStatus::Detached,
        })
    }

    pub fn list(&self) -> Result<Vec<SessionInfo>, String> {
        self.layout
            .list_sessions()
            .map(|sessions| sessions.into_iter().map(|record| session_info_from_record(self.layout.root(), record)).collect())
    }

    pub fn kill(&self, id: &str) -> Result<(), String> {
        if !self.layout.root().join(id).join("meta.json").exists() {
            return Err(format!("missing session {id}"));
        }
        let pid_path = crate::session::daemon_pid_path(self.layout.root(), id);
        if let Ok(Some(pid)) = std::fs::read_to_string(&pid_path).map(|value| value.trim().parse::<i32>().ok()) {
            if is_expected_bollard_process(pid) {
                // SAFETY: the pid was verified to belong to a cleat process before signaling it.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        }
        self.layout.remove_session(id)
    }

    pub fn detach(&self, id: &str) -> Result<(), String> {
        if !self.layout.root().join(id).join("meta.json").exists() {
            return Err(format!("missing session {id}"));
        }

        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream =
            std::os::unix::net::UnixStream::connect(&socket_path).map_err(|err| format!("connect {}: {err}", socket_path.display()))?;
        Frame::Detach.write(&mut stream).map_err(|err| format!("write detach request: {err}"))?;
        Ok(())
    }

    pub fn capture(&self, id: &str) -> Result<String, String> {
        if !self.layout.root().join(id).join("meta.json").exists() {
            return Err(format!("missing session {id}"));
        }

        let socket_path = session_socket_path(self.layout.root(), id);
        let mut stream =
            std::os::unix::net::UnixStream::connect(&socket_path).map_err(|err| format!("connect {}: {err}", socket_path.display()))?;
        Frame::Capture.write(&mut stream).map_err(|err| format!("write capture request: {err}"))?;
        match Frame::read(&mut stream).map_err(|err| format!("read capture response: {err}"))? {
            Frame::Output(bytes) => String::from_utf8(bytes).map_err(|err| format!("capture response was not valid utf-8: {err}")),
            Frame::Error(message) => Err(message),
            other => Err(format!("unexpected capture response: {other:?}")),
        }
    }

    pub fn attach(
        &self,
        name: Option<String>,
        vt_engine: Option<VtEngineKind>,
        cwd: Option<std::path::PathBuf>,
        cmd: Option<String>,
        no_create: bool,
    ) -> Result<(SessionInfo, ForegroundAttach), String> {
        let session = if no_create {
            let id = name.ok_or_else(|| "attach --no-create requires a session id".to_string())?;
            self.layout
                .list_sessions()?
                .into_iter()
                .find(|record| record.metadata.id == id)
                .map(|record| record.metadata)
                .ok_or_else(|| format!("missing session {id}"))?
        } else {
            ensure_session_started(&self.layout, name, vt_engine, cwd, cmd)?
        };
        let attach = attach_foreground(&self.layout, &session.id)?;
        Ok((
            SessionInfo {
                id: session.id,
                name: session.name,
                vt_engine: session.vt_engine,
                cwd: session.cwd,
                cmd: session.cmd,
                status: SessionStatus::Attached,
            },
            attach,
        ))
    }

    pub fn serve(&self, id: &str) -> Result<(), String> {
        run_session_daemon(self.layout.root(), id)
    }
}

fn session_info_from_record(root: &std::path::Path, record: SessionRecord) -> SessionInfo {
    let id = record.metadata.id.clone();
    let status = if foreground_path(root, &id).exists() { SessionStatus::Attached } else { SessionStatus::Detached };
    SessionInfo {
        id: record.metadata.id,
        name: record.metadata.name,
        vt_engine: record.metadata.vt_engine,
        cwd: record.metadata.cwd,
        cmd: record.metadata.cmd,
        status,
    }
}

fn is_expected_bollard_process(pid: i32) -> bool {
    let mut sys = System::new();
    let sysinfo_pid = Pid::from(pid as usize);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[sysinfo_pid]), true, ProcessRefreshKind::nothing());
    sys.process(sysinfo_pid).map(|process| process.name().to_string_lossy().contains("cleat")).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command, thread, time::Duration};

    use super::SessionService;
    use crate::{runtime::RuntimeLayout, session::daemon_pid_path};

    #[test]
    fn kill_does_not_signal_unrelated_process_from_stale_pid_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let service = SessionService::new(RuntimeLayout::new(temp.path().to_path_buf()));
        let session_dir = temp.path().join("alpha");
        fs::create_dir_all(&session_dir).expect("create session dir");
        fs::write(session_dir.join("meta.json"), r#"{"id":"alpha","name":"alpha","cwd":null,"cmd":null}"#).expect("write metadata");

        let mut child = Command::new("sleep").arg("30").spawn().expect("spawn sleep");
        fs::write(daemon_pid_path(temp.path(), "alpha"), child.id().to_string()).expect("write pid");

        service.kill("alpha").expect("kill session");

        thread::sleep(Duration::from_millis(50));
        assert!(child.try_wait().expect("try_wait").is_none(), "unrelated process should still be alive");

        let _ = child.kill();
        let _ = child.wait();
    }
}
