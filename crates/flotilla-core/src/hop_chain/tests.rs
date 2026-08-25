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
fn docker_environment_wrap_supervises_and_reaps_the_exec_command() {
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

    let [ResolvedAction::Command(args)] = context.actions.as_slice() else { panic!("expected one supervised command") };
    assert_eq!(&args[..2], [Arg::Literal("sh".into()), Arg::Literal("-c".into())]);
    let Arg::Quoted(wrapper) = &args[2] else { panic!("wrapper must be a quoted shell program") };
    assert!(wrapper.contains("trap cleanup EXIT HUP INT TERM"));
    assert!(wrapper.contains("FLOTILLA_ATTACH_LEASE=$lease"));
    assert!(wrapper.contains("kill -KILL \"$pid\""));
    assert!(wrapper.contains("sleep 5"));
    assert_eq!(&args[3..], [
        Arg::Literal("flotilla-docker-attach".into()),
        Arg::Quoted("crew-container".into()),
        Arg::Quoted("/work/crew".into()),
        Arg::Quoted(super::environment::DOCKER_ATTACH_INNER_WRAPPER.into()),
        Arg::Literal("cleat".into()),
        Arg::Literal("attach".into()),
        Arg::Quoted("session".into()),
    ]);
}

#[cfg(target_os = "linux")]
#[test]
fn severing_supervised_docker_hop_reaps_lease_owner() {
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, process::CommandExt},
        process::Command,
        thread,
        time::{Duration, Instant},
    };

    let environment = EnvironmentId::new("crew-box");
    let resolver = DockerEnvironmentHopResolver::new(HashMap::from([(environment.clone(), "crew-container".to_string())]));
    let mut context = context();
    context.actions.push(ResolvedAction::Command(vec![Arg::Literal("cleat".into()), Arg::Literal("attach".into())]));
    resolver.resolve_wrap(&environment, &mut context).expect("known environment");
    let [ResolvedAction::Command(args)] = context.actions.as_slice() else { panic!("expected one supervised command") };
    let Arg::Quoted(wrapper) = &args[2] else { panic!("wrapper must be a quoted shell program") };

    let temp = tempfile::tempdir().expect("fake Docker directory");
    let docker = temp.path().join("docker");
    let observed_pid = temp.path().join("observed-pid");
    let calls = temp.path().join("calls");
    fs::write(
        &docker,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$FLOTILLA_TEST_CALLS"
if [ "$6" = flotilla-attach-cleanup ]; then
    exec sh -c "$5" "$6" "$7" "$8"
fi
lease=
pid_file=
for arg do
    case "$arg" in
        FLOTILLA_ATTACH_LEASE=*) lease=${arg#*=} ;;
        /tmp/flotilla-attach-*.pid) pid_file=$arg ;;
    esac
done
(trap '' HUP INT TERM; export FLOTILLA_ATTACH_LEASE="$lease"; exec sleep 300) &
pid=$!
printf '%s' "$pid" > "$pid_file"
while ! tr '\000' '\n' < "/proc/$pid/environ" | grep -Fqx "FLOTILLA_ATTACH_LEASE=$lease"; do
    sleep 0.01
