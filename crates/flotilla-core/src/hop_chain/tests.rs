use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use flotilla_protocol::{arg::flatten, HostName, TerminalStatus};

use super::{
    remote::{RemoteHopResolver, SshRemoteHopResolver},
    resolver::{AlwaysSendKeys, AlwaysWrap, CombineStrategy, HopResolver},
    terminal::TerminalHopResolver,
    Arg, Hop, HopPlan, ResolutionContext, ResolvedAction, ResolvedPlan, SendKeyStep,
};
use crate::{
    attachable::{
        shared_in_memory_attachable_store, Attachable, AttachableContent, AttachableId, AttachableSet, AttachableStoreApi,
        InMemoryAttachableStore, SharedAttachableStore, TerminalAttachable, TerminalPurpose,
    },
    config::{HostsConfig, RemoteHostConfig, SshConfig},
    providers::terminal::{TerminalEnvVars, TerminalPool, TerminalSession},
};

fn minimal_context() -> ResolutionContext {
    ResolutionContext {
        current_host: HostName::new("test-host"),
        current_environment: None,
        working_directory: None,
        actions: Vec::new(),
        nesting_depth: 0,
    }
}

// ── Assertion helpers ───────────────────────────────────────────────

#[track_caller]
fn expect_command(action: &ResolvedAction) -> &[Arg] {
    match action {
        ResolvedAction::Command(args) => args,
        other => panic!("expected Command, got {other:?}"),
    }
}

#[track_caller]
fn expect_send_keys(action: &ResolvedAction) -> &[SendKeyStep] {
    match action {
        ResolvedAction::SendKeys { steps } => steps,
        other => panic!("expected SendKeys, got {other:?}"),
    }
}

#[track_caller]
fn expect_type_step(step: &SendKeyStep) -> &str {
    match step {
        SendKeyStep::Type(text) => text,
        other => panic!("expected Type step, got {other:?}"),
    }
}

#[track_caller]
fn expect_nested(arg: &Arg) -> &[Arg] {
    match arg {
        Arg::NestedCommand(args) => args,
        other => panic!("expected NestedCommand, got {other:?}"),
    }
}

fn flatten_actions(actions: &[ResolvedAction]) -> Vec<String> {
    actions
        .iter()
        .map(|action| match action {
            ResolvedAction::Command(args) => format!("Command: {}", flatten(args, 0)),
            ResolvedAction::SendKeys { steps } => format!("SendKeys: {steps:?}"),
        })
        .collect()
}

// ── CombineStrategy tests ───────────────────────────────────────────

fn all_hop_variants() -> [Hop; 3] {
    [
        Hop::RemoteToHost { host: HostName::new("gouda") },
        Hop::AttachTerminal { attachable_id: crate::attachable::AttachableId::new("sess-1") },
        Hop::RunCommand { command: vec![super::Arg::Literal("echo".into())] },
    ]
}

#[test]
fn always_wrap_returns_true_for_all_hop_variants() {
    let strategy = AlwaysWrap;
    let context = minimal_context();
    for hop in &all_hop_variants() {
        assert!(strategy.should_wrap(hop, &context), "AlwaysWrap should return true for {hop:?}");
    }
}

#[test]
fn always_send_keys_returns_false_for_all_hop_variants() {
    let strategy = AlwaysSendKeys;
    let context = minimal_context();
    for hop in &all_hop_variants() {
        assert!(!strategy.should_wrap(hop, &context), "AlwaysSendKeys should return false for {hop:?}");
    }
}

// ── SshRemoteHopResolver tests ──────────────────────────────────────

fn test_hosts_config() -> HostsConfig {
    let mut hosts = HashMap::new();
    hosts.insert("feta".into(), RemoteHostConfig {
        hostname: "feta.local".into(),
        expected_host_name: "feta".into(),
        user: Some("alice".into()),
        daemon_socket: "/tmp/flotilla.sock".into(),
        ssh_multiplex: None,
    });
    hosts.insert("gouda".into(), RemoteHostConfig {
        hostname: "gouda.example.com".into(),
        expected_host_name: "gouda".into(),
        user: None,
        daemon_socket: "/tmp/flotilla.sock".into(),
        ssh_multiplex: Some(false),
    });
    HostsConfig { ssh: SshConfig::default(), hosts }
}

fn test_resolver() -> SshRemoteHopResolver {
    // Use a temp dir for config_base so SSH control socket dir creation works
    let config_base = std::env::temp_dir().join("flotilla-test-ssh-resolver");
    SshRemoteHopResolver::new(config_base, test_hosts_config())
}

fn test_resolver_no_multiplex() -> SshRemoteHopResolver {
    let config_base = std::env::temp_dir().join("flotilla-test-ssh-resolver-nomux");
    let hosts = HostsConfig { ssh: SshConfig { multiplex: false }, hosts: test_hosts_config().hosts };
    SshRemoteHopResolver::new(config_base, hosts)
}

// ── resolve_wrap tests ──────────────────────────────────────────────

