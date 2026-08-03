use std::{
    collections::BTreeSet,
    process::{Output, Stdio},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_protocol::SleepInhibitionHealth;
use flotilla_resources::{
    apply_status_patch, Convoy, ConvoyPhase, Host, HostStatusPatch, ReadResourceList, ReadResourceObject, ReadWatchEvent,
    ReplicaReadResolver, ResourceError, ResourceObject, ResourceProvenance, TypedResolver, Vessel, WatchEvent, WatchStart,
};
use futures::StreamExt;
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
};
use tracing::{error, info, warn};

const HEALTHY_RECHECK_INTERVAL: Duration = Duration::from_secs(30);
const ACQUISITION_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const FAILED_RETRY_INTERVAL: Duration = Duration::from_secs(300);
const ACQUISITION_CONFIRMATION: Duration = Duration::from_millis(250);
const KDE_INHIBITION_ENFORCEMENT_DELAY: Duration = Duration::from_millis(5_250);
const KDE_VERIFICATION_TIMEOUT: Duration = Duration::from_secs(3);
const FAILURE_THRESHOLD: u32 = 3;
const INHIBITOR_REASON: &str = "Flotilla vessel crew is active";

#[async_trait]
trait SleepInhibitor: Send {
    async fn maintain(&mut self, required: bool) -> Result<SleepInhibitionHealth, String>;
}

pub(super) async fn run(
    convoys: ReplicaReadResolver<Convoy>,
    vessels: TypedResolver<Vessel>,
    hosts: TypedResolver<Host>,
    host_id: String,
) -> Result<(), ResourceError> {
    run_with_inhibitor(convoys, vessels, hosts, host_id, SystemSleepInhibitor::default()).await
}

async fn run_with_inhibitor<I: SleepInhibitor>(
    convoys: ReplicaReadResolver<Convoy>,
    vessels: TypedResolver<Vessel>,
    hosts: TypedResolver<Host>,
    host_id: String,
    mut inhibitor: I,
) -> Result<(), ResourceError> {
    let mut health = InhibitionHealthTracker::default();
    // Hold while the federated view is being established. Sleeping during an
    // unknown replica state is more expensive than briefly holding needlessly.
    maintain_and_publish(&mut inhibitor, true, &mut health, &hosts, &host_id).await?;

    // Overlay watches cannot resume from a list cursor. Start the watch first,
    // then list, so events racing with the snapshot remain queued for us.
    let mut watch = convoys.watch().await?;
    let listed = convoys.list().await?;
    let mut active_convoys = ActiveConvoys::from_objects(&listed, &host_id);

    let listed_vessels = vessels.list().await?;
    let mut active_vessels = ActiveVessels::from_objects(&listed_vessels.items);
    let mut vessel_watch = vessels.watch(WatchStart::resuming_from(&listed_vessels)).await?;
    maintain_and_publish(&mut inhibitor, active_convoys.required() || active_vessels.required(), &mut health, &hosts, &host_id).await?;

    let recheck = tokio::time::sleep(health.recheck_interval());
    tokio::pin!(recheck);

    loop {
        tokio::select! {
            event = watch.next() => {
                match event {
                    Some(Ok(event)) => {
                        active_convoys.apply(event, &host_id);
                        maintain_and_publish(
                            &mut inhibitor,
                            active_convoys.required() || active_vessels.required(),
                            &mut health,
                            &hosts,
                            &host_id,
                        ).await?;
                        recheck.as_mut().reset(tokio::time::Instant::now() + health.recheck_interval());
                    }
                    Some(Err(error)) => {
                        maintain_and_publish(&mut inhibitor, true, &mut health, &hosts, &host_id).await?;
                        return Err(error);
                    }
                    None => {
                        maintain_and_publish(&mut inhibitor, true, &mut health, &hosts, &host_id).await?;
                        return Err(ResourceError::other("sleep inhibitor convoy watch ended"));
                    }
                }
            }
            event = vessel_watch.next() => {
                match event {
                    Some(Ok(event)) => {
                        active_vessels.apply(event);
                        maintain_and_publish(
                            &mut inhibitor,
                            active_convoys.required() || active_vessels.required(),
                            &mut health,
                            &hosts,
                            &host_id,
                        ).await?;
                        recheck.as_mut().reset(tokio::time::Instant::now() + health.recheck_interval());
                    }
                    Some(Err(error)) => {
                        maintain_and_publish(&mut inhibitor, true, &mut health, &hosts, &host_id).await?;
                        return Err(error);
                    }
                    None => {
                        maintain_and_publish(&mut inhibitor, true, &mut health, &hosts, &host_id).await?;
                        return Err(ResourceError::other("sleep inhibitor vessel watch ended"));
                    }
                }
            }
            _ = &mut recheck => {
                maintain_and_publish(
                    &mut inhibitor,
                    active_convoys.required() || active_vessels.required(),
                    &mut health,
                    &hosts,
                    &host_id,
                ).await?;
                recheck.as_mut().reset(tokio::time::Instant::now() + health.recheck_interval());
            }
        }
    }
}

