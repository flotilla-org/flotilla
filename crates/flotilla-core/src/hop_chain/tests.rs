use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use flotilla_protocol::{arg::flatten, HostName, TerminalStatus};

use super::{
    remote::{RemoteHopResolver, SshRemoteHopResolver},
    resolver::{AlwaysSendKeys, AlwaysWrap, CombineStrategy},
    terminal::TerminalHopResolver,
    Arg, Hop, ResolutionContext, ResolvedAction, SendKeyStep,
};
use crate::{
    attachable::{
        shared_in_memory_attachable_store, AttachableContent, AttachableId, SharedAttachableStore, TerminalAttachable, TerminalPurpose,
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

// ── CombineStrategy tests ───────────────────────────────────────────

#[test]
fn always_wrap_returns_true() {
    let strategy = AlwaysWrap;
    let hop = Hop::RemoteToHost { host: HostName::new("feta") };
    let context = minimal_context();
    assert!(strategy.should_wrap(&hop, &context));
}

#[test]
fn always_send_keys_returns_false() {
    let strategy = AlwaysSendKeys;
    let hop = Hop::RemoteToHost { host: HostName::new("feta") };
    let context = minimal_context();
    assert!(!strategy.should_wrap(&hop, &context));
}

#[test]
fn always_wrap_returns_true_for_all_hop_variants() {
    let strategy = AlwaysWrap;
    let context = minimal_context();

    let hops = [
        Hop::RemoteToHost { host: HostName::new("gouda") },
        Hop::AttachTerminal { attachable_id: crate::attachable::AttachableId::new("sess-1") },
        Hop::RunCommand { command: vec![super::Arg::Literal("echo".into())] },
    ];

    for hop in &hops {
        assert!(strategy.should_wrap(hop, &context), "AlwaysWrap should return true for {hop:?}");
    }
}

#[test]
fn always_send_keys_returns_false_for_all_hop_variants() {
    let strategy = AlwaysSendKeys;
    let context = minimal_context();

    let hops = [
        Hop::RemoteToHost { host: HostName::new("gouda") },
        Hop::AttachTerminal { attachable_id: crate::attachable::AttachableId::new("sess-1") },
        Hop::RunCommand { command: vec![super::Arg::Literal("echo".into())] },
    ];

    for hop in &hops {
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
        Arg::Literal("cleat".into()),
        Arg::Literal("attach".into()),
        Arg::Literal("sess-1".into()),
    ]));

    resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect("resolve_wrap should succeed");

    assert_eq!(context.actions.len(), 1);
    assert!(context.working_directory.is_none(), "working_directory should be consumed");

    let action = &context.actions[0];
    let args = match action {
        ResolvedAction::Command(args) => args,
        other => panic!("expected Command, got {other:?}"),
    };

    // Verify the structure: ssh -t 'alice@feta.local' '<$SHELL -l -c ...>'
    assert_eq!(args[0], Arg::Literal("ssh".into()));
    assert_eq!(args[1], Arg::Literal("-t".into()));
    assert_eq!(args[2], Arg::Quoted("alice@feta.local".into()));

    // The outer NestedCommand wraps $SHELL -l -c <inner>
    match &args[3] {
        Arg::NestedCommand(shell_args) => {
            assert_eq!(shell_args[0], Arg::Literal("$SHELL".into()));
            assert_eq!(shell_args[1], Arg::Literal("-l".into()));
            assert_eq!(shell_args[2], Arg::Literal("-c".into()));
            // The inner NestedCommand has cd + inner command
            match &shell_args[3] {
                Arg::NestedCommand(inner_args) => {
                    assert_eq!(inner_args[0], Arg::Literal("cd".into()));
                    assert_eq!(inner_args[1], Arg::Quoted("/home/alice/dev/my-repo".into()));
                    assert_eq!(inner_args[2], Arg::Literal("&&".into()));
                    assert_eq!(inner_args[3], Arg::Literal("cleat".into()));
                    assert_eq!(inner_args[4], Arg::Literal("attach".into()));
                    assert_eq!(inner_args[5], Arg::Literal("sess-1".into()));
                }
                other => panic!("expected NestedCommand for inner, got {other:?}"),
            }
        }
        other => panic!("expected NestedCommand for $SHELL wrapper, got {other:?}"),
    }
}

