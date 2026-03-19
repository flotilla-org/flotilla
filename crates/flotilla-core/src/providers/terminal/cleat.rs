use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::{ManagedTerminal, ManagedTerminalId, TerminalStatus};
use serde::Deserialize;

use super::{TerminalEnvVars, TerminalPool};
use crate::{
    attachable::{
        terminal_session_binding_ref, AttachableContent, AttachableId, AttachableStoreApi, BindingObjectKind, SharedAttachableStore,
        TerminalPurpose,
    },
    providers::{run, CommandRunner},
};

#[derive(Debug, Deserialize)]
struct SessionInfo {
    id: String,
    cwd: Option<std::path::PathBuf>,
    cmd: Option<String>,
    status: SessionStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
enum SessionStatus {
    Attached,
    Detached,
}

pub struct CleatTerminalPool {
    runner: Arc<dyn CommandRunner>,
    binary: String,
    attachable_store: SharedAttachableStore,
}

impl CleatTerminalPool {
    pub fn new(runner: Arc<dyn CommandRunner>, binary: impl Into<String>, attachable_store: SharedAttachableStore) -> Self {
        Self { runner, binary: binary.into(), attachable_store }
    }

    fn parse_list_output(json: &str) -> Result<Vec<SessionInfo>, String> {
        serde_json::from_str(json).map_err(|err| format!("parse session list: {err}"))
    }

    fn find_persisted_session_id(store: &dyn AttachableStoreApi, id: &ManagedTerminalId) -> Option<String> {
        let expected = TerminalPurpose { checkout: id.checkout.clone(), role: id.role.clone(), index: id.index };
        for binding in &store.registry().bindings {
            if binding.provider_category != "terminal_pool"
                || binding.provider_name != "cleat"
                || binding.object_kind != BindingObjectKind::Attachable
            {
                continue;
            }
            let attachable_id = AttachableId::new(binding.object_id.clone());
            let Some(attachable) = store.registry().attachables.get(&attachable_id) else {
                continue;
            };
            match &attachable.content {
                AttachableContent::Terminal(terminal) if terminal.purpose == expected => return Some(binding.external_ref.clone()),
                _ => {}
            }
        }
        None
    }

    fn persist_attachable(
        store: &mut dyn AttachableStoreApi,
        id: &ManagedTerminalId,
        session_id: &str,
        command: &str,
        cwd: &Path,
        status: TerminalStatus,
    ) -> bool {
        let host = flotilla_protocol::HostName::local();
        let checkout_path = cwd.to_path_buf();
        let set_checkout = flotilla_protocol::HostPath::new(host.clone(), checkout_path.clone());
        let (set_id, changed_set) = store.ensure_terminal_set_with_change(Some(host), Some(set_checkout));
        let (_, changed_attachable) = store.ensure_terminal_attachable_with_change(
            &set_id,
            "terminal_pool",
            "cleat",
            session_id,
            TerminalPurpose { checkout: id.checkout.clone(), role: id.role.clone(), index: id.index },
            command,
            checkout_path,
            status,
        );
        changed_set || changed_attachable
    }

    fn reconcile_listed_session(store: &mut dyn AttachableStoreApi, session: SessionInfo) -> Option<ManagedTerminal> {
        let attachable_id = store
            .lookup_binding("terminal_pool", "cleat", BindingObjectKind::Attachable, &session.id)
            .map(|id| AttachableId::new(id.to_string()))?;
        let attachable = store.registry().attachables.get(&attachable_id)?;
        let (set_id, purpose, persisted_command, persisted_working_directory) = match &attachable.content {
            AttachableContent::Terminal(terminal) => {
                (attachable.set_id.clone(), terminal.purpose.clone(), terminal.command.clone(), terminal.working_directory.clone())
            }
        };
        let command = session.cmd.unwrap_or(persisted_command);
        let working_directory = session.cwd.unwrap_or(persisted_working_directory);
        let status = match session.status {
            SessionStatus::Attached => TerminalStatus::Running,
            SessionStatus::Detached => TerminalStatus::Disconnected,
        };
        let (_, _) = store.ensure_terminal_attachable_with_change(
            &set_id,
            "terminal_pool",
            "cleat",
            &session.id,
            purpose.clone(),
            &command,
            working_directory.clone(),
            status.clone(),
        );
        Some(ManagedTerminal {
            id: ManagedTerminalId { checkout: purpose.checkout, role: purpose.role.clone(), index: purpose.index },
            role: purpose.role,
            command,
            working_directory,
            status,
            attachable_id: Some(attachable_id),
            attachable_set_id: Some(set_id),
        })
    }