#[test]
fn wrap_with_working_directory_and_inner_command() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.working_directory = Some(PathBuf::from("/home/alice/dev/my-repo"));
    context.actions.push(ResolvedAction::Command(vec![
        Arg::Quoted("cleat".into()),
        Arg::Literal("attach".into()),
        Arg::Literal("sess-1".into()),
    ]));

    resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect("resolve_wrap should succeed");

    assert_eq!(context.actions.len(), 1);
    assert!(context.working_directory.is_none(), "working_directory should be consumed");

    let args = expect_command(&context.actions[0]);

    // Verify the structure: ssh -t 'alice@feta.local' '<$SHELL -l -c ...>'
    assert_eq!(args[0], Arg::Literal("ssh".into()));
    assert_eq!(args[1], Arg::Literal("-t".into()));
    assert_eq!(args[2], Arg::Quoted("alice@feta.local".into()));

    // The outer NestedCommand wraps $SHELL -l -c <inner>
    let shell_args = expect_nested(&args[3]);
    assert_eq!(shell_args[0], Arg::Literal("${SHELL:-/bin/sh}".into()));
    assert_eq!(shell_args[1], Arg::Literal("-l".into()));
    assert_eq!(shell_args[2], Arg::Literal("-c".into()));
    // The inner NestedCommand has cd + inner command
    let inner_args = expect_nested(&shell_args[3]);
    assert_eq!(inner_args[0], Arg::Literal("cd".into()));
    assert_eq!(inner_args[1], Arg::Quoted("/home/alice/dev/my-repo".into()));
    assert_eq!(inner_args[2], Arg::Literal("&&".into()));
    assert_eq!(inner_args[3], Arg::Quoted("cleat".into()));
    assert_eq!(inner_args[4], Arg::Literal("attach".into()));
    assert_eq!(inner_args[5], Arg::Literal("sess-1".into()));

    // Regression: full Arg tree matches expected structure (replaces old ssh wrap pattern)
    let expected_args = vec![
        Arg::Literal("ssh".into()),
        Arg::Literal("-t".into()),
        Arg::Quoted("alice@feta.local".into()),
        Arg::NestedCommand(vec![
            Arg::Literal("${SHELL:-/bin/sh}".into()),
            Arg::Literal("-l".into()),
            Arg::Literal("-c".into()),
            Arg::NestedCommand(vec![
                Arg::Literal("cd".into()),
                Arg::Quoted("/home/alice/dev/my-repo".into()),
                Arg::Literal("&&".into()),
                Arg::Quoted("cleat".into()),
                Arg::Literal("attach".into()),
                Arg::Literal("sess-1".into()),
            ]),
        ]),
    ];
    assert_eq!(args, &expected_args, "Arg tree should match expected structure");

    // Verify flatten output preserves key structural properties
    let flat = flatten(args, 0);
    assert!(flat.starts_with("ssh -t "), "should start with ssh -t: {flat}");
    assert!(flat.contains("'alice@feta.local'"), "should contain quoted target: {flat}");
    assert!(flat.contains("${SHELL:-/bin/sh} -l -c"), "should contain shell invocation: {flat}");
    assert!(flat.contains("/home/alice/dev/my-repo"), "should contain checkout dir: {flat}");
    assert!(flat.contains("cleat"), "should contain binary name: {flat}");
    assert!(flat.contains("attach sess-1"), "should contain trailing args: {flat}");
}

#[test]
fn wrap_without_working_directory() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    // No working_directory set
    context.actions.push(ResolvedAction::Command(vec![Arg::Literal("tmux".into()), Arg::Literal("attach".into())]));

    resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect("resolve_wrap should succeed");

    assert_eq!(context.actions.len(), 1);
    let args = expect_command(&context.actions[0]);

    // Should NOT have cd prefix
    let inner_args = expect_nested(&expect_nested(&args[3])[3]);
    assert_eq!(inner_args[0], Arg::Literal("tmux".into()));
    assert_eq!(inner_args[1], Arg::Literal("attach".into()));
    assert_eq!(inner_args.len(), 2, "no cd prefix when working_directory is None");
}

#[test]
fn wrap_empty_command_with_working_directory_produces_login_shell() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.working_directory = Some(PathBuf::from("/home/alice/dev/my-repo"));
    context.actions.push(ResolvedAction::Command(vec![]));

    resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect("resolve_wrap should succeed");

    let args = expect_command(&context.actions[0]);

    // Regression: full Arg tree for empty command → login shell pattern
    let expected_args = vec![
        Arg::Literal("ssh".into()),
        Arg::Literal("-t".into()),
        Arg::Quoted("alice@feta.local".into()),
        Arg::NestedCommand(vec![
            Arg::Literal("${SHELL:-/bin/sh}".into()),
            Arg::Literal("-l".into()),
            Arg::Literal("-c".into()),
            Arg::NestedCommand(vec![
                Arg::Literal("cd".into()),
                Arg::Quoted("/home/alice/dev/my-repo".into()),
                Arg::Literal("&&".into()),
                Arg::Literal("exec".into()),
                Arg::Literal("${SHELL:-/bin/sh}".into()),
                Arg::Literal("-l".into()),
            ]),
        ]),
    ];
    assert_eq!(args, &expected_args, "empty command should produce login shell pattern");

    let flat = flatten(args, 0);
    assert!(flat.contains("exec ${SHELL:-/bin/sh} -l"), "flattened should contain exec shell: {flat}");
}

#[test]
fn wrap_with_multiplex_includes_control_args() {
    let resolver = test_resolver();
    let mut context = minimal_context();
    context.actions.push(ResolvedAction::Command(vec![Arg::Literal("echo".into()), Arg::Literal("hi".into())]));

    // feta inherits global ssh.multiplex=true
    resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect("resolve_wrap should succeed");

    let args = expect_command(&context.actions[0]);

    // Should have: ssh -t -o ControlMaster=auto -o ControlPath=... -o ControlPersist=60 'alice@feta.local' <nested>
    assert_eq!(args[0], Arg::Literal("ssh".into()));
    assert_eq!(args[1], Arg::Literal("-t".into()));
    assert_eq!(args[2], Arg::Literal("-o".into()));
    assert_eq!(args[3], Arg::Literal("ControlMaster=auto".into()));
    assert_eq!(args[4], Arg::Literal("-o".into()));
    // args[5] is ControlPath=<path> — just check it starts correctly
    match &args[5] {
        Arg::Quoted(s) => assert!(s.starts_with("ControlPath="), "expected ControlPath, got {s}"),
        other => panic!("expected Quoted ControlPath, got {other:?}"),
    }
    assert_eq!(args[6], Arg::Literal("-o".into()));
    assert_eq!(args[7], Arg::Literal("ControlPersist=60".into()));
    assert_eq!(args[8], Arg::Quoted("alice@feta.local".into()));
    // args[9] is the NestedCommand
    assert!(matches!(args[9], Arg::NestedCommand(_)));

    // Regression: flattened output preserves multiplex args
    let flat = flatten(args, 0);
    assert!(flat.starts_with("ssh -t -o ControlMaster=auto -o "), "should have multiplex args: {flat}");
    assert!(flat.contains("ControlPersist=60"), "should have ControlPersist: {flat}");
    assert!(flat.contains("'alice@feta.local'"), "should have quoted target: {flat}");
}

