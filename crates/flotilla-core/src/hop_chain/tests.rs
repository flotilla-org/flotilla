use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use flotilla_protocol::{
    arg::{flatten, Arg},
    EnvironmentId, HostName,
};

use super::{
    environment::{DockerEnvironmentHopResolver, EnvironmentHopResolver, NoopEnvironmentHopResolver},
    remote::{RemoteHopResolver, SshRemoteHopResolver},
    resolver::HopResolver,
    terminal::{NoopTerminalHopResolver, TerminalHopResolver},
    Hop, HopPlan, ResolutionContext, ResolvedAction,
};
use crate::{
    attachable::AttachableId,
    config::{HostsConfig, RemoteHostConfig, SshConfig},
    path_context::{DaemonHostPath, ExecutionEnvironmentPath},
};

fn context() -> ResolutionContext {
    ResolutionContext {
        current_host: HostName::new("kiwi"),
        current_environment: None,
        working_directory: None,
        actions: Vec::new(),
        nesting_depth: 0,
    }
}

fn command(action: &ResolvedAction) -> &[Arg] {
    let ResolvedAction::Command(args) = action;
    args
}

fn nested(arg: &Arg) -> &[Arg] {
    let Arg::NestedCommand(args) = arg else { panic!("expected nested command, got {arg:?}") };
    args
}

fn ssh_resolver() -> SshRemoteHopResolver {
    let hosts = HashMap::from([
        ("udder".to_string(), RemoteHostConfig {
            hostname: "udder.example".to_string(),
            expected_host_name: "udder".to_string(),
            expected_node_id: None,
            user: Some("alice".to_string()),
            ssh_multiplex: Some(false),
        }),
        ("jump".to_string(), RemoteHostConfig {
            hostname: "jump.example".to_string(),
            expected_host_name: "jump".to_string(),
            expected_node_id: None,
            user: None,
            ssh_multiplex: Some(false),
        }),
    ]);
    SshRemoteHopResolver::new(DaemonHostPath::new(std::env::temp_dir().join("flotilla-hop-chain-tests")), HostsConfig {
        ssh: SshConfig { multiplex: false },
        hosts,
    })
}

#[test]
fn one_hop_ssh_runs_recursive_attach_as_the_remote_command() {
    let args = ssh_resolver()
        .one_hop_command_args(&HostName::new("jump"), vec![
            Arg::Literal("flotilla".into()),
            Arg::Literal("attach".into()),
            Arg::Literal("--host".into()),
            Arg::Quoted("udder".into()),
            Arg::Quoted("governor".into()),
        ])
        .expect("known next hop");

    assert_eq!(args[0], Arg::Literal("ssh".into()));
    assert_eq!(args[1], Arg::Literal("-t".into()));
    assert_eq!(args[2], Arg::Quoted("jump.example".into()));
    let login = nested(&args[3]);
    assert_eq!(login[..3], [Arg::Literal("${SHELL:-/bin/sh}".into()), Arg::Literal("-l".into()), Arg::Literal("-c".into())]);
    assert_eq!(nested(&login[3]), [
        Arg::Literal("flotilla".into()),
        Arg::Literal("attach".into()),
        Arg::Literal("--host".into()),
        Arg::Quoted("udder".into()),
        Arg::Quoted("governor".into()),
    ]);
}

#[test]
fn remote_wrap_preserves_working_directory_and_inner_command() {
    let resolver = ssh_resolver();
    let mut context = context();
    context.working_directory = Some(ExecutionEnvironmentPath::new("/work/crew"));
    context.actions.push(ResolvedAction::Command(vec![
        Arg::Literal("cleat".into()),
        Arg::Literal("attach".into()),
        Arg::Quoted("session".into()),
    ]));

    resolver.resolve_wrap(&HostName::new("udder"), &mut context).expect("known host");

    assert_eq!(context.actions.len(), 1);
    let rendered = flatten(command(&context.actions[0]), 0);
    assert!(rendered.starts_with("ssh -t 'alice@udder.example'"), "{rendered}");
    assert!(rendered.contains("cd"), "{rendered}");
    assert!(rendered.contains("/work/crew"), "{rendered}");
    assert!(rendered.contains("cleat attach"), "{rendered}");
}

#[test]
fn unknown_remote_hop_names_the_host() {
    let error = ssh_resolver()
        .one_hop_command_args(&HostName::new("missing"), vec![Arg::Literal("flotilla".into())])
        .expect_err("unknown host should fail");

    assert_eq!(error, "unknown remote host: missing");
}

#[test]
fn docker_environment_wrap_is_one_direct_exec_command() {
    let environment = EnvironmentId::new("crew-box");
    let resolver = DockerEnvironmentHopResolver::new(HashMap::from([(environment.clone(), "crew-container".to_string())]));
    let mut context = context();
    context.working_directory = Some(ExecutionEnvironmentPath::new("/work/crew"));
    context.actions.push(ResolvedAction::Command(vec![
        Arg::Literal("cleat".into()),
        Arg::Literal("attach".into()),
        Arg::Quoted("session".into()),
    ]));

    resolver.resolve_wrap(&environment, &mut context).expect("known environment");

    assert_eq!(context.actions, [ResolvedAction::Command(vec![
        Arg::Literal("docker".into()),
        Arg::Literal("exec".into()),
        Arg::Literal("-it".into()),
        Arg::Literal("-w".into()),
        Arg::Quoted("/work/crew".into()),
        Arg::Quoted("crew-container".into()),
        Arg::Literal("cleat".into()),
        Arg::Literal("attach".into()),
        Arg::Quoted("session".into()),
    ])]);
}

