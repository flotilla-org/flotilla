use crate::{
    protocol::{SessionInfo, SessionStatus},
    runtime::{RuntimeLayout, SessionRecord},
    session::{attach_foreground, ensure_session_started, foreground_path, run_session_daemon, ForegroundAttach},
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

    pub fn create(&self, name: Option<String>, cwd: Option<std::path::PathBuf>, cmd: Option<String>) -> Result<SessionInfo, String> {
        let session = ensure_session_started(&self.layout, name, cwd, cmd)?;
        Ok(SessionInfo { id: session.id, name: session.name, cwd: session.cwd, cmd: session.cmd, status: SessionStatus::Detached })
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
            // SAFETY: pid was read from our session pid file and kill only sends a signal.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        self.layout.remove_session(id)
    }

    pub fn attach(
        &self,
        name: Option<String>,
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
            ensure_session_started(&self.layout, name, cwd, cmd)?
        };
        let attach = attach_foreground(&self.layout, &session.id)?;
        Ok((
            SessionInfo { id: session.id, name: session.name, cwd: session.cwd, cmd: session.cmd, status: SessionStatus::Attached },
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
    SessionInfo { id: record.metadata.id, name: record.metadata.name, cwd: record.metadata.cwd, cmd: record.metadata.cmd, status }
}