#[test]
fn wrap_without_working_directory() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    // No working_directory set
    context.actions.push(ResolvedAction::Command(vec![Arg::Literal("tmux".into()), Arg::Literal("attach".into())]));

    resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect("resolve_wrap should succeed");

    assert_eq!(context.actions.len(), 1);
    let args = match &context.actions[0] {
        ResolvedAction::Command(args) => args,
        other => panic!("expected Command, got {other:?}"),
    };

    // Should NOT have cd prefix
    match &args[3] {
        Arg::NestedCommand(shell_args) => match &shell_args[3] {
            Arg::NestedCommand(inner_args) => {
                assert_eq!(inner_args[0], Arg::Literal("tmux".into()));
                assert_eq!(inner_args[1], Arg::Literal("attach".into()));
                assert_eq!(inner_args.len(), 2, "no cd prefix when working_directory is None");
            }
            other => panic!("expected NestedCommand, got {other:?}"),
        },
        other => panic!("expected NestedCommand, got {other:?}"),
    }
}

#[test]
fn wrap_empty_command_with_working_directory_produces_login_shell() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.working_directory = Some(PathBuf::from("/home/alice/dev/my-repo"));
    context.actions.push(ResolvedAction::Command(vec![]));

    resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect("resolve_wrap should succeed");

    let args = match &context.actions[0] {
        ResolvedAction::Command(args) => args,
        other => panic!("expected Command, got {other:?}"),
    };

    // Inner should be: cd <dir> && exec $SHELL -l
    match &args[3] {
        Arg::NestedCommand(shell_args) => match &shell_args[3] {
            Arg::NestedCommand(inner_args) => {
                assert_eq!(inner_args[0], Arg::Literal("cd".into()));
                assert_eq!(inner_args[1], Arg::Quoted("/home/alice/dev/my-repo".into()));
                assert_eq!(inner_args[2], Arg::Literal("&&".into()));
                assert_eq!(inner_args[3], Arg::Literal("exec".into()));
                assert_eq!(inner_args[4], Arg::Literal("$SHELL".into()));
                assert_eq!(inner_args[5], Arg::Literal("-l".into()));
            }
            other => panic!("expected NestedCommand, got {other:?}"),
        },
        other => panic!("expected NestedCommand, got {other:?}"),
    }
}

#[test]
fn wrap_with_multiplex_includes_control_args() {
    let resolver = test_resolver();
    let mut context = minimal_context();
    context.actions.push(ResolvedAction::Command(vec![Arg::Literal("echo".into()), Arg::Literal("hi".into())]));

    // feta inherits global ssh.multiplex=true
    resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect("resolve_wrap should succeed");

    let args = match &context.actions[0] {
        ResolvedAction::Command(args) => args,
        other => panic!("expected Command, got {other:?}"),
    };

    // Should have: ssh -t -o ControlMaster=auto -o ControlPath=... -o ControlPersist=60 'alice@feta.local' <nested>
    assert_eq!(args[0], Arg::Literal("ssh".into()));
    assert_eq!(args[1], Arg::Literal("-t".into()));
    assert_eq!(args[2], Arg::Literal("-o".into()));
    assert_eq!(args[3], Arg::Literal("ControlMaster=auto".into()));
    assert_eq!(args[4], Arg::Literal("-o".into()));
    // args[5] is ControlPath=<path> — just check it starts correctly
    match &args[5] {
        Arg::Literal(s) => assert!(s.starts_with("ControlPath="), "expected ControlPath, got {s}"),
        other => panic!("expected Literal ControlPath, got {other:?}"),
    }
    assert_eq!(args[6], Arg::Literal("-o".into()));
    assert_eq!(args[7], Arg::Literal("ControlPersist=60".into()));
    assert_eq!(args[8], Arg::Quoted("alice@feta.local".into()));
    // args[9] is the NestedCommand
    assert!(matches!(args[9], Arg::NestedCommand(_)));
}

