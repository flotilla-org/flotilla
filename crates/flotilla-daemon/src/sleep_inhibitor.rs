use std::{collections::BTreeSet, process::Stdio, time::Duration};

use async_trait::async_trait;
use flotilla_resources::{Convoy, ConvoyPhase, ResourceError, ResourceObject, TypedResolver, WatchEvent, WatchStart};
use futures::StreamExt;
use tokio::process::{Child, Command};
use tracing::{info, warn};

const RECHECK_INTERVAL: Duration = Duration::from_secs(30);
const INHIBITOR_REASON: &str = "Flotilla vessel crew is active";

#[async_trait]
trait SleepInhibitor: Send {
    async fn maintain(&mut self, required: bool) -> Result<(), String>;
}

pub(super) async fn run(convoys: TypedResolver<Convoy>) -> Result<(), ResourceError> {
    run_with_inhibitor(convoys, SystemSleepInhibitor::default()).await
}

async fn run_with_inhibitor<I: SleepInhibitor>(convoys: TypedResolver<Convoy>, mut inhibitor: I) -> Result<(), ResourceError> {
    let listed = convoys.list().await?;
    let mut active = ActiveConvoys::from_objects(&listed.items);
    let mut last_error = None;
    maintain_and_log(&mut inhibitor, active.required(), &mut last_error).await;

    let mut watch = convoys.watch(WatchStart::resuming_from(&listed)).await?;
    let mut recheck = tokio::time::interval(RECHECK_INTERVAL);
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    recheck.tick().await;

    loop {
        tokio::select! {
            event = watch.next() => {
                match event {
                    Some(Ok(event)) => {
                        active.apply(event);
                        maintain_and_log(&mut inhibitor, active.required(), &mut last_error).await;
                    }
                    Some(Err(error)) => return Err(error),
                    None => return Err(ResourceError::other("sleep inhibitor convoy watch ended")),
                }
            }
            _ = recheck.tick() => {
                maintain_and_log(&mut inhibitor, active.required(), &mut last_error).await;
            }
        }
    }
}

async fn maintain_and_log(inhibitor: &mut impl SleepInhibitor, required: bool, last_error: &mut Option<String>) {
    match inhibitor.maintain(required).await {
        Ok(()) => *last_error = None,
        Err(error) if last_error.as_ref() == Some(&error) => {}
        Err(error) => {
            warn!(%error, required, "failed to update system sleep inhibitor");
            *last_error = Some(error);
        }
    }
}

#[derive(Default)]
struct ActiveConvoys {
    names: BTreeSet<String>,
}

impl ActiveConvoys {
    fn from_objects(objects: &[ResourceObject<Convoy>]) -> Self {
        let names = objects.iter().filter(|convoy| convoy_requires_inhibitor(convoy)).map(|convoy| convoy.metadata.name.clone()).collect();
        Self { names }
    }

    fn apply(&mut self, event: WatchEvent<Convoy>) {
        match event {
            WatchEvent::Added(convoy) | WatchEvent::Modified(convoy) => {
                if convoy_requires_inhibitor(&convoy) {
                    self.names.insert(convoy.metadata.name);
                } else {
                    self.names.remove(&convoy.metadata.name);
                }
            }
            WatchEvent::Deleted(convoy) => {
                self.names.remove(&convoy.metadata.name);
            }
        }
    }

    fn required(&self) -> bool {
        !self.names.is_empty()
    }
}

fn convoy_requires_inhibitor(convoy: &ResourceObject<Convoy>) -> bool {
    !matches!(
        convoy.status.as_ref().map(|status| status.phase),
        Some(ConvoyPhase::Completed | ConvoyPhase::Failed | ConvoyPhase::Cancelled | ConvoyPhase::Abandoned)
    )
}

#[derive(Default)]
struct SystemSleepInhibitor {
    child: Option<Child>,
}

#[async_trait]
impl SleepInhibitor for SystemSleepInhibitor {
    async fn maintain(&mut self, required: bool) -> Result<(), String> {
        if let Some(child) = self.child.as_mut() {
            match child.try_wait().map_err(|error| format!("check sleep inhibitor process: {error}"))? {
                Some(status) => {
                    warn!(%status, "system sleep inhibitor process exited; reacquiring if still required");
                    self.child = None;
                }
                None if required => return Ok(()),
                None => {}
            }
        }

        if required {
            self.acquire()
        } else {
            self.release().await
        }
    }
}

impl SystemSleepInhibitor {
    fn acquire(&mut self) -> Result<(), String> {
        let command_spec = platform_inhibitor_command(std::process::id())?;
        let mut command = Command::new(command_spec.program);
        command.args(&command_spec.args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
        let child = command.spawn().map_err(|error| format!("start {}: {error}", command_spec.program))?;
        self.child = Some(child);
        info!(program = command_spec.program, reason = INHIBITOR_REASON, "acquired system sleep inhibitor");
        Ok(())
    }

    async fn release(&mut self) -> Result<(), String> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        child.kill().await.map_err(|error| format!("stop sleep inhibitor process: {error}"))?;
        info!("released system sleep inhibitor");
        Ok(())
    }
}