#[test]
fn wrap_without_multiplex_has_no_control_args() {
    let resolver = test_resolver();
    let mut context = minimal_context();
    context.actions.push(ResolvedAction::Command(vec![Arg::Literal("echo".into())]));

    // gouda has ssh_multiplex=false
    resolver.resolve_wrap(&HostName::new("gouda"), &mut context).expect("resolve_wrap should succeed");

    let args = expect_command(&context.actions[0]);

    // ssh -t 'gouda.example.com' <nested> — no -o flags
    assert_eq!(args[0], Arg::Literal("ssh".into()));
    assert_eq!(args[1], Arg::Literal("-t".into()));
    assert_eq!(args[2], Arg::Quoted("gouda.example.com".into()));
    assert!(matches!(args[3], Arg::NestedCommand(_)));
    assert_eq!(args.len(), 4);
}

#[test]
fn wrap_target_format_with_and_without_user() {
    let resolver = test_resolver_no_multiplex();

    // feta has user=Some("alice"), hostname="feta.local" -> "alice@feta.local"
    // gouda has user=None, hostname="gouda.example.com" -> "gouda.example.com"
    for (host, expected_target) in [("feta", "alice@feta.local"), ("gouda", "gouda.example.com")] {
        let mut context = minimal_context();
        context.actions.push(ResolvedAction::Command(vec![Arg::Literal("ls".into())]));
        resolver.resolve_wrap(&HostName::new(host), &mut context).expect("resolve_wrap should succeed");
        let args = expect_command(&context.actions[0]);
        assert_eq!(args[2], Arg::Quoted(expected_target.into()), "target for host {host}");
    }
}

#[test]
fn wrap_unknown_host_returns_error() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.actions.push(ResolvedAction::Command(vec![Arg::Literal("ls".into())]));

    let err = resolver.resolve_wrap(&HostName::new("unknown"), &mut context).expect_err("should fail for unknown host");
    assert!(err.contains("unknown remote host"), "error should mention unknown host: {err}");
}

#[test]
fn wrap_empty_stack_returns_error() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    // No actions on stack

    let err = resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect_err("should fail with empty stack");
    assert!(err.contains("no inner action"), "error should mention missing action: {err}");
}

#[test]
fn wrap_non_command_on_stack_returns_error() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.actions.push(ResolvedAction::SendKeys { steps: vec![SendKeyStep::Type("hello".into())] });

    let err = resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect_err("should fail with non-Command");
    assert!(err.contains("expected Command"), "error should mention expected Command: {err}");
}

// current_host is updated by HopResolver, not by per-hop resolvers
#[test]
fn wrap_and_enter_do_not_update_current_host() {
    for method in ["wrap", "enter"] {
        let resolver = test_resolver_no_multiplex();
        let mut context = minimal_context();
        context.actions.push(ResolvedAction::Command(vec![Arg::Literal("ls".into())]));

        match method {
            "wrap" => resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect("resolve_wrap should succeed"),
            "enter" => resolver.resolve_enter(&HostName::new("feta"), &mut context).expect("resolve_enter should succeed"),
            _ => unreachable!(),
        }
        assert_eq!(context.current_host.as_str(), "test-host", "{method} should not update current_host");
    }
}

// ── resolve_enter tests ─────────────────────────────────────────────

#[test]
fn enter_produces_ssh_command_and_sendkeys() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.working_directory = Some(PathBuf::from("/home/alice/dev/my-repo"));
    context.actions.push(ResolvedAction::Command(vec![
        Arg::Quoted("cleat".into()),
        Arg::Literal("attach".into()),
        Arg::Literal("sess-1".into()),
    ]));

    resolver.resolve_enter(&HostName::new("feta"), &mut context).expect("resolve_enter should succeed");

    // Stack should have: [SendKeys, SSH Command] (SSH on top, SendKeys below)
    assert_eq!(context.actions.len(), 2);

    // Bottom: SendKeys with the flattened inner command
    let steps = expect_send_keys(&context.actions[0]);
    assert_eq!(steps.len(), 2);
    let text = expect_type_step(&steps[0]);
    assert!(text.contains("cd"), "should include cd: {text}");
    assert!(text.contains("/home/alice/dev/my-repo"), "should include dir: {text}");
    assert!(text.contains("'cleat' attach sess-1"), "should include inner cmd: {text}");
    assert_eq!(steps[1], SendKeyStep::WaitForPrompt);

    // Top: SSH enter command (no inner command arg)
    let args = expect_command(&context.actions[1]);
    assert_eq!(args[0], Arg::Literal("ssh".into()));
    assert_eq!(args[1], Arg::Literal("-t".into()));
    assert_eq!(args[2], Arg::Quoted("alice@feta.local".into()));
    assert_eq!(args.len(), 3, "SSH enter command should not have a nested command arg");
}

#[test]
fn enter_without_working_directory() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.actions.push(ResolvedAction::Command(vec![Arg::Literal("echo".into()), Arg::Quoted("hello".into())]));

    resolver.resolve_enter(&HostName::new("feta"), &mut context).expect("resolve_enter should succeed");

    assert_eq!(context.actions.len(), 2);

    // SendKeys should just have the inner command, no cd
    let text = expect_type_step(&expect_send_keys(&context.actions[0])[0]);
    assert!(!text.contains("cd"), "should not include cd: {text}");
    assert_eq!(text, "echo 'hello'");
}