#[derive(Default)]
struct ActiveVessels {
    names: BTreeSet<String>,
}

impl ActiveVessels {
    fn from_objects(objects: &[ResourceObject<Vessel>]) -> Self {
        Self { names: objects.iter().map(|vessel| vessel.metadata.name.clone()).collect() }
    }

    fn apply(&mut self, event: WatchEvent<Vessel>) {
        match event {
            WatchEvent::Added(vessel) | WatchEvent::Modified(vessel) => {
                self.names.insert(vessel.metadata.name);
            }
            WatchEvent::Deleted(vessel) => {
                self.names.remove(&vessel.metadata.name);
            }
        }
    }

    fn required(&self) -> bool {
        !self.names.is_empty()
    }
}

async fn maintain_and_publish(
    inhibitor: &mut impl SleepInhibitor,
    required: bool,
    tracker: &mut InhibitionHealthTracker,
    hosts: &TypedResolver<Host>,
    host_id: &str,
) -> Result<(), ResourceError> {
    let result = inhibitor.maintain(required).await;
    let observation = tracker.observe(result);
    match observation.log {
        FailureLog::None => {}
        FailureLog::First => {
            warn!(error = observation.error.as_deref().unwrap_or_default(), required, "system sleep inhibition failed; retrying")
        }
        FailureLog::Degraded => error!(
            error = observation.error.as_deref().unwrap_or_default(),
            consecutive_failures = FAILURE_THRESHOLD,
            "system sleep inhibition is degraded"
        ),
    }
    if let Some(health) = observation.changed {
        apply_status_patch(hosts, host_id, &HostStatusPatch::SleepInhibition { health, observed_at: chrono::Utc::now() }).await?;
    }
    Ok(())
}

#[derive(Default)]
struct InhibitionHealthTracker {
    consecutive_failures: u32,
    last_error: Option<String>,
    published: Option<SleepInhibitionHealth>,
}