#[test]
fn wrap_without_multiplex_has_no_control_args() {
    let resolver = test_resolver();
    let mut context = minimal_context();
    context.actions.push(ResolvedAction::Command(vec![Arg::Literal("echo".into())]));

    // gouda has ssh_multiplex=false
    resolver.resolve_wrap(&HostName::new("gouda"), &mut context).expect("resolve_wrap should succeed");

    let args = match &context.actions[0] {
        ResolvedAction::Command(args) => args,
        other => panic!("expected Command, got {other:?}"),
    };

    // ssh -t 'gouda.example.com' <nested> — no -o flags
    assert_eq!(args[0], Arg::Literal("ssh".into()));
    assert_eq!(args[1], Arg::Literal("-t".into()));
    assert_eq!(args[2], Arg::Quoted("gouda.example.com".into()));
    assert!(matches!(args[3], Arg::NestedCommand(_)));
    assert_eq!(args.len(), 4);
}

#[test]
fn wrap_user_at_host_target_format() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.actions.push(ResolvedAction::Command(vec![Arg::Literal("ls".into())]));

    // feta has user=Some("alice"), hostname="feta.local" -> "alice@feta.local"
    resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect("resolve_wrap should succeed");

    let args = match &context.actions[0] {
        ResolvedAction::Command(args) => args,
        other => panic!("expected Command, got {other:?}"),
    };
    assert_eq!(args[2], Arg::Quoted("alice@feta.local".into()));
}

#[test]
fn wrap_no_user_target_format() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.actions.push(ResolvedAction::Command(vec![Arg::Literal("ls".into())]));

    // gouda has user=None, hostname="gouda.example.com" -> "gouda.example.com"
    resolver.resolve_wrap(&HostName::new("gouda"), &mut context).expect("resolve_wrap should succeed");

    let args = match &context.actions[0] {
        ResolvedAction::Command(args) => args,
        other => panic!("expected Command, got {other:?}"),
    };
    assert_eq!(args[2], Arg::Quoted("gouda.example.com".into()));
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

#[test]
fn wrap_updates_current_host() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    assert_eq!(context.current_host.as_str(), "test-host");
    context.actions.push(ResolvedAction::Command(vec![Arg::Literal("ls".into())]));

    resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect("resolve_wrap should succeed");
    assert_eq!(context.current_host.as_str(), "feta");
}

// ── resolve_enter tests ─────────────────────────────────────────────

#[test]
fn enter_produces_ssh_command_and_sendkeys() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.working_directory = Some(PathBuf::from("/home/alice/dev/my-repo"));
    context.actions.push(ResolvedAction::Command(vec![
        Arg::Literal("cleat".into()),
        Arg::Literal("attach".into()),
        Arg::Literal("sess-1".into()),
    ]));

    resolver.resolve_enter(&HostName::new("feta"), &mut context).expect("resolve_enter should succeed");

    // Stack should have: [SendKeys, SSH Command] (SSH on top, SendKeys below)
    assert_eq!(context.actions.len(), 2);

    // Bottom: SendKeys with the flattened inner command
    match &context.actions[0] {
        ResolvedAction::SendKeys { steps } => {
            assert_eq!(steps.len(), 2);
            match &steps[0] {
                SendKeyStep::Type(text) => {
                    assert!(text.contains("cd"), "should include cd: {text}");
                    assert!(text.contains("/home/alice/dev/my-repo"), "should include dir: {text}");
                    assert!(text.contains("cleat attach sess-1"), "should include inner cmd: {text}");
                }
                other => panic!("expected Type step, got {other:?}"),
            }
            assert_eq!(steps[1], SendKeyStep::WaitForPrompt);
        }
        other => panic!("expected SendKeys, got {other:?}"),
    }

    // Top: SSH enter command (no inner command arg)
    match &context.actions[1] {
        ResolvedAction::Command(args) => {
            assert_eq!(args[0], Arg::Literal("ssh".into()));
            assert_eq!(args[1], Arg::Literal("-t".into()));
            assert_eq!(args[2], Arg::Quoted("alice@feta.local".into()));
            assert_eq!(args.len(), 3, "SSH enter command should not have a nested command arg");
        }
        other => panic!("expected Command, got {other:?}"),
    }
}

#[test]
fn enter_without_working_directory() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.actions.push(ResolvedAction::Command(vec![Arg::Literal("echo".into()), Arg::Quoted("hello".into())]));

    resolver.resolve_enter(&HostName::new("feta"), &mut context).expect("resolve_enter should succeed");

    assert_eq!(context.actions.len(), 2);

    // SendKeys should just have the inner command, no cd
    match &context.actions[0] {
        ResolvedAction::SendKeys { steps } => match &steps[0] {
            SendKeyStep::Type(text) => {
                assert!(!text.contains("cd"), "should not include cd: {text}");
                assert_eq!(text, "echo 'hello'");
            }
            other => panic!("expected Type step, got {other:?}"),
        },
        other => panic!("expected SendKeys, got {other:?}"),
    }
}