#[test]
fn enter_empty_command_no_sendkeys() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.actions.push(ResolvedAction::Command(vec![]));

    resolver.resolve_enter(&HostName::new("feta"), &mut context).expect("resolve_enter should succeed");

    // Only the SSH command, no SendKeys since there's nothing to type
    assert_eq!(context.actions.len(), 1);
    assert_eq!(expect_command(&context.actions[0])[0], Arg::Literal("ssh".into()));
}

#[test]
fn enter_with_working_directory_and_empty_command() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.working_directory = Some(PathBuf::from("/remote/dir"));
    context.actions.push(ResolvedAction::Command(vec![]));

    resolver.resolve_enter(&HostName::new("feta"), &mut context).expect("resolve_enter should succeed");

    // Should have SendKeys with just the cd, plus SSH command
    assert_eq!(context.actions.len(), 2);
    assert_eq!(expect_type_step(&expect_send_keys(&context.actions[0])[0]), "cd '/remote/dir'");
}

// ── PoolTerminalHopResolver tests ────────────────────────────────────

/// A fake TerminalPool that records attach_args calls and returns a predictable Arg vector.
struct FakeTerminalPool {
    calls: Mutex<Vec<FakePoolCall>>,
}

#[derive(Debug, Clone)]
struct FakePoolCall {
    session_name: String,
    command: String,
    cwd: PathBuf,
    env_vars: TerminalEnvVars,
}

impl FakeTerminalPool {
    fn new() -> Self {
        Self { calls: Mutex::new(Vec::new()) }
    }

    fn recorded_calls(&self) -> Vec<FakePoolCall> {
        self.calls.lock().expect("lock").clone()
    }
}

#[async_trait::async_trait]
impl TerminalPool for FakeTerminalPool {
    async fn list_sessions(&self) -> Result<Vec<TerminalSession>, String> {
        Ok(Vec::new())
    }

    async fn ensure_session(&self, _session_name: &str, _command: &str, _cwd: &Path, _env_vars: &TerminalEnvVars) -> Result<(), String> {
        Ok(())
    }

    fn attach_args(
        &self,
        session_name: &str,
        command: &str,
        cwd: &Path,
        env_vars: &TerminalEnvVars,
    ) -> Result<Vec<flotilla_protocol::arg::Arg>, String> {
        self.calls.lock().expect("lock").push(FakePoolCall {
            session_name: session_name.to_string(),
            command: command.to_string(),
            cwd: cwd.to_path_buf(),
            env_vars: env_vars.clone(),
        });
        Ok(vec![Arg::Quoted("cleat".into()), Arg::Literal("attach".into()), Arg::Literal(session_name.to_string())])
    }

    async fn kill_session(&self, _session_name: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Populate a store with one terminal attachable.
fn insert_terminal(
    store: &mut dyn AttachableStoreApi,
    attachable_id: &AttachableId,
    host_affinity: Option<HostName>,
    command: &str,
    cwd: &Path,
) {
    let set_id = store.allocate_set_id();
    store.insert_set(AttachableSet {
        id: set_id.clone(),
        host_affinity,
        checkout: None,
        template_identity: None,
        members: vec![attachable_id.clone()],
    });
    store.insert_attachable(Attachable {
        id: attachable_id.clone(),
        set_id,
        content: AttachableContent::Terminal(TerminalAttachable {
            purpose: TerminalPurpose { checkout: "feat".to_string(), role: "shell".to_string(), index: 0 },
            command: command.to_string(),
            working_directory: cwd.to_path_buf(),
            status: TerminalStatus::Disconnected,
        }),
    });
}

/// Helper: create a shared store with one terminal attachable pre-inserted.
fn store_with_terminal(attachable_id: &AttachableId, command: &str, cwd: &Path) -> SharedAttachableStore {
    let store = shared_in_memory_attachable_store();
    {
        let mut s = store.lock().expect("lock");
        insert_terminal(&mut *s, attachable_id, Some(HostName::new("test-host")), command, cwd);
    }
    store
}

#[test]
fn terminal_resolve_pushes_command_onto_context() {
    use super::terminal::PoolTerminalHopResolver;

    let att_id = AttachableId::new("term-1");
    let pool = Arc::new(FakeTerminalPool::new());
    let store = store_with_terminal(&att_id, "bash", Path::new("/repo/wt-feat"));
    let resolver = PoolTerminalHopResolver::new(pool.clone(), store, Some("/tmp/flotilla.sock".to_string()));

    let mut context = minimal_context();
    resolver.resolve(&att_id, &mut context).expect("resolve should succeed");

    // Verify a Command was pushed onto the context
    assert_eq!(context.actions.len(), 1);
    let args = expect_command(&context.actions[0]);
    assert_eq!(args[0], Arg::Quoted("cleat".into()));
    assert_eq!(args[1], Arg::Literal("attach".into()));
    assert_eq!(args[2], Arg::Literal(att_id.to_string()));

    // Verify the pool received the correct arguments
    let calls = pool.recorded_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].session_name, att_id.to_string());
    assert_eq!(calls[0].command, "bash");
    assert_eq!(calls[0].cwd, PathBuf::from("/repo/wt-feat"));
}

#[test]
fn terminal_resolve_unknown_attachable_returns_error() {
    use super::terminal::PoolTerminalHopResolver;

    let pool = Arc::new(FakeTerminalPool::new());
    let store = shared_in_memory_attachable_store();
    let resolver = PoolTerminalHopResolver::new(pool, store, None);

    let mut context = minimal_context();
    let err = resolver.resolve(&AttachableId::new("nonexistent"), &mut context).expect_err("should fail for unknown attachable");
    assert!(err.contains("attachable not found"), "error should mention not found: {err}");
    assert!(context.actions.is_empty(), "no actions should be pushed on error");
}