done
printf '%s\n%s' "$pid" "$pid_file" > "$FLOTILLA_TEST_PID"
wait "$pid"
"#,
    )
    .expect("write fake Docker shim");
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).expect("make fake Docker shim executable");

    let path = format!("{}:{}", temp.path().display(), std::env::var("PATH").unwrap_or_default());
    let mut upstream = Command::new("sh");
    upstream
        .arg("-c")
        .arg(wrapper)
        .args(["flotilla-docker-attach", "crew-container", "", super::environment::DOCKER_ATTACH_INNER_WRAPPER, "cleat", "attach"])
        .env("PATH", path)
        .env("FLOTILLA_TEST_PID", &observed_pid)
        .env("FLOTILLA_TEST_CALLS", &calls)
        .process_group(0);
    let mut upstream = upstream.spawn().expect("start supervised attach hop");

    let deadline = Instant::now() + Duration::from_secs(2);
    while !observed_pid.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let observed = fs::read_to_string(&observed_pid).expect("fake in-container process should start");
    let mut observed = observed.lines();
    let attach_pid: i32 = observed.next().expect("attach pid").parse().expect("numeric attach pid");
    let pid_file = observed.next().expect("attach pidfile").to_string();

    // SAFETY: the child was placed in its own process group above.
    assert_eq!(unsafe { libc::kill(-(upstream.id() as i32), libc::SIGHUP) }, 0);
    let deadline = Instant::now() + Duration::from_secs(2);
    while upstream.try_wait().expect("poll upstream").is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(upstream.try_wait().expect("poll reaped upstream").is_some(), "upstream wrapper should exit after severance");

    let process_state = || {
        fs::read_to_string(format!("/proc/{attach_pid}/stat"))
            .ok()
            .and_then(|stat| stat.split_whitespace().nth(2).and_then(|state| state.chars().next()))
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_state().is_some_and(|state| state != 'Z') && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(process_state().is_none_or(|state| state == 'Z'), "lease owner should be dead");
    assert!(!std::path::Path::new(&pid_file).exists(), "cleanup should remove its pidfile");
    assert!(fs::read_to_string(calls).expect("fake Docker calls").contains("flotilla-attach-cleanup"));
}

#[cfg(unix)]
#[test]
fn docker_attach_lease_file_refuses_existing_symlink() {
    use std::{fs, os::unix::fs::symlink, process::Command};

    let temp = tempfile::tempdir().expect("lease test directory");
    let target = temp.path().join("target");
    let lease = temp.path().join("lease");
    fs::write(&target, "preserve me").expect("write symlink target");
    symlink(&target, &lease).expect("create hostile lease symlink");

    let status = Command::new("sh")
        .arg("-c")
        .arg(super::environment::DOCKER_ATTACH_INNER_WRAPPER)
        .arg("flotilla-attach")
        .arg(&lease)
        .arg("true")
        .status()
        .expect("run inner attach wrapper");

    assert!(!status.success(), "an existing lease path must be refused");
    assert_eq!(fs::read_to_string(target).expect("read symlink target"), "preserve me");
}

#[cfg(unix)]
#[test]
fn docker_attach_cleanup_is_bounded_when_transport_hangs() {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        process::Command,
        time::{Duration, Instant},
    };

    let environment = EnvironmentId::new("crew-box");
    let resolver = DockerEnvironmentHopResolver::new(HashMap::from([(environment.clone(), "crew-container".to_string())]));
    let mut context = context();
    context.actions.push(ResolvedAction::Command(Vec::new()));
    resolver.resolve_wrap(&environment, &mut context).expect("known environment");
    let [ResolvedAction::Command(args)] = context.actions.as_slice() else { panic!("expected one supervised command") };
    let Arg::Quoted(wrapper) = &args[2] else { panic!("wrapper must be a quoted shell program") };
    let wrapper = wrapper.replace("sleep 5", "sleep 0.05").replace("sleep 1", "sleep 0.05");

    let temp = tempfile::tempdir().expect("fake Docker directory");
    let docker = temp.path().join("docker");
    fs::write(
        &docker,
        r#"#!/bin/sh
if [ "$6" = flotilla-attach-cleanup ]; then
    trap '' HUP INT TERM
    exec sleep 300
fi
"#,
    )
    .expect("write hanging Docker shim");
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).expect("make fake Docker shim executable");
    let path = format!("{}:{}", temp.path().display(), std::env::var("PATH").unwrap_or_default());

    let started = Instant::now();
    let status = Command::new("sh")
        .arg("-c")
        .arg(wrapper)
        .args(["flotilla-docker-attach", "crew-container", "", super::environment::DOCKER_ATTACH_INNER_WRAPPER])
        .env("PATH", path)
        .status()
        .expect("run supervised attach with hanging cleanup");

    assert!(status.success(), "cleanup timeout should preserve the attach command status");
    assert!(started.elapsed() < Duration::from_secs(2), "cleanup watchdog should bound teardown delay");
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
    assert_eq!(docker[0], Arg::Literal("sh".into()));
    assert_eq!(docker[1], Arg::Literal("-c".into()));
    assert_eq!(docker[4], Arg::Quoted("crew-container".into()));
    assert_eq!(docker[7..], [Arg::Literal("cleat".into()), Arg::Literal("attach".into()), Arg::Quoted("session".into())]);
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