#[test]
fn enter_empty_command_no_sendkeys() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.actions.push(ResolvedAction::Command(vec![]));

    resolver.resolve_enter(&HostName::new("feta"), &mut context).expect("resolve_enter should succeed");

    // Only the SSH command, no SendKeys since there's nothing to type
    assert_eq!(context.actions.len(), 1);
    match &context.actions[0] {
        ResolvedAction::Command(args) => {
            assert_eq!(args[0], Arg::Literal("ssh".into()));
        }
        other => panic!("expected Command, got {other:?}"),
    }
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
    match &context.actions[0] {
        ResolvedAction::SendKeys { steps } => match &steps[0] {
            SendKeyStep::Type(text) => {
                assert_eq!(text, "cd '/remote/dir'");
            }
            other => panic!("expected Type step, got {other:?}"),
        },
        other => panic!("expected SendKeys, got {other:?}"),
    }
}

#[test]
fn enter_updates_current_host() {
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.actions.push(ResolvedAction::Command(vec![Arg::Literal("ls".into())]));

    resolver.resolve_enter(&HostName::new("feta"), &mut context).expect("resolve_enter should succeed");
    assert_eq!(context.current_host.as_str(), "feta");
}

// ── Regression: flatten output matches old wrap_remote_attach_commands ──

#[test]
fn regression_flatten_matches_old_ssh_wrap_pattern() {
    // The old code produced (for target="alice@feta.local", no multiplex,
    // dir="/home/alice/dev/my-repo", command="cleat attach sess-1"):
    //   ssh -t 'alice@feta.local' '$SHELL -l -c "cd '\''/home/alice/dev/my-repo'\'' && cleat attach sess-1"'
    //
    // The new Arg model with single-quote-at-all-depths produces:
    //   ssh -t 'alice@feta.local' '$SHELL -l -c '\''cd '\'\'\''/home/alice/dev/my-repo'\''\\'\'''\'' && cleat attach sess-1'\'''
    //
    // These are semantically equivalent: the remote shell receives the same
    // effective command. We verify the Arg tree structure produces the correct
    // flatten output matching the protocol's regression test.

    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.working_directory = Some(PathBuf::from("/home/alice/dev/my-repo"));
    context.actions.push(ResolvedAction::Command(vec![
        Arg::Literal("cleat".into()),
        Arg::Literal("attach".into()),
        Arg::Literal("sess-1".into()),
    ]));

    resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect("resolve_wrap should succeed");

    let args = match &context.actions[0] {
        ResolvedAction::Command(args) => args,
        other => panic!("expected Command, got {other:?}"),
    };

    let flat = flatten(args, 0);

    // Verify structural properties of the flattened output
    assert!(flat.starts_with("ssh -t "), "should start with ssh -t: {flat}");
    assert!(flat.contains("'alice@feta.local'"), "should contain quoted target: {flat}");
    assert!(flat.contains("$SHELL -l -c"), "should contain $SHELL -l -c: {flat}");
    assert!(flat.contains("/home/alice/dev/my-repo"), "should contain checkout dir: {flat}");
    assert!(flat.contains("cleat attach sess-1"), "should contain inner command: {flat}");

    // Verify the inner command can be traced through the nesting:
    // depth 2 (innermost): "cd '/home/alice/dev/my-repo' && cleat attach sess-1"
    // depth 1: "$SHELL -l -c '<quoted depth 2>'"
    // depth 0: "ssh -t 'alice@feta.local' '<quoted depth 1>'"
    //
    // The flatten function at protocol level already has regression tests for
    // this exact structure. Here we verify the resolver produces the right tree.
    let expected_args = vec![
        Arg::Literal("ssh".into()),
        Arg::Literal("-t".into()),
        Arg::Quoted("alice@feta.local".into()),
        Arg::NestedCommand(vec![
            Arg::Literal("$SHELL".into()),
            Arg::Literal("-l".into()),
            Arg::Literal("-c".into()),
            Arg::NestedCommand(vec![
                Arg::Literal("cd".into()),
                Arg::Quoted("/home/alice/dev/my-repo".into()),
                Arg::Literal("&&".into()),
                Arg::Literal("cleat".into()),
                Arg::Literal("attach".into()),
                Arg::Literal("sess-1".into()),
            ]),
        ]),
    ];

    assert_eq!(args, &expected_args, "Arg tree should match expected structure");
    assert_eq!(flatten(args, 0), flatten(&expected_args, 0));
}

