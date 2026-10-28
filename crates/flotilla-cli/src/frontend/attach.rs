use crate::terminal::client_attach_plan;

use color_eyre::Result;
use flotilla_config::ConfigStore;
use flotilla_protocol::{commands::CommandValue, output::OutputFormat, Command, CommandAction, RepoSelector};

use crate::cli::args::Cli;

use super::connection::{connect_daemon, reset_sigpipe};
use super::targets::{resolve_cli_repository, resolve_optional_cwd_repository, resolve_repo_from_env};

pub async fn run_attach(
    cli: &Cli,
    reference: &str,
    mode: flotilla_protocol::commands::AttachMode,
    transient: bool,
    host: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    reset_sigpipe();
    let code = attach_until_settled(|reattaching| {
        async move {
            let resolved = resolve_attach(cli, reference, mode, transient, host).await;
            let (plan, binding) = match resolved {
                Ok(CommandValue::AttachCommandResolved { plan, binding }) => (plan, binding),
                Ok(CommandValue::Error { message }) => match format {
                    OutputFormat::Json => {
                        println!("{}", flotilla_protocol::output::json_pretty(&CommandValue::Error { message: message.clone() }));
                        return Err(color_eyre::eyre::eyre!(message));
                    }
                    OutputFormat::Human => {
                        eprintln!("{message}");
                        std::process::exit(1);
                    }
                },
                Ok(other) => return Err(color_eyre::eyre::eyre!("unexpected attach response: {other:?}")),
                // A daemon that is restarting is part of the roll being
                // survived: wait for it, saying why. The first resolution
                // fails as before.
                Err(error) if reattaching => {
                    eprintln!("flotilla: cannot resolve {reference} yet: {error}");
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            let plan = client_attach_plan(
                &cli.daemon_endpoint().map_err(color_eyre::eyre::Report::msg)?,
                plan,
                binding.as_ref(),
                reference,
                mode,
                || {
                    let paths = cli.client_paths()?;
                    ConfigStore::with_base(&paths.config_dir).load_hosts()
                },
            )
            .map_err(color_eyre::eyre::Report::msg)?;
            if format == OutputFormat::Json {
                println!("{}", flotilla_protocol::output::json_pretty(&CommandValue::AttachCommandResolved { plan, binding }));
                std::process::exit(0);
            }
            if let Some(phase) = binding.as_ref().and_then(|binding| binding.convoy_phase) {
                eprintln!("Convoy phase: {phase}");
            }
            if !transient && !reattaching {
                stamp_pane_identity(reference, binding.as_ref()).await;
            }
            Ok(Some(plan))
        }
    })
    .await?;
    std::process::exit(code.unwrap_or(1));
}

/// Resolve, run and, when cleat asks for it, resolve again. `resolve` gets
/// whether this is a reattach and yields `None` when the daemon is not there
/// yet. Returns the status of the attach that ended for any other reason.
pub async fn attach_until_settled<F>(mut resolve: impl FnMut(bool) -> F) -> Result<Option<i32>>
where
    F: std::future::Future<Output = Result<Option<flotilla_protocol::ResolvedAttachPlan>>>,
{
    let mut failures = 0u32;
    let mut reattaching = false;
    loop {
        let Some(plan) = resolve(reattaching).await? else {
            failures += 1;
            eprintln!("flotilla: retrying");
            tokio::time::sleep(reattach_backoff(failures)).await;
            continue;
        };
        let started = std::time::Instant::now();
        // The attach blocks for its whole life; keep it off the executor.
        let code = tokio::task::spawn_blocking(move || crate::terminal::run_attach_plan_waiting(&plan))
            .await?
            .map_err(color_eyre::eyre::Report::msg)?;
        if code != Some(CLEAT_REATTACH_STATUS) {
            return Ok(code);
        }
        // A path that dies as soon as it is made must back off, not spin:
        // short-lived attaches ratchet the wait up to its cap.
        failures = if started.elapsed() > REATTACH_STABLE { 0 } else { failures + 1 };
        eprintln!("flotilla: attachment moved or its path broke; reattaching");
        reattaching = true;
        tokio::time::sleep(reattach_backoff(failures)).await;
    }
}

/// cleat's exit status (EX_TEMPFAIL) for "the session moved or the path to it
/// broke: resolve it again". Every other status ends the attach as it did.
pub const CLEAT_REATTACH_STATUS: i32 = 75;
const REATTACH_STABLE: std::time::Duration = std::time::Duration::from_secs(10);

fn reattach_backoff(failures: u32) -> std::time::Duration {
    std::time::Duration::from_millis((250u64 << failures.min(5)).min(5_000))
}

async fn resolve_attach(
    cli: &Cli,
    reference: &str,
    mode: flotilla_protocol::commands::AttachMode,
    transient: bool,
    host: Option<&str>,
) -> Result<CommandValue> {
    let daemon = connect_daemon(cli).await?;
    let context_repo = match resolve_repo_from_env(cli) {
        Some(repo) => Some(repo),
        None => std::env::current_dir().ok().map(RepoSelector::Path),
    };
    let context_repo = match context_repo {
        Some(RepoSelector::Path(path)) => resolve_optional_cwd_repository(&*daemon, path).await.map_err(color_eyre::eyre::Report::msg)?,
        Some(selector) => Some(resolve_cli_repository(&*daemon, selector).await.map_err(color_eyre::eyre::Report::msg)?),
        None => None,
    };
    daemon
        .execute_query(
            Command {
                node_id: None,
                provisioning_target: None,
                context_repo,
                action: if transient {
                    CommandAction::AttachTransient {
                        reference: reference.to_string(),
                        host: host.map(flotilla_protocol::HostName::new),
                        mode,
                    }
                } else {
                    CommandAction::Attach { reference: reference.to_string(), host: host.map(flotilla_protocol::HostName::new), mode }
                },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .map_err(|e| color_eyre::eyre::eyre!(e))
}

/// Publish pane ≙ identity into the enclosing PM's metadata plane before
/// launching the attach command — the one moment a process knows the binding
/// (flotilla-org/flotilla#708, half 1). Best-effort: a PM-less or failed
/// stamp never blocks the attach.
pub async fn stamp_pane_identity(reference: &str, binding: Option<&flotilla_protocol::AttachBinding>) {
    use flotilla_manifest::{pm::PmInstance, stamp::pane_stamp};
    let Some(pm) = PmInstance::detect(&|key| std::env::var(key).ok()) else {
        return;
    };
    let Some(pane) = pm.current_pane() else {
        return;
    };
    if let Err(error) = pm.sink().send(&pane_stamp(pane, reference, binding)).await {
        eprintln!("warning: could not stamp pane identity: {error}");
    }
}

pub fn run_attach_plan(plan: &flotilla_protocol::ResolvedAttachPlan) -> Result<()> {
    crate::terminal::exec_attach_plan(plan).map(|never| match never {}).map_err(color_eyre::eyre::Report::msg)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use flotilla_protocol::{arg::Arg, ResolvedAttachAction, ResolvedAttachPlan};

    use super::attach_until_settled;

    fn exits_with(status: i32) -> ResolvedAttachPlan {
        ResolvedAttachPlan(vec![ResolvedAttachAction::Command(
            ["sh", "-c", &format!("exit {status}")].into_iter().map(|part| Arg::Literal(part.to_string())).collect(),
        )])
    }

    // cleat's status is the whole contract: 75 resolves again, anything else
    // (a deliberate detach, the session ending, a refusal) ends the attach.
    async fn run(statuses: &[Option<i32>]) -> (Option<i32>, Vec<bool>) {
        let calls = RefCell::new(Vec::new());
        let code = attach_until_settled(|reattaching| {
            let index = calls.borrow().len();
            calls.borrow_mut().push(reattaching);
            let next = statuses[index];
            async move { Ok(next.map(exits_with)) }
        })
        .await
        .expect("attach settles");
        (code, calls.into_inner())
    }

    #[tokio::test(start_paused = true)]
    async fn reattach_status_resolves_again_with_the_reattach_flag() {
        let (code, calls) = run(&[Some(75), Some(75), Some(0)]).await;
        assert_eq!(code, Some(0));
        assert_eq!(calls, [false, true, true]);
    }

    #[tokio::test(start_paused = true)]
    async fn other_statuses_end_the_attach_unchanged() {
        for status in [0, 1, 2, 74, 76, 130] {
            let (code, calls) = run(&[Some(status)]).await;
            assert_eq!(code, Some(status));
            assert_eq!(calls, [false]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_unavailable_daemon_is_waited_for_after_a_reattach() {
        let (code, calls) = run(&[Some(75), None, None, Some(3)]).await;
        assert_eq!(code, Some(3));
        assert_eq!(calls, [false, true, true, true]);
    }

    // Real cleat, no fake output: a session handed to another daemon makes the
    // attached client exit 75, and the attach resolves it again. Needs the
    // pinned new cleat (FLOTILLA_TEST_CLEAT_NEW, see ci/cleat-environment).
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "private native crew subprocess only"]
    fn native_crew_probe() {
        flotilla_discovery_testkit::native_terminal::crew_probe();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "needs the pinned new cleat: FLOTILLA_TEST_CLEAT_NEW"]
    async fn real_cleat_handover_makes_the_attach_resolve_again() {
        use flotilla_discovery_testkit::native_terminal::NativeTerminal;

        let cleat = std::env::var("FLOTILLA_TEST_CLEAT_NEW").expect("pinned new cleat");
        let bin = std::path::Path::new(&cleat).parent().expect("cleat directory").to_string_lossy().into_owned();
        let path = format!("{bin}:{}", std::env::var("PATH").unwrap_or_default());
        // SAFETY: the CI filter by name runs this ignored test alone in its process.
        unsafe { std::env::set_var("PATH", &path) };
        let mut terminal = NativeTerminal::new("frontend::attach::tests::native_crew_probe");
        let session = terminal.endpoint.session.clone();
        // The vessel form: a socket attachment cannot follow a redirect to a
        // host-local path, so handover must end it with 75.
        let attach = |socket: &std::path::Path| {
            let mut args = vec![Arg::Literal("env".into()), Arg::Literal("-u".into()), Arg::Literal("CLEAT_DAEMON".into())];
            for (key, value) in &terminal.baseline {
                args.push(Arg::EnvAssignment { key: key.clone(), value: value.clone() });
            }
            // The attach needs a terminal; python's pty module supplies one and
            // passes the client's exit status through.
            for part in [
                "python3",
                "-c",
                "import os,pty,sys; sys.exit(os.waitstatus_to_exitcode(pty.spawn(sys.argv[1:])))",
                "cleat",
                "attach",
                session.as_str(),
                "--socket",
                &socket.to_string_lossy(),
            ] {
                args.push(Arg::Literal(part.into()));
            }
            ResolvedAttachPlan(vec![ResolvedAttachAction::Command(args)])
        };
        let root = std::path::PathBuf::from(&terminal.endpoint.runtime_root);
        let before = root.join(&terminal.endpoint.daemon).join("socket");
        let after = root.join("moved@1").join("socket");
        let calls = RefCell::new(0);
        let mover = std::thread::spawn({
            let session = session.clone();
            let baseline = terminal.baseline.clone();
            move || {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let run = |args: &[&str]| {
                    std::process::Command::new("cleat").args(args).env_clear().envs(&baseline).output().expect("cleat runs")
                };
                assert!(run(&["transfer", &session, "--to", "moved"]).status.success(), "handover");
                std::thread::sleep(std::time::Duration::from_secs(2));
                run(&["kill", &session]);
            }
        });
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            attach_until_settled(|_| {
                let first = *calls.borrow() == 0;
                *calls.borrow_mut() += 1;
                let plan = attach(if first { &before } else { &after });
                async move { Ok(Some(plan)) }
            }),
        )
        .await
        .expect("attach settled within the deadline")
        .expect("attach settles");
        mover.join().expect("mover");
        assert!(*calls.borrow() >= 2, "handover must make the attach resolve again (status {code:?})");
        terminal.shutdown();
    }
}
