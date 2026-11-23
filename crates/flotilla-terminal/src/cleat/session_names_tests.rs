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

/// Cleat's session-name rule (cleat crates/cleat/src/session_name.rs): any non-empty text
/// without NUL.
fn cleat_accepts(name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains('\0') {
        Err(format!("invalid session name: {name:?}"))
    } else {
        Ok(())
    }
}

/// Process boundary: a cleat that holds sessions by name and checks, as cleat does, every name an
/// operation gives it.
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

// A session named for an engine launch carries `#`. The pool names it to cleat as it is in every
// operation, and cleat lists it under that name.
#[tokio::test]
async fn a_launch_named_session_goes_through_every_operation_under_its_own_name() {
    let cleat = Arc::new(NameRuleCleat::default());
    let pool = CleatTerminalPool::new(Arc::clone(&cleat) as Arc<dyn CommandRunner>, "cleat", &EnvironmentBag::new());
    pool.launch_capability.set(super::LaunchCapability { env_clear: true, argv: true, any_name: true }).expect("known launch capability");
    pool.send_key_capability.set(false).expect("known send capability");

    pool.ensure_session(LAUNCH_SESSION, "claude", &ExecutionEnvironmentPath::new("/repo"), &Vec::new(), &[]).await.expect("launch");
    let listed = pool.list_sessions().await.expect("list");
    assert_eq!(listed.iter().map(|session| session.session_name.as_str()).collect::<Vec<_>>(), [LAUNCH_SESSION]);
    // A second ensure finds the session it launched rather than launching another.
    pool.ensure_session(LAUNCH_SESSION, "claude", &ExecutionEnvironmentPath::new("/repo"), &Vec::new(), &[]).await.expect("relaunch");
    assert_eq!(pool.list_sessions().await.expect("list").len(), 1);

    assert!(matches!(pool.session_liveness(LAUNCH_SESSION).await.expect("liveness"), TerminalSessionLiveness::Running));
    let endpoint = pool.cleat_endpoint(LAUNCH_SESSION).await.expect("endpoint").expect("one daemon holds the session");
    assert_eq!(endpoint.session, LAUNCH_SESSION, "the endpoint names the session as cleat holds it");

    pool.operator_deliver(LAUNCH_SESSION, "hello").await.expect("send");
    pool.type_without_submit(LAUNCH_SESSION, "draft").await.expect("paste");
    pool.guarded_deliver(LAUNCH_SESSION, "turn", "key-1").await.expect("guarded send");
    assert_eq!(pool.capture_screen(LAUNCH_SESSION).await.expect("capture").as_deref(), Some("screen"));

    let attach = pool
        .attach_args_for_mode(LAUNCH_SESSION, Some("claude"), &ExecutionEnvironmentPath::new("/repo"), &Vec::new(), AttachMode::Take)
        .expect("attach args");
    let flat = flotilla_protocol::arg::flatten(&attach, 0);
    // The shell must see the whole name: unquoted, `#` would start a comment.
    assert!(flat.ends_with(&format!(" '{LAUNCH_SESSION}'")), "attach names the session, quoted: {flat}");

    pool.kill_session(LAUNCH_SESSION).await.expect("kill");
    assert!(matches!(pool.session_liveness(LAUNCH_SESSION).await.expect("liveness"), TerminalSessionLiveness::Absent));
    let named = cleat.named.lock().expect("named").clone();
    assert!(named.len() >= 6 && named.iter().all(|id| id == LAUNCH_SESSION), "{named:?}");
}

// A plain name reaches cleat as it is too.
#[tokio::test]
async fn a_plain_session_name_is_cleats_id_unchanged() {
    let cleat = Arc::new(NameRuleCleat::default());
    let pool = CleatTerminalPool::new(Arc::clone(&cleat) as Arc<dyn CommandRunner>, "cleat", &EnvironmentBag::new());
    pool.launch_capability.set(super::LaunchCapability { env_clear: true, argv: true, any_name: true }).expect("known launch capability");
    pool.ensure_session("terminal-conv-implement-coder", "claude", &ExecutionEnvironmentPath::new("/repo"), &Vec::new(), &[])
        .await
        .expect("launch");
    assert_eq!(cleat.sessions.lock().expect("sessions").iter().collect::<Vec<_>>(), ["terminal-conv-implement-coder"]);
}