#[test]
fn terminal_resolve_injects_env_vars_with_socket() {
    use super::terminal::PoolTerminalHopResolver;

    let att_id = AttachableId::new("term-env");
    let pool = Arc::new(FakeTerminalPool::new());
    let store = store_with_terminal(&att_id, "claude", Path::new("/repo/wt-feat"));
    let resolver = PoolTerminalHopResolver::new(pool.clone(), store, Some("/run/flotilla.sock".to_string()));

    let mut context = minimal_context();
    resolver.resolve(&att_id, &mut context).expect("resolve should succeed");

    let calls = pool.recorded_calls();
    assert_eq!(calls.len(), 1);
    let env_vars = &calls[0].env_vars;

    assert!(
        env_vars.iter().any(|(k, v)| k == "FLOTILLA_ATTACHABLE_ID" && v == att_id.as_str()),
        "should have FLOTILLA_ATTACHABLE_ID: {env_vars:?}"
    );
    assert!(
        env_vars.iter().any(|(k, v)| k == "FLOTILLA_DAEMON_SOCKET" && v == "/run/flotilla.sock"),
        "should have FLOTILLA_DAEMON_SOCKET: {env_vars:?}"
    );
}

#[test]
fn terminal_resolve_omits_socket_env_var_when_none() {
    use super::terminal::PoolTerminalHopResolver;

    let att_id = AttachableId::new("term-nosock");
    let pool = Arc::new(FakeTerminalPool::new());
    let store = store_with_terminal(&att_id, "bash", Path::new("/repo/wt-feat"));
    let resolver = PoolTerminalHopResolver::new(pool.clone(), store, None);

    let mut context = minimal_context();
    resolver.resolve(&att_id, &mut context).expect("resolve should succeed");

    let calls = pool.recorded_calls();
    assert_eq!(calls.len(), 1);
    let env_vars = &calls[0].env_vars;

    assert!(env_vars.iter().any(|(k, _)| k == "FLOTILLA_ATTACHABLE_ID"), "should have FLOTILLA_ATTACHABLE_ID: {env_vars:?}");
    assert!(
        !env_vars.iter().any(|(k, _)| k == "FLOTILLA_DAEMON_SOCKET"),
        "should NOT have FLOTILLA_DAEMON_SOCKET when daemon_socket_path is None: {env_vars:?}"
    );
}

// ── Mock resolvers for HopResolver tests ─────────────────────────────

/// Records which methods were called on the remote hop resolver.
#[derive(Debug, Clone)]
enum MockRemoteCall {
    Wrap(HostName),
    Enter(HostName),
}

/// A mock RemoteHopResolver that records calls and produces predictable outputs.
///
/// - `resolve_wrap`: pops the inner Command, prepends `[Literal("ssh"), Quoted(host)]`,
///   pushes back a single `Command` with `NestedCommand(inner)`.
/// - `resolve_enter`: pops the inner Command, pushes an SSH Command, then converts
///   the inner to SendKeys.
struct MockRemoteHopResolver {
    calls: Mutex<Vec<MockRemoteCall>>,
}

impl MockRemoteHopResolver {
    fn new() -> Self {
        Self { calls: Mutex::new(Vec::new()) }
    }

    fn recorded_calls(&self) -> Vec<MockRemoteCall> {
        self.calls.lock().expect("lock").clone()
    }
}

impl RemoteHopResolver for MockRemoteHopResolver {
    fn resolve_wrap(&self, host: &HostName, context: &mut ResolutionContext) -> Result<(), String> {
        self.calls.lock().expect("lock").push(MockRemoteCall::Wrap(host.clone()));

        let inner_action = context.actions.pop().ok_or("mock resolve_wrap: no inner action")?;
        let inner_args = match inner_action {
            ResolvedAction::Command(args) => args,
            other => return Err(format!("mock resolve_wrap: expected Command, got {other:?}")),
        };

        let mut ssh_args = vec![Arg::Literal("ssh".into()), Arg::Quoted(host.as_str().to_string())];
        ssh_args.push(Arg::NestedCommand(inner_args));
        context.actions.push(ResolvedAction::Command(ssh_args));
        Ok(())
    }

    fn resolve_enter(&self, host: &HostName, context: &mut ResolutionContext) -> Result<(), String> {
        self.calls.lock().expect("lock").push(MockRemoteCall::Enter(host.clone()));

        let inner_action = context.actions.pop().ok_or("mock resolve_enter: no inner action")?;
        let inner_args = match inner_action {
            ResolvedAction::Command(args) => args,
            other => return Err(format!("mock resolve_enter: expected Command, got {other:?}")),
        };

        // Convert inner to SendKeys
        if !inner_args.is_empty() {
            let text = flotilla_protocol::arg::flatten(&inner_args, 0);
            context.actions.push(ResolvedAction::SendKeys { steps: vec![SendKeyStep::Type(text), SendKeyStep::WaitForPrompt] });
        }

        // Push SSH enter command
        let ssh_args = vec![Arg::Literal("ssh".into()), Arg::Quoted(host.as_str().to_string())];
        context.actions.push(ResolvedAction::Command(ssh_args));
        Ok(())
    }
}

/// A mock TerminalHopResolver that pushes a simple attach command.
struct MockTerminalHopResolver {
    calls: Mutex<Vec<AttachableId>>,
}

impl MockTerminalHopResolver {
    fn new() -> Self {
        Self { calls: Mutex::new(Vec::new()) }
    }

    fn recorded_calls(&self) -> Vec<AttachableId> {
        self.calls.lock().expect("lock").clone()
    }
}

impl TerminalHopResolver for MockTerminalHopResolver {
    fn resolve(&self, attachable_id: &AttachableId, context: &mut ResolutionContext) -> Result<(), String> {
        self.calls.lock().expect("lock").push(attachable_id.clone());
        context.actions.push(ResolvedAction::Command(vec![Arg::Literal("mock-attach".into()), Arg::Quoted(attachable_id.to_string())]));
        Ok(())
    }
}

fn mock_hop_resolver(strategy: Arc<dyn CombineStrategy>) -> (HopResolver, Arc<MockRemoteHopResolver>, Arc<MockTerminalHopResolver>) {
    let remote = Arc::new(MockRemoteHopResolver::new());
    let terminal = Arc::new(MockTerminalHopResolver::new());
    let resolver = HopResolver { remote: remote.clone(), terminal: terminal.clone(), strategy };
    (resolver, remote, terminal)
}