    fn disconnected_known_terminals(
        store: &mut dyn AttachableStoreApi,
        observed_sessions: &std::collections::HashSet<String>,
    ) -> Vec<ManagedTerminal> {
        let mut terminals = Vec::new();
        for binding in &store.registry().bindings {
            if binding.provider_category != "terminal_pool"
                || binding.provider_name != "cleat"
                || binding.object_kind != BindingObjectKind::Attachable
                || observed_sessions.contains(&binding.external_ref)
            {
                continue;
            }
            let attachable_id = AttachableId::new(binding.object_id.clone());
            let Some(attachable) = store.registry().attachables.get(&attachable_id) else {
                continue;
            };
            let (set_id, purpose, command, working_directory) = match &attachable.content {
                AttachableContent::Terminal(terminal) => {
                    (attachable.set_id.clone(), terminal.purpose.clone(), terminal.command.clone(), terminal.working_directory.clone())
                }
            };
            terminals.push(ManagedTerminal {
                id: ManagedTerminalId { checkout: purpose.checkout, role: purpose.role.clone(), index: purpose.index },
                role: purpose.role,
                command,
                working_directory,
                status: TerminalStatus::Disconnected,
                attachable_id: Some(attachable_id),
                attachable_set_id: Some(set_id),
            });
        }
        terminals
    }
}

#[async_trait]
impl TerminalPool for CleatTerminalPool {
    async fn list_terminals(&self) -> Result<Vec<ManagedTerminal>, String> {
        let output = run!(self.runner, &self.binary, &["list", "--json"], Path::new("/"))?;
        let sessions = Self::parse_list_output(&output)?;
        let Ok(mut store) = self.attachable_store.lock() else {
            return Ok(vec![]);
        };
        let observed_sessions: std::collections::HashSet<String> = sessions.iter().map(|session| session.id.clone()).collect();
        let mut terminals: Vec<ManagedTerminal> =
            sessions.into_iter().filter_map(|session| Self::reconcile_listed_session(store.as_mut(), session)).collect();
        terminals.extend(Self::disconnected_known_terminals(store.as_mut(), &observed_sessions));
        let _ = store.save();
        Ok(terminals)
    }

    async fn ensure_running(&self, id: &ManagedTerminalId, command: &str, cwd: &Path) -> Result<(), String> {
        if let Ok(store) = self.attachable_store.lock() {
            if Self::find_persisted_session_id(store.as_ref(), id).is_some() {
                return Ok(());
            }
        }
        let requested_name = terminal_session_binding_ref(id);
        let output = run!(
            self.runner,
            &self.binary,
            &["create", "--json", &requested_name, "--cwd", &cwd.display().to_string(), "--cmd", command],
            Path::new("/")
        )?;
        let session: SessionInfo = serde_json::from_str(&output).map_err(|err| format!("parse create session output: {err}"))?;
        let Ok(mut store) = self.attachable_store.lock() else {
            return Ok(());
        };
        if Self::persist_attachable(store.as_mut(), id, &session.id, command, cwd, TerminalStatus::Disconnected) {
            let _ = store.save();
        }
        Ok(())
    }

    async fn attach_command(
        &self,
        id: &ManagedTerminalId,
        command: &str,
        cwd: &Path,
        env_vars: &TerminalEnvVars,
    ) -> Result<String, String> {
        fn sq(s: &str) -> String {
            format!("'{}'", s.replace('\'', "'\\''"))
        }

        let session_id = self
            .attachable_store
            .lock()
            .ok()
            .and_then(|store| Self::find_persisted_session_id(store.as_ref(), id))
            .unwrap_or_else(|| terminal_session_binding_ref(id));
        let mut parts = vec![sq(&self.binary), "attach".into(), sq(&session_id), "--cwd".into(), sq(&cwd.display().to_string())];
        if !command.is_empty() || !env_vars.is_empty() {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            let env_prefix = if env_vars.is_empty() {
                String::new()
            } else {
                let pairs: Vec<String> = env_vars.iter().map(|(k, v)| format!("{k}={}", sq(v))).collect();
                format!("env {} ", pairs.join(" "))
            };
            let wrapped = if command.is_empty() {
                format!("{env_prefix}{shell}")
            } else {
                format!("{env_prefix}{shell} -lc '{}'", command.replace('\'', "'\\''"))
            };
            parts.push("--cmd".into());
            parts.push(sq(&wrapped));
        }
        Ok(parts.join(" "))
    }