struct InhibitorCommand {
    program: &'static str,
    args: Vec<String>,
}

#[cfg(target_os = "linux")]
fn platform_inhibitor_command(daemon_pid: u32) -> Result<InhibitorCommand, String> {
    Ok(InhibitorCommand {
        program: "systemd-inhibit",
        args: vec![
            "--what=sleep".to_string(),
            "--who=flotillad".to_string(),
            format!("--why={INHIBITOR_REASON}"),
            "--mode=block".to_string(),
            "tail".to_string(),
            format!("--pid={daemon_pid}"),
            "-f".to_string(),
            "/dev/null".to_string(),
        ],
    })
}

#[cfg(target_os = "macos")]
fn platform_inhibitor_command(daemon_pid: u32) -> Result<InhibitorCommand, String> {
    Ok(InhibitorCommand { program: "caffeinate", args: vec!["-i".to_string(), "-w".to_string(), daemon_pid.to_string()] })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_inhibitor_command(_daemon_pid: u32) -> Result<InhibitorCommand, String> {
    Err("system sleep inhibition is unsupported on this platform".to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use flotilla_protocol::PrincipalRef;
    use flotilla_resources::{ConvoySpec, ConvoyStatus, InMemoryBackend, InputMeta, ResourceBackend};
    use tokio::sync::mpsc;

    use super::*;

    const NAMESPACE: &str = "flotilla";

    struct RecordingInhibitor {
        states: mpsc::UnboundedSender<bool>,
    }

    #[async_trait]
    impl SleepInhibitor for RecordingInhibitor {
        async fn maintain(&mut self, required: bool) -> Result<(), String> {
            self.states.send(required).map_err(|_| "test receiver closed".to_string())
        }
    }

    fn convoy_spec() -> ConvoySpec {
        ConvoySpec {
            workflow_ref: "single-agent-contained".to_string(),
            dispatching_principal_ref: PrincipalRef::default(),
            inputs: BTreeMap::new(),
            placement_policy: None,
            repositories: vec![],
            r#ref: None,
            project_ref: None,
            adopted_checkout_refs: BTreeMap::new(),
            issues: vec![],
            instruction: None,
        }
    }

    async fn create_convoy(convoys: &TypedResolver<Convoy>, name: &str, phase: ConvoyPhase) -> ResourceObject<Convoy> {
        let created = convoys.create(&InputMeta::builder().name(name.to_string()).build(), &convoy_spec()).await.expect("create convoy");
        convoys
            .update_status(name, &created.metadata.resource_version, &ConvoyStatus { phase, ..ConvoyStatus::default() })
            .await
            .expect("update convoy status")
    }

    async fn next_state(states: &mut mpsc::UnboundedReceiver<bool>) -> bool {
        tokio::time::timeout(Duration::from_secs(1), states.recv())
            .await
            .expect("timed out waiting for inhibitor state")
            .expect("inhibitor state channel closed")
    }

    #[tokio::test]
    async fn startup_reacquires_for_an_existing_active_convoy() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let convoys = backend.using::<Convoy>(NAMESPACE);
        create_convoy(&convoys, "active", ConvoyPhase::Active).await;
        let (tx, mut rx) = mpsc::unbounded_channel();

        let task = tokio::spawn(run_with_inhibitor(convoys, RecordingInhibitor { states: tx }));

        assert!(next_state(&mut rx).await);
        task.abort();
    }

    #[tokio::test]
    async fn releases_only_after_the_last_convoy_becomes_terminal() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let convoys = backend.using::<Convoy>(NAMESPACE);
        let first = create_convoy(&convoys, "first", ConvoyPhase::Active).await;
        let second = create_convoy(&convoys, "second", ConvoyPhase::Active).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_with_inhibitor(convoys.clone(), RecordingInhibitor { states: tx }));
        assert!(next_state(&mut rx).await);

        convoys
            .update_status("first", &first.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Completed,
                ..ConvoyStatus::default()
            })
            .await
            .expect("complete first convoy");
        assert!(next_state(&mut rx).await);

        convoys
            .update_status("second", &second.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Completed,
                ..ConvoyStatus::default()
            })
            .await
            .expect("complete second convoy");
        assert!(!next_state(&mut rx).await);
        task.abort();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_uses_a_visible_logind_block_inhibitor() {
        let command = platform_inhibitor_command(42).expect("Linux inhibitor command");
        assert_eq!(command.program, "systemd-inhibit");
        assert!(command.args.iter().any(|arg| arg == "--what=sleep"));
        assert!(command.args.iter().any(|arg| arg == "--mode=block"));
        assert!(command.args.iter().any(|arg| arg == "--who=flotillad"));
        assert!(command.args.iter().any(|arg| arg == &format!("--why={INHIBITOR_REASON}")));
        assert!(command.args.iter().any(|arg| arg == "--pid=42"));
    }
}