impl InhibitionHealthTracker {
    fn observe(&mut self, result: Result<SleepInhibitionHealth, String>) -> HealthObservation {
        let (health, log, error) = match result {
            Ok(health) => {
                self.consecutive_failures = 0;
                self.last_error = None;
                (health, FailureLog::None, None)
            }
            Err(error) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1).min(FAILURE_THRESHOLD);
                let error_changed = self.last_error.as_ref() != Some(&error);
                self.last_error = Some(error.clone());
                let health = if self.consecutive_failures >= FAILURE_THRESHOLD {
                    SleepInhibitionHealth::Failed { consecutive_failures: self.consecutive_failures, message: error.clone() }
                } else {
                    SleepInhibitionHealth::Acquiring { consecutive_failures: self.consecutive_failures, message: error.clone() }
                };
                let log = if self.consecutive_failures == FAILURE_THRESHOLD
                    && (!matches!(self.published, Some(SleepInhibitionHealth::Failed { .. })) || error_changed)
                {
                    FailureLog::Degraded
                } else if self.consecutive_failures == 1 || error_changed {
                    FailureLog::First
                } else {
                    FailureLog::None
                };
                (health, log, Some(error))
            }
        };
        let changed = (self.published.as_ref() != Some(&health)).then(|| {
            self.published = Some(health.clone());
            health
        });
        HealthObservation { changed, log, error }
    }

    fn recheck_interval(&self) -> Duration {
        match self.published.as_ref() {
            Some(SleepInhibitionHealth::Acquiring { .. }) => ACQUISITION_RETRY_INTERVAL,
            Some(SleepInhibitionHealth::Failed { .. }) => FAILED_RETRY_INTERVAL,
            _ => HEALTHY_RECHECK_INTERVAL,
        }
    }
}

struct HealthObservation {
    changed: Option<SleepInhibitionHealth>,
    log: FailureLog,
    error: Option<String>,
}

enum FailureLog {
    None,
    First,
    Degraded,
}

#[derive(Default)]
struct ActiveConvoys {
    keys: BTreeSet<(Option<flotilla_protocol::NodeId>, String)>,
}

impl ActiveConvoys {
    fn from_objects(listed: &ReadResourceList<Convoy>, host_id: &str) -> Self {
        let keys = listed.items.iter().filter(|convoy| convoy_requires_inhibitor(&convoy.object, host_id)).map(convoy_key).collect();
        Self { keys }
    }

    fn apply(&mut self, event: ReadWatchEvent<Convoy>, host_id: &str) {
        match event {
            ReadWatchEvent::Added(convoy) | ReadWatchEvent::Modified(convoy) => {
                let key = convoy_key(&convoy);
                if convoy_requires_inhibitor(&convoy.object, host_id) {
                    self.keys.insert(key);
                } else {
                    self.keys.remove(&key);
                }
            }
            ReadWatchEvent::Deleted(convoy) => {
                self.keys.remove(&convoy_key(&convoy));
            }
        }
    }

    fn required(&self) -> bool {
        !self.keys.is_empty()
    }
}

fn convoy_key(convoy: &ReadResourceObject<Convoy>) -> (Option<flotilla_protocol::NodeId>, String) {
    let origin = match &convoy.provenance {
        ResourceProvenance::Local => None,
        ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.clone()),
    };
    (origin, convoy.object.metadata.name.clone())
}

fn convoy_requires_inhibitor(convoy: &ResourceObject<Convoy>, host_id: &str) -> bool {
    let non_terminal = !matches!(
        convoy.status.as_ref().map(|status| status.phase),
        Some(ConvoyPhase::Landed | ConvoyPhase::Failed | ConvoyPhase::Cancelled | ConvoyPhase::Abandoned)
    );
    non_terminal
        && convoy
            .status
            .as_ref()
            .and_then(|status| status.placement_decision.as_ref())
            .is_none_or(|decision| decision.target_host.reference == host_id)
}

#[derive(Default)]
struct SystemSleepInhibitor {
    child: Option<Child>,
}

#[async_trait]
impl SleepInhibitor for SystemSleepInhibitor {
    async fn maintain(&mut self, required: bool) -> Result<SleepInhibitionHealth, String> {
        if let Some(child) = self.child.as_mut() {
            match child.try_wait().map_err(|error| format!("check sleep inhibitor process: {error}"))? {
                Some(status) => {
                    warn!(%status, "system sleep inhibitor process exited; reacquiring if still required");
                    self.child = None;
                }
                None if required => return Ok(SleepInhibitionHealth::Held),
                None => {}
            }
        }

        if required {
            self.acquire().await?;
            Ok(SleepInhibitionHealth::Held)
        } else {
            self.release().await?;
            Ok(SleepInhibitionHealth::NotRequired)
        }
    }
}