#[derive(Default)]
struct RecordingRemote {
    calls: Mutex<Vec<HostName>>,
}

impl RemoteHopResolver for RecordingRemote {
    fn resolve_wrap(&self, host: &HostName, context: &mut ResolutionContext) -> Result<(), String> {
        self.calls.lock().expect("calls lock").push(host.clone());
        let ResolvedAction::Command(inner) = context.actions.pop().ok_or("missing inner command")?;
        context.actions.push(ResolvedAction::Command(vec![
            Arg::Literal("ssh".into()),
            Arg::Quoted(host.to_string()),
            Arg::NestedCommand(inner),
        ]));
        Ok(())
    }
}

#[derive(Default)]
struct RecordingTerminal {
    calls: Mutex<Vec<AttachableId>>,
}

impl TerminalHopResolver for RecordingTerminal {
    fn resolve(&self, attachable_id: &AttachableId, context: &mut ResolutionContext) -> Result<(), String> {
        self.calls.lock().expect("calls lock").push(attachable_id.clone());
        context.actions.push(ResolvedAction::Command(vec![
            Arg::Literal("cleat".into()),
            Arg::Literal("attach".into()),
            Arg::Quoted(attachable_id.to_string()),
        ]));
        Ok(())
    }
}

#[test]
fn resolver_composes_command_execution_inside_out() {
    let environment = EnvironmentId::new("crew-box");
    let attachable = AttachableId::new("session");
    let remote = Arc::new(RecordingRemote::default());
    let terminal = Arc::new(RecordingTerminal::default());
    let resolver = HopResolver::new(
        remote.clone(),
        Arc::new(DockerEnvironmentHopResolver::new(HashMap::from([(environment.clone(), "crew-container".to_string())]))),
        terminal.clone(),
    );
    let plan = HopPlan(vec![
        Hop::RemoteToHost { host: HostName::new("udder") },
        Hop::EnterEnvironment { env_id: environment.clone(), provider: "docker".into() },
        Hop::AttachTerminal { attachable_id: attachable.clone() },
    ]);
    let mut context = context();

    let resolved = resolver.resolve(&plan, &mut context).expect("chain should resolve");

    assert_eq!(resolved.0.len(), 1);
    let outer = command(&resolved.0[0]);
    assert_eq!(outer[0], Arg::Literal("ssh".into()));
    let docker = nested(&outer[2]);
    assert_eq!(docker[..4], [
        Arg::Literal("docker".into()),
        Arg::Literal("exec".into()),
        Arg::Literal("-it".into()),
        Arg::Quoted("crew-container".into()),
    ]);
    assert_eq!(docker[4..], [Arg::Literal("cleat".into()), Arg::Literal("attach".into()), Arg::Quoted("session".into())]);
    assert_eq!(*remote.calls.lock().expect("calls lock"), [HostName::new("udder")]);
    assert_eq!(*terminal.calls.lock().expect("calls lock"), [attachable]);
    assert_eq!(context.nesting_depth, 2);
}

#[test]
fn resolver_collapses_local_host_and_current_environment() {
    let environment = EnvironmentId::new("crew-box");
    let resolver = HopResolver::new(
        Arc::new(super::remote::NoopRemoteHopResolver),
        Arc::new(NoopEnvironmentHopResolver),
        Arc::new(NoopTerminalHopResolver),
    );
    let plan = HopPlan(vec![
        Hop::RemoteToHost { host: HostName::new("kiwi") },
        Hop::EnterEnvironment { env_id: environment.clone(), provider: "docker".into() },
        Hop::RunCommand { command: vec![Arg::Literal("true".into())] },
    ]);
    let mut context = context();
    context.current_environment = Some(environment);

    let resolved = resolver.resolve(&plan, &mut context).expect("already-local hops collapse");

    assert_eq!(resolved.0, [ResolvedAction::Command(vec![Arg::Literal("true".into())])]);
    assert_eq!(context.nesting_depth, 0);
}

#[test]
fn missing_environment_adapter_names_the_environment() {
    let resolver = HopResolver::new(
        Arc::new(super::remote::NoopRemoteHopResolver),
        Arc::new(NoopEnvironmentHopResolver),
        Arc::new(NoopTerminalHopResolver),
    );
    let plan = HopPlan(vec![Hop::EnterEnvironment { env_id: EnvironmentId::new("missing"), provider: "docker".into() }, Hop::RunCommand {
        command: vec![Arg::Literal("true".into())],
    }]);

    let error = resolver.resolve(&plan, &mut context()).expect_err("missing adapter should fail");

    assert_eq!(error, "no environment transport available for environment: missing");
}