#[test]
fn regression_flatten_empty_command_matches_login_shell_pattern() {
    // Old code for empty command: "cd '/dir' && exec $SHELL -l"
    let resolver = test_resolver_no_multiplex();
    let mut context = minimal_context();
    context.working_directory = Some(PathBuf::from("/home/alice/dev/my-repo"));
    context.actions.push(ResolvedAction::Command(vec![]));

    resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect("resolve_wrap should succeed");

    let args = match &context.actions[0] {
        ResolvedAction::Command(args) => args,
        other => panic!("expected Command, got {other:?}"),
    };

    let expected_args = vec![
        Arg::Literal("ssh".into()),
        Arg::Literal("-t".into()),
        Arg::Quoted("alice@feta.local".into()),
        Arg::NestedCommand(vec![
            Arg::Literal("$SHELL".into()),
            Arg::Literal("-l".into()),
            Arg::Literal("-c".into()),
            Arg::NestedCommand(vec![
                Arg::Literal("cd".into()),
                Arg::Quoted("/home/alice/dev/my-repo".into()),
                Arg::Literal("&&".into()),
                Arg::Literal("exec".into()),
                Arg::Literal("$SHELL".into()),
                Arg::Literal("-l".into()),
            ]),
        ]),
    ];

    assert_eq!(args, &expected_args, "empty command should produce login shell pattern");

    // Verify the flattened form contains the exec $SHELL -l pattern
    let flat = flatten(args, 0);
    assert!(flat.contains("exec $SHELL -l"), "flattened should contain exec $SHELL -l: {flat}");
}

#[test]
fn regression_multiplex_args_in_flatten() {
    let resolver = test_resolver();
    let mut context = minimal_context();
    context.actions.push(ResolvedAction::Command(vec![Arg::Literal("tmux".into()), Arg::Literal("attach".into())]));

    resolver.resolve_wrap(&HostName::new("feta"), &mut context).expect("resolve_wrap should succeed");

    let args = match &context.actions[0] {
        ResolvedAction::Command(args) => args,
        other => panic!("expected Command, got {other:?}"),
    };

    let flat = flatten(args, 0);
    assert!(flat.starts_with("ssh -t -o ControlMaster=auto -o "), "should have multiplex args: {flat}");
    assert!(flat.contains("ControlPersist=60"), "should have ControlPersist: {flat}");
    assert!(flat.contains("'alice@feta.local'"), "should have quoted target: {flat}");
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

    async fn ensure_session(&self, _session_name: &str, _command: &str, _cwd: &Path) -> Result<(), String> {
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
        Ok(vec![Arg::Literal("cleat".into()), Arg::Literal("attach".into()), Arg::Literal(session_name.to_string())])
    }

    async fn kill_session(&self, _session_name: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Helper: create an in-memory store with one terminal attachable pre-inserted.
fn store_with_terminal(attachable_id: &AttachableId, command: &str, cwd: &Path) -> SharedAttachableStore {
    use flotilla_protocol::HostName;

    use crate::attachable::{Attachable, AttachableSet};

    let store = shared_in_memory_attachable_store();
    {
        let mut s = store.lock().expect("lock");
        let set_id = s.allocate_set_id();
        s.insert_set(AttachableSet {
            id: set_id.clone(),
            host_affinity: Some(HostName::new("test-host")),
            checkout: None,
            template_identity: None,
            members: vec![attachable_id.clone()],
        });
        s.insert_attachable(Attachable {
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
    match &context.actions[0] {
        ResolvedAction::Command(args) => {
            assert_eq!(args[0], Arg::Literal("cleat".into()));
            assert_eq!(args[1], Arg::Literal("attach".into()));
            assert_eq!(args[2], Arg::Literal(att_id.to_string()));
        }
        other => panic!("expected Command, got {other:?}"),
    }

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
