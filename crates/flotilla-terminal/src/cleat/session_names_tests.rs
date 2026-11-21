use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_protocol::commands::AttachMode;

use super::CleatTerminalPool;
use crate::{TerminalPool, TerminalSessionLiveness};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner};

/// An engine launch's terminal session name, as `TerminalSessionIdentity::name` spells it.
const LAUNCH_SESSION: &str = "terminal-convoy-2d327f85c7a64b039d4a2734c7900e86-work#52-coder";

/// Cleat's `validate_runtime_name` (cleat crates/cleat/src/runtime.rs).
fn cleat_accepts(name: &str) -> Result<(), String> {
    if !name.is_empty()
        && name != "."
        && name != ".."
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        Ok(())
    } else {
        Err(format!("invalid filesystem-safe name: {name}"))
    }
}

/// Process boundary: a cleat that holds sessions by id and refuses, as cleat does, any id that is
/// not a filesystem-safe name, in every operation that names a session.
#[derive(Default)]
struct NameRuleCleat {
    sessions: Mutex<BTreeSet<String>>,
    named: Mutex<Vec<String>>,
}

impl NameRuleCleat {
    fn operate(&self, args: &[&str]) -> Result<String, String> {
        // The controlled client runs `/usr/bin/env -i NAME=VALUE... cleat ARGS`.
        let start = args.iter().position(|arg| *arg == "cleat").expect("cleat invocation") + 1;
        let args = &args[start..];
        let named = |id: &str| -> Result<String, String> {
            cleat_accepts(id)?;
            self.named.lock().expect("named").push(id.to_string());
            Ok(id.to_string())
        };
        let known = |id: &str| -> Result<(), String> {
            let id = named(id)?;
            if self.sessions.lock().expect("sessions").contains(&id) {
                Ok(())
            } else {
                Err(format!("no session {id}"))
            }
        };
        match args {
            ["daemons", "--json"] => Ok(r#"[{"name":"default@1","runtime_root":"/state/cleat","alive":true}]"#.into()),
            ["list", "--json"] | ["--runtime-root", _, "--server", _, "list", "--json"] => {
                let sessions = self.sessions.lock().expect("sessions");
                let rows = sessions.iter().map(|id| serde_json::json!({"id": id, "status": "Detached"})).collect::<Vec<_>>();
                Ok(serde_json::Value::Array(rows).to_string())
            }
            ["launch", rest @ ..] => {
                let index = rest.iter().position(|arg| *arg == "--record").expect("launch records its session");
                let id = named(rest[index + 1])?;
                self.sessions.lock().expect("sessions").insert(id);
                Ok("{}".into())
            }
            ["send", rest @ ..] => {
                let index = rest.iter().position(|arg| *arg == "--").expect("send ends its options");
                known(rest[index + 1])?;
                Ok(String::new())
            }
            ["capture", id] => known(id).map(|()| "screen".into()),
            ["kill", id] => {
                known(id)?;
                self.sessions.lock().expect("sessions").remove(*id);
                Ok(String::new())
            }
            _ => Err(format!("unexpected cleat operation {args:?}")),
        }
    }
}

#[async_trait]
impl CommandRunner for NameRuleCleat {
    async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        assert_eq!(cmd, "/usr/bin/env");
        self.operate(args)
    }

    async fn run_output(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        assert_eq!(cmd, "/usr/bin/env");
        Ok(match self.operate(args) {
            Ok(stdout) => CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) },
            Err(stderr) => CommandOutput { stdout: String::new(), stderr, exit_code: Some(3) },
        })
    }

    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        false
    }
}

// A session named for an engine launch carries `#`, which cleat refuses. The pool names it to
// cleat by its filesystem-safe encoding in every operation and lists it under its own name.
#[tokio::test]
async fn a_launch_named_session_goes_through_every_operation_under_cleats_name_rule() {
    assert!(cleat_accepts(LAUNCH_SESSION).is_err(), "the rule refuses the raw name");
    let cleat = Arc::new(NameRuleCleat::default());
    let pool = CleatTerminalPool::new(Arc::clone(&cleat) as Arc<dyn CommandRunner>, "cleat", &EnvironmentBag::new());
    pool.launch_capability.set(super::LaunchCapability { env_clear: true, argv: true }).expect("known launch capability");
    pool.send_key_capability.set(false).expect("known send capability");

    pool.ensure_session(LAUNCH_SESSION, "claude", &ExecutionEnvironmentPath::new("/repo"), &Vec::new(), &[]).await.expect("launch");
    let listed = pool.list_sessions().await.expect("list");
    assert_eq!(listed.iter().map(|session| session.session_name.as_str()).collect::<Vec<_>>(), [LAUNCH_SESSION]);
    // A second ensure finds the session it launched rather than launching another.
    pool.ensure_session(LAUNCH_SESSION, "claude", &ExecutionEnvironmentPath::new("/repo"), &Vec::new(), &[]).await.expect("relaunch");
    assert_eq!(pool.list_sessions().await.expect("list").len(), 1);

    assert!(matches!(pool.session_liveness(LAUNCH_SESSION).await.expect("liveness"), TerminalSessionLiveness::Running));
    let endpoint = pool.cleat_endpoint(LAUNCH_SESSION).await.expect("endpoint").expect("one daemon holds the session");
    cleat_accepts(&endpoint.session).expect("the endpoint names cleat's id");

    pool.operator_deliver(LAUNCH_SESSION, "hello").await.expect("send");
    pool.type_without_submit(LAUNCH_SESSION, "draft").await.expect("paste");
    pool.guarded_deliver(LAUNCH_SESSION, "turn", "key-1").await.expect("guarded send");
    assert_eq!(pool.capture_screen(LAUNCH_SESSION).await.expect("capture").as_deref(), Some("screen"));

    let attach = pool
        .attach_args_for_mode(LAUNCH_SESSION, Some("claude"), &ExecutionEnvironmentPath::new("/repo"), &Vec::new(), AttachMode::Take)
        .expect("attach args");
    let flat = flotilla_protocol::arg::flatten(&attach, 0);
    let attached = flat.rsplit(' ').next().expect("attach names its session");
    assert_eq!(attached, endpoint.session, "attach names the session cleat holds: {flat}");

    pool.kill_session(LAUNCH_SESSION).await.expect("kill");
    assert!(matches!(pool.session_liveness(LAUNCH_SESSION).await.expect("liveness"), TerminalSessionLiveness::Absent));
    let named = cleat.named.lock().expect("named").clone();
    assert!(named.len() >= 6 && named.iter().all(|id| *id == endpoint.session), "{named:?}");
}

// Names that never needed encoding reach cleat as they are, so sessions launched before the
// encoding keep their ids.
#[tokio::test]
async fn a_plain_session_name_is_cleats_id_unchanged() {
    let cleat = Arc::new(NameRuleCleat::default());
    let pool = CleatTerminalPool::new(Arc::clone(&cleat) as Arc<dyn CommandRunner>, "cleat", &EnvironmentBag::new());
    pool.launch_capability.set(super::LaunchCapability { env_clear: true, argv: true }).expect("known launch capability");
    pool.ensure_session("terminal-conv-implement-coder", "claude", &ExecutionEnvironmentPath::new("/repo"), &Vec::new(), &[])
        .await
        .expect("launch");
    assert_eq!(cleat.sessions.lock().expect("sessions").iter().collect::<Vec<_>>(), ["terminal-conv-implement-coder"]);
}