/// `LAUNCH_SESSION` as flotilla named it to cleat before #4374.
const ENCODED_LAUNCH_SESSION: &str = "terminal-convoy-2d327f85c7a64b039d4a2734c7900e86-work.2352-coder";

// A crew launched before #4374 runs under the old encoded id. Until it relaunches, the pool lists
// it under its name, does not launch a second one, and reaches it by the id cleat holds.
#[tokio::test]
async fn a_crew_launched_under_the_old_encoding_is_found_until_it_relaunches() {
    let cleat = Arc::new(NameRuleCleat::default());
    cleat.sessions.lock().expect("sessions").insert(ENCODED_LAUNCH_SESSION.to_string());
    let pool = CleatTerminalPool::new(Arc::clone(&cleat) as Arc<dyn CommandRunner>, "cleat", &EnvironmentBag::new());
    pool.launch_capability.set(super::LaunchCapability { env_clear: true, argv: true, any_name: true }).expect("known launch capability");
    pool.send_key_capability.set(false).expect("known send capability");

    let listed = pool.list_sessions().await.expect("list");
    assert_eq!(listed.iter().map(|session| session.session_name.as_str()).collect::<Vec<_>>(), [LAUNCH_SESSION]);
    pool.ensure_session(LAUNCH_SESSION, "claude", &ExecutionEnvironmentPath::new("/repo"), &Vec::new(), &[]).await.expect("ensure");
    assert_eq!(cleat.sessions.lock().expect("sessions").len(), 1, "no second crew");
    assert!(matches!(pool.session_liveness(LAUNCH_SESSION).await.expect("liveness"), TerminalSessionLiveness::Running));
    let endpoint = pool.cleat_endpoint(LAUNCH_SESSION).await.expect("endpoint").expect("one daemon holds the session");
    assert_eq!(endpoint.session, ENCODED_LAUNCH_SESSION, "a direct attach names the id cleat holds");

    pool.operator_deliver(LAUNCH_SESSION, "hello").await.expect("send");
    assert_eq!(pool.capture_screen(LAUNCH_SESSION).await.expect("capture").as_deref(), Some("screen"));
    let attach = pool
        .attach_args_for_mode(LAUNCH_SESSION, Some("claude"), &ExecutionEnvironmentPath::new("/repo"), &Vec::new(), AttachMode::Take)
        .expect("attach args");
    let flat = flotilla_protocol::arg::flatten(&attach, 0);
    assert!(flat.ends_with(&format!(" '{ENCODED_LAUNCH_SESSION}'")), "attach names the id cleat holds: {flat}");
    pool.kill_session(LAUNCH_SESSION).await.expect("kill");
    assert!(cleat.sessions.lock().expect("sessions").is_empty());
}

// A cleat older than #4374 takes only `[A-Za-z0-9._-]`. The pool refuses to launch a name it
// cannot hold, saying to upgrade, rather than renaming the session for it.
#[tokio::test]
async fn an_older_cleat_is_refused_a_name_it_cannot_hold() {
    let cleat = Arc::new(NameRuleCleat::default());
    let pool = CleatTerminalPool::new(Arc::clone(&cleat) as Arc<dyn CommandRunner>, "cleat", &EnvironmentBag::new());
    pool.launch_capability.set(super::LaunchCapability { env_clear: true, argv: true, any_name: false }).expect("known launch capability");

    let refused = pool
        .ensure_session(LAUNCH_SESSION, "claude", &ExecutionEnvironmentPath::new("/repo"), &Vec::new(), &[])
        .await
        .expect_err("an older cleat cannot hold the name");
    assert!(refused.contains("upgrade cleat"), "{refused}");
    assert!(cleat.sessions.lock().expect("sessions").is_empty());

    pool.ensure_session("terminal-conv-implement-coder", "claude", &ExecutionEnvironmentPath::new("/repo"), &Vec::new(), &[])
        .await
        .expect("a plain name still launches");
}