impl SystemSleepInhibitor {
    async fn acquire(&mut self) -> Result<(), String> {
        let mut failures = Vec::new();
        for command_spec in platform_inhibitor_commands(std::process::id())? {
            let program = command_spec.program.clone();
            match self.acquire_command(command_spec).await {
                Ok(()) => return Ok(()),
                Err(error) => failures.push(format!("{program}: {error}")),
            }
        }
        Err(format!("all system sleep inhibitor strategies failed: {}", failures.join("; ")))
    }

    async fn acquire_command(&mut self, command_spec: InhibitorCommand) -> Result<(), String> {
        let mut command = Command::new(&command_spec.program);
        command.args(&command_spec.args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| format!("start {}: {error}", command_spec.program))?;
        tokio::time::sleep(ACQUISITION_CONFIRMATION).await;
        if let Some(status) = child.try_wait().map_err(|error| format!("confirm sleep inhibitor process: {error}"))? {
            let mut stderr = String::new();
            if let Some(mut stream) = child.stderr.take() {
                stream.read_to_string(&mut stderr).await.map_err(|error| format!("read sleep inhibitor error output: {error}"))?;
            }
            let detail = stderr.trim();
            return Err(if detail.is_empty() {
                format!("{} exited during acquisition confirmation with {status}", command_spec.program)
            } else {
                format!("{} exited during acquisition confirmation with {status}: {detail}", command_spec.program)
            });
        }
        if command_spec.verification == InhibitorVerification::KdePowerManagement {
            tokio::time::sleep(KDE_INHIBITION_ENFORCEMENT_DELAY).await;
            if let Err(error) = verify_kde_power_inhibition().await {
                child
                    .kill()
                    .await
                    .map_err(|kill_error| format!("{error}; stop unverified {} process: {kill_error}", command_spec.program))?;
                return Err(error);
            }
        }
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                if let Err(error) = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await {
                    warn!(%error, "failed to drain sleep inhibitor error output");
                }
            });
        }
        self.child = Some(child);
        info!(program = %command_spec.program, reason = INHIBITOR_REASON, "acquired system sleep inhibitor");
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
    program: String,
    args: Vec<String>,
    verification: InhibitorVerification,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InhibitorVerification {
    ProcessAlive,
    KdePowerManagement,
}