/// Resolve a hop plan with mock resolvers and return all outputs needed for assertions.
struct MockResolution {
    resolved: ResolvedPlan,
    remote_calls: Vec<MockRemoteCall>,
    terminal_calls: Vec<AttachableId>,
    context: ResolutionContext,
}

fn resolve_with_mocks(strategy: Arc<dyn CombineStrategy>, hops: Vec<Hop>) -> MockResolution {
    let (resolver, remote, terminal) = mock_hop_resolver(strategy);
    let mut context = minimal_context();
    let resolved = resolver.resolve(&HopPlan(hops), &mut context).expect("resolve should succeed");
    MockResolution { resolved, remote_calls: remote.recorded_calls(), terminal_calls: terminal.recorded_calls(), context }
}

// ── HopResolver tests ────────────────────────────────────────────────

#[test]
fn hop_resolver_remote_run_command_with_always_wrap() {
    let r = resolve_with_mocks(Arc::new(AlwaysWrap), vec![Hop::RemoteToHost { host: HostName::new("feta") }, Hop::RunCommand {
        command: vec![Arg::Literal("echo".into()), Arg::Literal("hello".into())],
    }]);

    assert_eq!(r.resolved.0.len(), 1);
    let args = expect_command(&r.resolved.0[0]);
    assert_eq!(args[0], Arg::Literal("ssh".into()));
    assert_eq!(args[1], Arg::Quoted("feta".into()));
    let inner = expect_nested(&args[2]);
    assert_eq!(inner[0], Arg::Literal("echo".into()));
    assert_eq!(inner[1], Arg::Literal("hello".into()));

    assert_eq!(r.remote_calls.len(), 1);
    assert!(matches!(&r.remote_calls[0], MockRemoteCall::Wrap(h) if h.as_str() == "feta"));
    assert!(r.terminal_calls.is_empty());
}

#[test]
fn hop_resolver_remote_run_command_with_always_send_keys() {
    let r = resolve_with_mocks(Arc::new(AlwaysSendKeys), vec![Hop::RemoteToHost { host: HostName::new("feta") }, Hop::RunCommand {
        command: vec![Arg::Literal("echo".into()), Arg::Literal("hello".into())],
    }]);

    assert_eq!(r.resolved.0.len(), 2);

    let steps = expect_send_keys(&r.resolved.0[0]);
    assert_eq!(steps.len(), 2);
    let text = expect_type_step(&steps[0]);
    assert!(text.contains("echo"), "SendKeys should contain inner command: {text}");
    assert!(text.contains("hello"), "SendKeys should contain inner command args: {text}");
    assert_eq!(steps[1], SendKeyStep::WaitForPrompt);

    let args = expect_command(&r.resolved.0[1]);
    assert_eq!(args[0], Arg::Literal("ssh".into()));
    assert_eq!(args[1], Arg::Quoted("feta".into()));
    assert_eq!(args.len(), 2, "SSH enter command should not have nested command");

    assert_eq!(r.remote_calls.len(), 1);
    assert!(matches!(&r.remote_calls[0], MockRemoteCall::Enter(h) if h.as_str() == "feta"));
    assert!(r.terminal_calls.is_empty());
}

#[test]
fn hop_resolver_collapses_remote_to_local_host() {
    let r = resolve_with_mocks(Arc::new(AlwaysWrap), vec![Hop::RemoteToHost { host: HostName::new("test-host") }, Hop::RunCommand {
        command: vec![Arg::Literal("ls".into())],
    }]);

    assert_eq!(r.resolved.0.len(), 1);
    assert_eq!(expect_command(&r.resolved.0[0]), [Arg::Literal("ls".into())]);
    assert!(r.remote_calls.is_empty());
    assert!(r.terminal_calls.is_empty());
}

#[test]
fn hop_resolver_remote_attach_terminal_with_always_wrap() {
    let att_id = AttachableId::new("sess-1");
    let r = resolve_with_mocks(Arc::new(AlwaysWrap), vec![Hop::RemoteToHost { host: HostName::new("feta") }, Hop::AttachTerminal {
        attachable_id: att_id.clone(),
    }]);

    // Terminal resolver pushes Command(mock-attach, sess-1)
    // Then remote resolver wraps it: ssh feta <NestedCommand(mock-attach, sess-1)>
    assert_eq!(r.resolved.0.len(), 1);
    let args = expect_command(&r.resolved.0[0]);
    assert_eq!(args[0], Arg::Literal("ssh".into()));
    assert_eq!(args[1], Arg::Quoted("feta".into()));
    let inner = expect_nested(&args[2]);
    assert_eq!(inner[0], Arg::Literal("mock-attach".into()));
    assert_eq!(inner[1], Arg::Quoted("sess-1".into()));

    assert_eq!(r.terminal_calls.len(), 1);
    assert_eq!(r.terminal_calls[0], att_id);
    assert_eq!(r.remote_calls.len(), 1);
    assert!(matches!(&r.remote_calls[0], MockRemoteCall::Wrap(h) if h.as_str() == "feta"));
}

#[test]
fn hop_resolver_empty_plan() {
    let r = resolve_with_mocks(Arc::new(AlwaysWrap), vec![]);

    assert!(r.resolved.0.is_empty(), "empty plan should produce empty resolved plan");
    assert!(r.remote_calls.is_empty());
    assert!(r.terminal_calls.is_empty());
}

#[test]
fn hop_resolver_run_command_only() {
    let r = resolve_with_mocks(Arc::new(AlwaysWrap), vec![Hop::RunCommand {
        command: vec![Arg::Literal("cargo".into()), Arg::Literal("build".into())],
    }]);

    assert_eq!(r.resolved.0.len(), 1);
    assert_eq!(expect_command(&r.resolved.0[0]), [Arg::Literal("cargo".into()), Arg::Literal("build".into())]);
    assert!(r.remote_calls.is_empty());
    assert!(r.terminal_calls.is_empty());
}

