use std::sync::Arc;

use flotilla_protocol::{
    arg::{flatten, Arg},
    EnvironmentId, HostName,
};

use super::{
    remote::{RemoteHopResolver, SshRemoteHopResolver},
    resolver::HopResolver,
    resolver::NoopEnvironmentHopResolver,
    Hop, HopPlan, ResolutionContext, ResolvedAction,
};
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_paths::path_context::ExecutionEnvironmentPath;

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
    SshRemoteHopResolver::new(
        DaemonHostPath::new(std::env::temp_dir().join("flotilla-hop-chain-tests")),
        vec![
            (HostName::new("udder"), super::remote::SshConnection { target: "alice@udder.example".into(), multiplex: false }),
            (HostName::new("jump"), super::remote::SshConnection { target: "jump.example".into(), multiplex: false }),
        ],
    )
}

#[test]
fn one_hop_ssh_runs_recursive_attach_as_the_remote_command() {
    let args = ssh_resolver()
        .one_hop_command_args(
            &HostName::new("jump"),
            vec![
                Arg::Literal("flotilla".into()),
                Arg::Literal("attach".into()),
                Arg::Literal("--host".into()),
                Arg::Quoted("udder".into()),
                Arg::Quoted("governor".into()),
            ],
        )
        .expect("known next hop");

    assert_eq!(args[0], Arg::Literal("ssh".into()));
    assert_eq!(args[1], Arg::Literal("-t".into()));
    assert_eq!(args[2], Arg::Quoted("jump.example".into()));
    let login = nested(&args[3]);
    assert_eq!(login[..3], [Arg::Literal("${SHELL:-/bin/sh}".into()), Arg::Literal("-l".into()), Arg::Literal("-c".into())]);
    assert_eq!(
        nested(&login[3]),
        [
            Arg::Literal("flotilla".into()),
            Arg::Literal("attach".into()),
            Arg::Literal("--host".into()),
            Arg::Quoted("udder".into()),
            Arg::Quoted("governor".into()),
        ]
    );
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
fn resolver_collapses_local_host_and_current_environment() {
    let environment = EnvironmentId::new("crew-box");
    let resolver = HopResolver::new(Arc::new(super::remote::NoopRemoteHopResolver), Arc::new(NoopEnvironmentHopResolver));
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
    let resolver = HopResolver::new(Arc::new(super::remote::NoopRemoteHopResolver), Arc::new(NoopEnvironmentHopResolver));
    let plan = HopPlan(vec![
        Hop::EnterEnvironment { env_id: EnvironmentId::new("missing"), provider: "docker".into() },
        Hop::RunCommand { command: vec![Arg::Literal("true".into())] },
    ]);

    let error = resolver.resolve(&plan, &mut context()).expect_err("missing adapter should fail");

    assert_eq!(error, "no environment transport available for environment: missing");
}

// Resolved connections preserve first-match host lookup and multiplex selection.
// Generate both multiplex choices, absent/present/duplicate hosts and user targets.
#[hegel::test]
fn resolved_ssh_connections_preserve_lookup_and_multiplex(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
    let multiplex = tc.draw(gs::booleans());
    let user = tc.draw(gs::booleans());
    let host = HostName::new("remote");
    let target = if user { "alice@remote.example" } else { "remote.example" };
    let connections = (0..count)
        .map(|index| {
            (
                host.clone(),
                super::remote::SshConnection { target: if index == 0 { target.into() } else { "duplicate.example".into() }, multiplex },
            )
        })
        .collect();
    let dir = tempfile::tempdir().expect("config base");
    let resolver = SshRemoteHopResolver::new(DaemonHostPath::new(dir.path()), connections);
    let result = resolver.one_hop_command_args(&host, vec![Arg::Literal("true".into())]);
    if count == 0 {
        assert_eq!(result.expect_err("missing host"), "unknown remote host: remote");
    } else {
        let args = result.expect("resolved host");
        assert!(args.contains(&Arg::Quoted(target.into())));
        assert!(!args.contains(&Arg::Quoted("duplicate.example".into())));
        assert_eq!(args.contains(&Arg::Literal("ControlMaster=auto".into())), multiplex);
    }
}