async fn verify_kde_power_inhibition() -> Result<(), String> {
    let mut command = Command::new("qdbus6");
    command.args([
        "org.freedesktop.PowerManagement.Inhibit",
        "/org/freedesktop/PowerManagement/Inhibit",
        "org.freedesktop.PowerManagement.Inhibit.HasInhibit",
    ]);
    let output = command_output_with_timeout(command, KDE_VERIFICATION_TIMEOUT, "verify KDE power inhibition with qdbus6").await?;
    if !output.status.success() {
        return Err(format!("verify KDE power inhibition with qdbus6: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    if String::from_utf8_lossy(&output.stdout).trim() != "true" {
        return Err("KDE power inhibition was not active after its enforcement delay".to_string());
    }
    Ok(())
}

async fn command_output_with_timeout(mut command: Command, timeout: Duration, description: &str) -> Result<Output, String> {
    command.kill_on_drop(true);
    tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| format!("{description}: timed out after {}s", timeout.as_secs_f64()))?
        .map_err(|error| format!("{description}: {error}"))
}

#[cfg(target_os = "linux")]
fn platform_inhibitor_commands(daemon_pid: u32) -> Result<Vec<InhibitorCommand>, String> {
    let payload = || vec!["tail".to_string(), format!("--pid={daemon_pid}"), "-f".to_string(), "/dev/null".to_string()];
    let systemd = |what: &str| {
        let mut args =
            vec![format!("--what={what}"), "--who=flotillad".to_string(), format!("--why={INHIBITOR_REASON}"), "--mode=block".to_string()];
        args.extend(payload());
        InhibitorCommand { program: "systemd-inhibit".to_string(), args, verification: InhibitorVerification::ProcessAlive }
    };
    // kde-inhibit waits for its payload, so make the payload watch both the
    // daemon and kde-inhibit itself. This prevents an orphan if verification
    // fails and we have to kill the wrapper.
    let kde_args = vec![
        "--power".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        "wrapper=$PPID; while kill -0 \"$1\" 2>/dev/null && kill -0 \"$wrapper\" 2>/dev/null; do sleep 1; done".to_string(),
        "flotilla-sleep-inhibitor".to_string(),
        daemon_pid.to_string(),
    ];
    Ok(vec![
        systemd("sleep"),
        InhibitorCommand { program: "kde-inhibit".to_string(), args: kde_args, verification: InhibitorVerification::KdePowerManagement },
        systemd("idle"),
    ])
}

#[cfg(target_os = "macos")]
fn platform_inhibitor_commands(daemon_pid: u32) -> Result<Vec<InhibitorCommand>, String> {
    Ok(vec![InhibitorCommand {
        program: "caffeinate".to_string(),
        args: vec!["-i".to_string(), "-w".to_string(), daemon_pid.to_string()],
        verification: InhibitorVerification::ProcessAlive,
    }])
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_inhibitor_commands(_daemon_pid: u32) -> Result<Vec<InhibitorCommand>, String> {
    Err("system sleep inhibition is unsupported on this platform".to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};

    use chrono::Utc;
    use flotilla_protocol::{NodeId, PlacementDecision, PlacementTargetHost, PrincipalRef};
    use flotilla_resources::{ConvoySpec, ConvoyStatus, HostSpec, InMemoryBackend, InputMeta, ResourceBackend, VesselSpec};
    use tokio::sync::mpsc;

    use super::*;

    const NAMESPACE: &str = "flotilla";

    struct RecordingInhibitor {
        states: mpsc::UnboundedSender<bool>,
    }

    struct ScriptedInhibitor {
        results: VecDeque<Result<SleepInhibitionHealth, String>>,
    }

    #[async_trait]
    impl SleepInhibitor for RecordingInhibitor {
        async fn maintain(&mut self, required: bool) -> Result<SleepInhibitionHealth, String> {
            self.states.send(required).map_err(|_| "test receiver closed".to_string())?;
            Ok(if required { SleepInhibitionHealth::Held } else { SleepInhibitionHealth::NotRequired })
        }
    }

    #[async_trait]
    impl SleepInhibitor for ScriptedInhibitor {
        async fn maintain(&mut self, _required: bool) -> Result<SleepInhibitionHealth, String> {
            self.results.pop_front().expect("scripted inhibitor result")
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
            change_request: None,
            instruction: None,
        }
    }

    async fn create_convoy(
        convoys: &TypedResolver<Convoy>,
        name: &str,
        phase: ConvoyPhase,
        target_host: Option<&str>,
    ) -> ResourceObject<Convoy> {
        let created = convoys.create(&InputMeta::builder().name(name.to_string()).build(), &convoy_spec()).await.expect("create convoy");
        convoys
            .update_status(name, &created.metadata.resource_version, &ConvoyStatus {
                phase,
                placement_decision: target_host.map(|host| PlacementDecision {
                    policy_name: "test-policy".to_string(),
                    target_host: PlacementTargetHost { reference: host.to_string(), display_name: host.to_string() },
                    refused_candidates: vec![],
                    viable_not_selected: vec![],
                }),
                ..ConvoyStatus::default()
            })
            .await
            .expect("update convoy status")
    }

    async fn replicate_convoys(source: &TypedResolver<Convoy>, target: &ResourceBackend, origin: &str) {
        let listed = source.list().await.expect("list convoy authority");
        target.replica_writer::<Convoy>(NodeId::new(origin), NAMESPACE).replace(&listed, Utc::now()).await.expect("replicate convoys");
    }

    async fn create_host(hosts: &TypedResolver<Host>) {
        hosts.create(&InputMeta::builder().name("test-host".to_string()).build(), &HostSpec::default()).await.expect("create host");
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
        let hosts = backend.using::<Host>(NAMESPACE);
        create_host(&hosts).await;
        create_convoy(&convoys, "active", ConvoyPhase::Active, Some("test-host")).await;
        let (tx, mut rx) = mpsc::unbounded_channel();

        let task = tokio::spawn(run_with_inhibitor(
            backend.including_replicas::<Convoy>(NAMESPACE),
            backend.using::<Vessel>(NAMESPACE),
            hosts.clone(),
            "test-host".to_string(),
            RecordingInhibitor { states: tx },
        ));

        assert!(next_state(&mut rx).await, "uncertain startup must hold");
        assert!(next_state(&mut rx).await);
        assert_eq!(
            hosts.get("test-host").await.expect("get host").status.expect("host status").sleep_inhibition,
            SleepInhibitionHealth::Held
        );
        task.abort();
    }

    #[tokio::test]
    async fn releases_only_after_the_last_convoy_becomes_terminal() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let convoys = backend.using::<Convoy>(NAMESPACE);
        let hosts = backend.using::<Host>(NAMESPACE);
        create_host(&hosts).await;
        let first = create_convoy(&convoys, "first", ConvoyPhase::Active, Some("test-host")).await;
        let second = create_convoy(&convoys, "second", ConvoyPhase::Active, Some("test-host")).await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_with_inhibitor(
            backend.including_replicas::<Convoy>(NAMESPACE),
            backend.using::<Vessel>(NAMESPACE),
            hosts,
            "test-host".to_string(),
            RecordingInhibitor { states: tx },
        ));
        assert!(next_state(&mut rx).await);
        assert!(next_state(&mut rx).await);

        convoys
            .update_status("first", &first.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Landed,
                ..ConvoyStatus::default()
            })
            .await
            .expect("complete first convoy");
        assert!(next_state(&mut rx).await);

        convoys
            .update_status("second", &second.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Landed,
                ..ConvoyStatus::default()
            })
            .await
            .expect("complete second convoy");
        assert!(!next_state(&mut rx).await);
        task.abort();
    }

    #[tokio::test]
    async fn remotely_dispatched_convoy_placed_here_holds_inhibition() {
        let local = ResourceBackend::InMemory(InMemoryBackend::default());
        let remote = ResourceBackend::InMemory(InMemoryBackend::default());
        let hosts = local.using::<Host>(NAMESPACE);
        create_host(&hosts).await;
        let remote_convoys = remote.using::<Convoy>(NAMESPACE);
        create_convoy(&remote_convoys, "remote", ConvoyPhase::Active, Some("test-host")).await;
        replicate_convoys(&remote_convoys, &local, "dispatch-host").await;
        let (tx, mut rx) = mpsc::unbounded_channel();

        let task = tokio::spawn(run_with_inhibitor(
            local.including_replicas::<Convoy>(NAMESPACE),
            local.using::<Vessel>(NAMESPACE),
            hosts,
            "test-host".to_string(),
            RecordingInhibitor { states: tx },
        ));

        assert!(next_state(&mut rx).await);
        assert!(next_state(&mut rx).await);
        task.abort();
    }

    #[tokio::test]
    async fn locally_dispatched_convoy_placed_elsewhere_does_not_hold_inhibition() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let hosts = backend.using::<Host>(NAMESPACE);
        create_host(&hosts).await;
        create_convoy(&backend.using::<Convoy>(NAMESPACE), "remote-work", ConvoyPhase::Active, Some("other-host")).await;
        let (tx, mut rx) = mpsc::unbounded_channel();

        let task = tokio::spawn(run_with_inhibitor(
            backend.including_replicas::<Convoy>(NAMESPACE),
            backend.using::<Vessel>(NAMESPACE),
            hosts,
            "test-host".to_string(),
            RecordingInhibitor { states: tx },
        ));

        assert!(next_state(&mut rx).await);
        assert!(!next_state(&mut rx).await);
        task.abort();
    }

    #[tokio::test]
    async fn unknown_placement_holds_until_replica_catches_up() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let hosts = backend.using::<Host>(NAMESPACE);
        create_host(&hosts).await;
        let convoys = backend.using::<Convoy>(NAMESPACE);
        let lagging = create_convoy(&convoys, "lagging", ConvoyPhase::Active, None).await;
        let (tx, mut rx) = mpsc::unbounded_channel();

        let task = tokio::spawn(run_with_inhibitor(
            backend.including_replicas::<Convoy>(NAMESPACE),
            backend.using::<Vessel>(NAMESPACE),
            hosts,
            "test-host".to_string(),
            RecordingInhibitor { states: tx },
        ));

        assert!(next_state(&mut rx).await);
        assert!(next_state(&mut rx).await);
        convoys
            .update_status("lagging", &lagging.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Active,
                placement_decision: Some(PlacementDecision {
                    policy_name: "test-policy".to_string(),
                    target_host: PlacementTargetHost { reference: "other-host".to_string(), display_name: "other-host".to_string() },
                    refused_candidates: vec![],
                    viable_not_selected: vec![],
                }),
                ..ConvoyStatus::default()
            })
            .await
            .expect("resolve placement after lag");
        assert!(!next_state(&mut rx).await);
        task.abort();
    }

    #[tokio::test]
    async fn local_vessel_holds_after_its_convoy_no_longer_requires_inhibition() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let hosts = backend.using::<Host>(NAMESPACE);
        let vessels = backend.using::<Vessel>(NAMESPACE);
        create_host(&hosts).await;
        create_convoy(&backend.using::<Convoy>(NAMESPACE), "winding-down", ConvoyPhase::Landed, Some("test-host")).await;
        vessels
            .create(&InputMeta::builder().name("local-vessel".to_string()).build(), &VesselSpec {
                convoy_ref: "winding-down".to_string(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "test-policy".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            })
            .await
            .expect("create local vessel");
        let (tx, mut rx) = mpsc::unbounded_channel();

        let task = tokio::spawn(run_with_inhibitor(
            backend.including_replicas::<Convoy>(NAMESPACE),
            vessels.clone(),
            hosts,
            "test-host".to_string(),
            RecordingInhibitor { states: tx },
        ));

        assert!(next_state(&mut rx).await);
        assert!(next_state(&mut rx).await);
        vessels.delete("local-vessel").await.expect("delete torn-down vessel");
        assert!(!next_state(&mut rx).await);
        task.abort();
    }

    #[test]
    fn three_consecutive_failures_escalate_sleep_inhibition_health() {
        let mut tracker = InhibitionHealthTracker::default();

        let first = tracker.observe(Err("polkit denied".to_string())).changed.expect("first failure should publish");
        assert!(matches!(first, SleepInhibitionHealth::Acquiring { consecutive_failures: 1, .. }));

        let second = tracker.observe(Err("polkit denied".to_string())).changed.expect("second failure should publish");
        assert!(matches!(second, SleepInhibitionHealth::Acquiring { consecutive_failures: 2, .. }));

        let third = tracker.observe(Err("polkit denied".to_string())).changed.expect("third failure should publish");
        assert_eq!(third, SleepInhibitionHealth::Failed { consecutive_failures: FAILURE_THRESHOLD, message: "polkit denied".to_string() });

        assert!(tracker.observe(Err("polkit denied".to_string())).changed.is_none(), "identical failures should stay deduplicated");
        assert_eq!(tracker.recheck_interval(), FAILED_RETRY_INTERVAL);
    }

    #[tokio::test]
    async fn persistent_failure_publishes_a_host_condition_that_recovery_clears() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let hosts = backend.using::<Host>(NAMESPACE);
        create_host(&hosts).await;
        let mut inhibitor = ScriptedInhibitor {
            results: VecDeque::from([
                Err("polkit denied".to_string()),
                Err("polkit denied".to_string()),
                Err("polkit denied".to_string()),
                Ok(SleepInhibitionHealth::Held),
            ]),
        };
        let mut tracker = InhibitionHealthTracker::default();

        for _ in 0..FAILURE_THRESHOLD {
            maintain_and_publish(&mut inhibitor, true, &mut tracker, &hosts, "test-host").await.expect("publish sleep inhibition health");
        }

        let failed = hosts.get("test-host").await.expect("get failed host").status.expect("failed host status");
        assert_eq!(failed.sleep_inhibition, SleepInhibitionHealth::Failed {
            consecutive_failures: FAILURE_THRESHOLD,
            message: "polkit denied".to_string()
        });
        assert_eq!(failed.conditions.len(), 1);
        assert_eq!(failed.conditions[0].condition_type, "SleepInhibition");
        assert_eq!(failed.conditions[0].reason, "InhibitorNotHeld");
        assert!(failed.conditions[0].message.contains("sleep inhibition required but not held"));
        assert!(failed.conditions[0].message.contains("polkit denied"));

        maintain_and_publish(&mut inhibitor, true, &mut tracker, &hosts, "test-host").await.expect("publish sleep inhibition recovery");

        let recovered = hosts.get("test-host").await.expect("get recovered host").status.expect("recovered host status");
        assert_eq!(recovered.sleep_inhibition, SleepInhibitionHealth::Held);
        assert!(recovered.conditions.iter().all(|condition| condition.condition_type != "SleepInhibition"));
    }

    #[tokio::test]
    async fn immediate_child_exit_is_an_acquisition_failure_with_stderr() {
        let mut inhibitor = SystemSleepInhibitor::default();

        let error = inhibitor
            .acquire_command(InhibitorCommand {
                program: "sh".to_string(),
                args: vec!["-c".to_string(), "echo 'polkit denied' >&2; exit 1".to_string()],
                verification: InhibitorVerification::ProcessAlive,
            })
            .await
            .expect_err("immediate exit should fail acquisition");

        assert!(error.contains("exited during acquisition confirmation with exit status: 1"), "{error}");
        assert!(error.contains("polkit denied"), "{error}");
        assert!(inhibitor.child.is_none());
    }

    #[tokio::test]
    async fn external_verification_command_is_bounded_by_a_timeout() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30"]);

        let error = command_output_with_timeout(command, Duration::from_millis(10), "test verification")
            .await
            .expect_err("slow verification should time out");

        assert!(error.contains("test verification: timed out"), "{error}");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_falls_back_from_logind_sleep_to_desktop_and_idle_inhibition() {
        let commands = platform_inhibitor_commands(42).expect("Linux inhibitor commands");
        assert_eq!(commands.len(), 3);

        assert_eq!(commands[0].program, "systemd-inhibit");
        assert!(commands[0].args.iter().any(|arg| arg == "--what=sleep"));
        assert!(commands[0].args.iter().any(|arg| arg == "--mode=block"));

        assert_eq!(commands[1].program, "kde-inhibit");
        assert!(commands[1].args.iter().any(|arg| arg == "--power"));
        assert!(commands[1].args.iter().any(|arg| arg == "42"));
        assert_eq!(commands[1].verification, InhibitorVerification::KdePowerManagement);

        assert_eq!(commands[2].program, "systemd-inhibit");
        assert!(commands[2].args.iter().any(|arg| arg == "--what=idle"));

        for command in [&commands[0], &commands[2]] {
            assert!(command.args.iter().any(|arg| arg == "--pid=42"));
        }
    }
}