#[test]
fn hop_resolver_nesting_depth_incremented_for_remote_hops() {
    let r = resolve_with_mocks(Arc::new(AlwaysWrap), vec![Hop::RemoteToHost { host: HostName::new("feta") }, Hop::RunCommand {
        command: vec![Arg::Literal("ls".into())],
    }]);

    assert_eq!(r.context.nesting_depth, 1, "nesting_depth should be incremented for remote hop");
}

#[test]
fn hop_resolver_collapsed_hop_does_not_increment_nesting_depth() {
    let r = resolve_with_mocks(Arc::new(AlwaysWrap), vec![
        Hop::RemoteToHost { host: HostName::new("test-host") }, // same as current_host
        Hop::RunCommand { command: vec![Arg::Literal("ls".into())] },
    ]);

    assert_eq!(r.context.nesting_depth, 0, "nesting_depth should not change when hop is collapsed");
}

#[test]
fn hop_resolver_current_host_updated_after_remote_hop() {
    let r = resolve_with_mocks(Arc::new(AlwaysWrap), vec![Hop::RemoteToHost { host: HostName::new("feta") }, Hop::RunCommand {
        command: vec![Arg::Literal("ls".into())],
    }]);

    assert_eq!(r.context.current_host.as_str(), "feta", "current_host should be updated to remote host");
}

#[test]
fn hop_resolver_attach_terminal_only() {
    let att_id = AttachableId::new("term-local");
    let r = resolve_with_mocks(Arc::new(AlwaysWrap), vec![Hop::AttachTerminal { attachable_id: att_id }]);

    assert_eq!(r.resolved.0.len(), 1);
    assert_eq!(expect_command(&r.resolved.0[0]), [Arg::Literal("mock-attach".into()), Arg::Quoted("term-local".into())]);
    assert_eq!(r.terminal_calls.len(), 1);
    assert!(r.remote_calls.is_empty(), "remote resolver should not be called for local terminal attach");
}

#[test]
fn hop_resolver_remote_attach_terminal_with_always_send_keys() {
    let att_id = AttachableId::new("sess-2");
    let r = resolve_with_mocks(Arc::new(AlwaysSendKeys), vec![Hop::RemoteToHost { host: HostName::new("feta") }, Hop::AttachTerminal {
        attachable_id: att_id,
    }]);

    // Terminal resolver pushes Command(mock-attach, sess-2)
    // Remote resolve_enter pops it, converts to SendKeys, pushes SSH Command
    assert_eq!(r.resolved.0.len(), 2);

    let text = expect_type_step(&expect_send_keys(&r.resolved.0[0])[0]);
    assert!(text.contains("mock-attach"), "SendKeys should contain terminal command: {text}");

    let args = expect_command(&r.resolved.0[1]);
    assert_eq!(args[0], Arg::Literal("ssh".into()));
    assert_eq!(args[1], Arg::Quoted("feta".into()));

    assert_eq!(r.terminal_calls.len(), 1);
    assert_eq!(r.remote_calls.len(), 1);
    assert!(matches!(&r.remote_calls[0], MockRemoteCall::Enter(h) if h.as_str() == "feta"));
}

// ── HopPlanBuilder tests ─────────────────────────────────────────────

use super::builder::HopPlanBuilder;

/// Helper: create an in-memory store with a terminal attachable in a set with the given host affinity.
fn builder_store_with_host(attachable_id: &AttachableId, host_affinity: Option<HostName>) -> InMemoryAttachableStore {
    let mut store = InMemoryAttachableStore::new();
    insert_terminal(&mut store, attachable_id, host_affinity, "bash", Path::new("/repo/wt-feat"));
    store
}

#[test]
fn build_for_attachable_host_routing() {
    let local_host = HostName::new("my-host");
    // AttachableId("") is a placeholder — the match loop below substitutes the real att_id for AttachTerminal hops.
    let cases: &[(&str, Option<HostName>, &[Hop])] = &[
        ("local host → attach only", Some(HostName::new("my-host")), &[Hop::AttachTerminal { attachable_id: AttachableId::new("") }]),
        ("remote host → remote + attach", Some(HostName::new("feta")), &[
            Hop::RemoteToHost { host: HostName::new("feta") },
            Hop::AttachTerminal { attachable_id: AttachableId::new("") },
        ]),
        ("no host affinity → attach only", None, &[Hop::AttachTerminal { attachable_id: AttachableId::new("") }]),
    ];

    for (label, host_affinity, expected_pattern) in cases {
        let att_id = AttachableId::new(format!("term-{label}"));
        let store = builder_store_with_host(&att_id, host_affinity.clone());
        let builder = HopPlanBuilder::new(&local_host);

        let plan = builder.build_for_attachable(&att_id, &store).unwrap_or_else(|e| panic!("{label}: should succeed: {e}"));
        assert_eq!(plan.0.len(), expected_pattern.len(), "{label}: wrong hop count");

        for (i, expected_hop) in expected_pattern.iter().enumerate() {
            match expected_hop {
                Hop::RemoteToHost { host } => assert_eq!(plan.0[i], Hop::RemoteToHost { host: host.clone() }, "{label}: hop {i}"),
                Hop::AttachTerminal { .. } => {
                    assert_eq!(plan.0[i], Hop::AttachTerminal { attachable_id: att_id.clone() }, "{label}: hop {i}")
                }
                _ => panic!("{label}: unexpected hop pattern"),
            }
        }
    }
}

#[test]
fn build_for_attachable_unknown_id_returns_error() {
    let local_host = HostName::new("my-host");
    let store = InMemoryAttachableStore::new();
    let builder = HopPlanBuilder::new(&local_host);

    let err = builder.build_for_attachable(&AttachableId::new("nonexistent"), &store).expect_err("should fail for unknown attachable");
    assert!(err.contains("attachable not found"), "error should mention not found: {err}");
}