    async fn kill_terminal(&self, id: &ManagedTerminalId) -> Result<(), String> {
        let session_id = self
            .attachable_store
            .lock()
            .ok()
            .and_then(|store| Self::find_persisted_session_id(store.as_ref(), id))
            .unwrap_or_else(|| terminal_session_binding_ref(id));
        run!(self.runner, &self.binary, &["kill", &session_id], Path::new("/"))?;

        let Ok(mut store) = self.attachable_store.lock() else {
            return Ok(());
        };
        if store.remove_binding_object("terminal_pool", "cleat", BindingObjectKind::Attachable, &session_id) {
            let _ = store.save();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use async_trait::async_trait;

    use super::*;
    use crate::{
        attachable::shared_in_memory_attachable_store,
        providers::{ChannelLabel, CommandOutput, CommandRunner},
    };

    struct MockRunner {
        responses: std::sync::Mutex<Vec<Result<String, String>>>,
        calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    }

    impl MockRunner {
        fn new(responses: Vec<Result<String, String>>) -> Self {
            Self { responses: std::sync::Mutex::new(responses), calls: std::sync::Mutex::new(vec![]) }
        }
    }

    #[async_trait]
    impl CommandRunner for MockRunner {
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            self.calls.lock().expect("calls").push((cmd.into(), args.iter().map(|arg| (*arg).into()).collect()));
            self.responses.lock().expect("responses").remove(0)
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            self.run(cmd, args, cwd, label).await.map(|stdout| CommandOutput { stdout, stderr: String::new(), success: true })
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn attach_command_wraps_cli_with_name_and_cwd() {
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![])), "cleat", shared_in_memory_attachable_store());
        let id = ManagedTerminalId { checkout: "feat".into(), role: "shell".into(), index: 0 };

        let command = pool.attach_command(&id, "bash", Path::new("/repo"), &vec![]).await.expect("attach command");

        assert!(command.contains("'cleat' attach"));
        assert!(command.contains("'flotilla/feat/shell/0'"));
        assert!(command.contains("--cwd '/repo'"));
    }

    #[tokio::test]
    async fn list_terminals_maps_json_output() {
        let json = r#"[{"id":"session-123","name":"flotilla/feat/shell/0","cwd":"/repo","cmd":"bash","status":"Attached"}]"#;
        let store = shared_in_memory_attachable_store();
        {
            let mut store_guard = store.lock().expect("store");
            CleatTerminalPool::persist_attachable(
                store_guard.as_mut(),
                &ManagedTerminalId { checkout: "feat".into(), role: "shell".into(), index: 0 },
                "session-123",
                "bash",
                Path::new("/repo"),
                TerminalStatus::Disconnected,
            );
        }
        let pool = CleatTerminalPool::new(Arc::new(MockRunner::new(vec![Ok(json.into())])), "cleat", store);

        let terminals = pool.list_terminals().await.expect("list terminals");

        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].id.checkout, "feat");
        assert_eq!(terminals[0].working_directory, std::path::PathBuf::from("/repo"));
        assert_eq!(terminals[0].status, TerminalStatus::Running);
        assert!(terminals[0].attachable_id.is_some());
    }

    #[tokio::test]
    async fn kill_terminal_calls_cli() {
        let runner = Arc::new(MockRunner::new(vec![Ok(String::new())]));
        let store = shared_in_memory_attachable_store();
        {
            let mut store_guard = store.lock().expect("store");
            CleatTerminalPool::persist_attachable(
                store_guard.as_mut(),
                &ManagedTerminalId { checkout: "feat".into(), role: "shell".into(), index: 0 },
                "session-123",
                "bash",
                Path::new("/repo"),
                TerminalStatus::Disconnected,
            );
        }
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat", store.clone());
        let id = ManagedTerminalId { checkout: "feat".into(), role: "shell".into(), index: 0 };

        pool.kill_terminal(&id).await.expect("kill terminal");

        let calls = runner.calls.lock().expect("calls");
        assert_eq!(calls[0].0, "cleat");
        assert_eq!(calls[0].1, vec!["kill".to_string(), "session-123".to_string()]);
        drop(calls);

        let store = store.lock().expect("store");
        assert!(CleatTerminalPool::find_persisted_session_id(store.as_ref(), &id).is_none());
    }

    #[tokio::test]
    async fn ensure_running_persists_created_session_handle() {
        let json = r#"{ "id":"session-123", "name":"flotilla/feat/shell/0", "cwd":"/repo", "cmd":"bash", "status":"Detached" }"#;
        let store = shared_in_memory_attachable_store();
        let runner = Arc::new(MockRunner::new(vec![Ok(json.into())]));
        let pool = CleatTerminalPool::new(Arc::clone(&runner) as Arc<dyn CommandRunner>, "cleat", store.clone());
        let id = ManagedTerminalId { checkout: "feat".into(), role: "shell".into(), index: 0 };

        pool.ensure_running(&id, "bash", Path::new("/repo")).await.expect("ensure running");

        let store = store.lock().expect("store lock");
        assert!(CleatTerminalPool::find_persisted_session_id(store.as_ref(), &id).as_deref() == Some("session-123"));
    }
}