#[test]
fn build_for_prepared_command_host_routing() {
    let local_host = HostName::new("my-host");
    let command = vec![Arg::Literal("cargo".into()), Arg::Literal("build".into())];

    for (label, target, expect_remote_hop) in [("remote", "feta", true), ("local", "my-host", false)] {
        let target = HostName::new(target);
        let builder = HopPlanBuilder::new(&local_host);
        let plan = builder.build_for_prepared_command(&target, &command);

        if expect_remote_hop {
            assert_eq!(plan.0.len(), 2, "{label}: should have remote + run");
            assert_eq!(plan.0[0], Hop::RemoteToHost { host: target }, "{label}: hop 0");
            assert_eq!(plan.0[1], Hop::RunCommand { command: command.clone() }, "{label}: hop 1");
        } else {
            assert_eq!(plan.0.len(), 1, "{label}: should have run only");
            assert_eq!(plan.0[0], Hop::RunCommand { command: command.clone() }, "{label}: hop 0");
        }
    }
}

// ── E2E helpers ──────────────────────────────────────────────────────

/// Simple cleat-style attach args: `cleat attach <session> --cwd <cwd>`.
fn e2e_cleat_args(session: &str, cwd: &str) -> Vec<Arg> {
    vec![
        Arg::Quoted("cleat".into()),
        Arg::Literal("attach".into()),
        Arg::Quoted(session.into()),
        Arg::Literal("--cwd".into()),
        Arg::Quoted(cwd.into()),
    ]
}

/// Cleat args with a `--cmd` sub-command (env vars + shell exec).
fn e2e_cleat_args_with_cmd(session: &str, cwd: &str) -> Vec<Arg> {
    let mut args = e2e_cleat_args(session, cwd);
    args.push(Arg::Literal("--cmd".into()));
    args.push(Arg::NestedCommand(vec![
        Arg::Literal("env".into()),
        Arg::Literal(format!("FLOTILLA_ATTACHABLE_ID='{session}'")),
        Arg::Literal("FLOTILLA_DAEMON_SOCKET='/tmp/flotilla.sock'".into()),
        Arg::Literal("${SHELL:-/bin/sh}".into()),
        Arg::Literal("-lc".into()),
        Arg::Quoted("bash".into()),
    ]));
    args
}

/// Build an E2E hop resolver with the real SSH resolver (no multiplex) and the given strategy.
fn e2e_resolve(strategy: Arc<dyn CombineStrategy>, target: &str, cleat_args: &[Arg], working_directory: Option<&str>) -> ResolvedPlan {
    let local_host = HostName::new("my-laptop");
    let target_host = HostName::new(target);
    let builder = HopPlanBuilder::new(&local_host);
    let plan = builder.build_for_prepared_command(&target_host, cleat_args);

    let (remote, terminal): (Arc<dyn RemoteHopResolver>, Arc<dyn TerminalHopResolver>) = if target == "my-laptop" {
        (Arc::new(super::remote::NoopRemoteHopResolver), Arc::new(super::terminal::NoopTerminalHopResolver))
    } else {
        (Arc::new(test_resolver_no_multiplex()), Arc::new(super::terminal::NoopTerminalHopResolver))
    };
    let hop_resolver = HopResolver { remote, terminal, strategy };
    let mut context = ResolutionContext {
        current_host: local_host,
        current_environment: None,
        working_directory: working_directory.map(PathBuf::from),
        actions: Vec::new(),
        nesting_depth: 0,
    };
    hop_resolver.resolve(&plan, &mut context).expect("resolve should succeed")
}

/// End-to-end regression: mimics the real workspace creation flow.
///
/// 1. Build cleat-style attach args (ResolvedPaneCommand output)
/// 2. Build hop plan via `build_for_prepared_command`
/// 3. Resolve with SSH hop resolver (no multiplex) and AlwaysWrap
/// 4. Flatten to shell string
/// 5. Snapshot the final command string
#[test]
fn snapshot_e2e_workspace_creation_flow() {
    let cleat_args = e2e_cleat_args_with_cmd("feat__shell__0", "/home/alice/dev/my-repo/wt-feat");
    let resolved = e2e_resolve(Arc::new(AlwaysWrap), "feta", &cleat_args, Some("/home/alice/dev/my-repo/wt-feat"));

    insta::assert_debug_snapshot!("e2e_workspace_resolved_plan", &resolved);

    let flattened = flatten_actions(&resolved.0);
    insta::assert_debug_snapshot!("e2e_workspace_flattened_commands", &flattened);
}

/// End-to-end: local workspace creation (no remote hop).
/// The plan has only RunCommand — no SSH wrapping needed.
#[test]
fn snapshot_e2e_local_workspace_creation() {
    let cleat_args = e2e_cleat_args("main__shell__0", "/home/alice/dev/my-repo");
    let resolved = e2e_resolve(Arc::new(AlwaysWrap), "my-laptop", &cleat_args, None);

    insta::assert_debug_snapshot!("e2e_local_workspace_resolved_plan", &resolved);

    let flat = flatten(expect_command(&resolved.0[0]), 0);
    insta::assert_snapshot!("e2e_local_workspace_flattened", flat);
}

/// End-to-end: remote workspace creation with AlwaysSendKeys strategy.
#[test]
fn snapshot_e2e_remote_workspace_send_keys() {
    let cleat_args = e2e_cleat_args("feat__shell__0", "/home/alice/dev/my-repo/wt-feat");
    let resolved = e2e_resolve(Arc::new(AlwaysSendKeys), "feta", &cleat_args, Some("/home/alice/dev/my-repo/wt-feat"));

    insta::assert_debug_snapshot!("e2e_remote_send_keys_resolved_plan", &resolved);

    let flattened = flatten_actions(&resolved.0);
    insta::assert_debug_snapshot!("e2e_remote_send_keys_flattened", &flattened);
}
