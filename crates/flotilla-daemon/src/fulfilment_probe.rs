//! Independent availability observations. Account/model entitlement is unknown:
//! only actual crew work uses the selected credential and may spend model turns.
use std::{collections::BTreeMap, path::Path, time::Duration};

use chrono::Utc;
use flotilla_agent::adapter::Harness;
use flotilla_environment::environment::EnvironmentProvider;
use flotilla_provider_runtime::{execution_context::ObservationContext, CommandRunner, EnvVars};
use flotilla_resources::{
    FulfilmentFacts, FulfilmentGrant, FulfilmentKindSpec, FulfilmentRealisation, HarnessFacts, ModelProbeState, Platform,
};

const TOOLCHAINS: &[&str] = &["rustc", "cargo", "node", "python3"];

pub(crate) struct DetectionTarget<'a> {
    pub(crate) image: Option<&'a str>,
    pub(crate) provider: &'a dyn EnvironmentProvider,
    /// The target's prepared base layer: what its crews run with, minus
    /// their account material.
    pub(crate) context: &'a ObservationContext,
    /// The harnesses to look for: the built-in ones and those registered adapters contribute.
    pub(crate) harnesses: &'a [Harness],
}

async fn run_in_realisation(
    target: &DetectionTarget<'_>,
    binary: &str,
    args: &[&str],
    scratch: &Path,
) -> Result<flotilla_provider_runtime::CommandOutput, String> {
    tokio::time::timeout(
        Duration::from_secs(15),
        target.provider.detection().ok_or("environment provider has no tool detection capability")?.detect(
            target.image,
            target.context,
            binary,
            args,
            scratch,
        ),
    )
    .await
    .map_err(|_| format!("timed out observing {binary}"))?
}

fn harness_version(stdout: &str) -> String {
    stdout
        .split_whitespace()
        .map(|token| token.strip_prefix('v').unwrap_or(token))
        .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or_default()
        .to_string()
}

/// Source: the target provider's isolated harness --version output. No login or
/// model request participates; an absent version leaves this harness unknown.
async fn harness_facts(target: &DetectionTarget<'_>, scratch: &Path) -> BTreeMap<String, HarnessFacts> {
    let mut facts = BTreeMap::new();
    let outputs = futures::future::join_all(
        target.harnesses.iter().map(|harness| run_in_realisation(target, harness.binary, &["--version"], scratch)),
    )
    .await;
    for (Harness { adapter: harness, .. }, output) in target.harnesses.iter().zip(outputs) {
        if let Ok(output) = output {
            let version = harness_version(&output.stdout);
            if output.success() && !version.is_empty() {
                facts.insert((*harness).into(), HarnessFacts { version, models: BTreeMap::new() });
            }
        }
    }
    facts
}

/// Source: the target provider's isolated toolchain --version output.
async fn toolchain_facts(target: &DetectionTarget<'_>, scratch: &Path) -> BTreeMap<String, String> {
    let mut facts = BTreeMap::new();
    let outputs = futures::future::join_all(TOOLCHAINS.iter().map(|tool| run_in_realisation(target, tool, &["--version"], scratch))).await;
    for (tool, output) in TOOLCHAINS.iter().zip(outputs) {
        if let Ok(output) = output {
            let version = output.stdout.lines().next().or_else(|| output.stderr.lines().next()).unwrap_or_default().trim();
            if output.success() && !version.is_empty() {
                facts.insert((*tool).into(), version.into());
            }
        }
    }
    facts
}

/// Source: the provider's local image inventory; observation never pulls.
async fn image_fact(target: &DetectionTarget<'_>) -> Option<Option<flotilla_resources::PlacedImageIdentity>> {
    let cache = target.provider.local_image_cache()?;
    tokio::time::timeout(Duration::from_secs(15), cache.inspect_reference(target.image?)).await.ok()?.ok()
}

/// Source: display environment, or the target host's launchd GUI domain.
async fn gui_fact(spec: &FulfilmentKindSpec, target: &DetectionTarget<'_>, env: &dyn EnvVars, scratch: &Path) -> bool {
    if !matches!(spec.realisation, FulfilmentRealisation::HostDirect) {
        return false;
    }
    if env.get("DISPLAY").is_some() || env.get("WAYLAND_DISPLAY").is_some() {
        return true;
    }
    if spec.grants.contains(&FulfilmentGrant::platform(Platform::Macos.to_string())) {
        if let Ok(output) = run_in_realisation(target, "id", &["-u"], scratch).await {
            let uid = output.stdout.trim();
            if output.success() && uid.parse::<u32>().is_ok() {
                return run_in_realisation(target, "launchctl", &["print", &format!("gui/{uid}")], scratch)
                    .await
                    .is_ok_and(|session| session.success());
            }
        }
    }
    false
}

/// Source: discovered terminal pool availability. None means unbounded.
fn capacity_fact(pool_available: bool) -> Option<u32> {
    (!pool_available).then_some(0)
}

pub(crate) async fn probe_kind(
    spec: &FulfilmentKindSpec,
    target: DetectionTarget<'_>,
    pool_available: bool,
    _runner: &dyn CommandRunner,
    env: &dyn EnvVars,
    scratch: &Path,
    model_probes: &mut ModelProbeState,
) -> Result<FulfilmentFacts, String> {
    // Retired request evidence remains decodable but is never fresh evidence.
    *model_probes = ModelProbeState::default();
    let mut facts = FulfilmentFacts {
        image: target.image.map(|image| image.to_string().into()),
        observed_at: Utc::now(),
        free_vessel_slots: capacity_fact(pool_available),
        ..Default::default()
    };
    if matches!(spec.realisation, FulfilmentRealisation::DockerPerVessel { .. }) {
        let inspected = image_fact(&target).await;
        facts.image_present = inspected.as_ref().map(Option::is_some);
        if let Some(identity) = inspected.flatten() {
            if let Some(image) = facts.image.as_mut() {
                image.local_image_id = Some(identity.local_image_id);
                image.registry_digest = identity.registry_digest;
            }
        }
        if facts.image_present == Some(false) {
            return Ok(facts);
        }
    }
    let (harnesses, toolchains, gui) =
        tokio::join!(harness_facts(&target, scratch), toolchain_facts(&target, scratch), gui_fact(spec, &target, env, scratch));
    facts.harnesses = harnesses;
    facts.toolchains = toolchains;
    facts.gui_session_logged_in = gui;
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_agent::adapter::built_in_harnesses;
    use flotilla_discovery_testkit::{DiscoveryMockRunner, TestEnvVars};
    use flotilla_environment::environment::{command_provider, EnvironmentInput, EnvironmentProvisionError};
    use flotilla_provider_runtime::{ChannelLabel, CommandOutput};
    use std::sync::{Arc, Mutex};

    /// The command the isolation shell runs: after its root, the counted
    /// base-layer words and the counted account homes.
    fn observed_command<'a>(args: &'a [&'a str]) -> Option<&'a [&'a str]> {
        let mut index = args.iter().position(|arg| *arg == "flotilla-observation")? + 2;
        for _ in 0..2 {
            index += args[index].parse::<usize>().expect("word count") + 1;
        }
        Some(&args[index..])
    }

    // Subprocess seam: unwrap only the provider's isolated observation shell.
    // Any model-turn command or ambient token/profile read fails immediately.
    #[derive(Default)]
    struct NoInference {
        calls: Mutex<Vec<String>>,
        slow_tool: bool,
        missing_image: bool,
        inference_requests: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl CommandRunner for NoInference {
        async fn run(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
            panic!("unexpected request or credential read")
        }
        async fn run_output(&self, cmd: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
            let harness_args = observed_command(args).unwrap_or(args);
            if harness_args.iter().any(|arg| ["--model", "--print", "-p", "/messages", "/completions"].contains(arg)) {
                self.inference_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Err("automatic inference forbidden".into());
            }
            if cmd == "rm" {
                return Ok(CommandOutput { stdout: String::new(), stderr: String::new(), exit_code: Some(0) });
            }
            if cmd == "docker" && args.contains(&"{{json .Config.Env}}") {
                return Ok(CommandOutput { stdout: r#"["PATH=/usr/bin:/bin"]"#.into(), stderr: String::new(), exit_code: Some(0) });
            }
            if cmd == "docker" && args.first() == Some(&"image") {
                return if self.missing_image {
                    Ok(CommandOutput { stdout: String::new(), stderr: "image unavailable".into(), exit_code: Some(1) })
                } else {
                    Err("inventory unknown".into())
                };
            }
            if cmd == "docker" && args.first() == Some(&"rm") {
                return Ok(CommandOutput { stdout: String::new(), stderr: String::new(), exit_code: Some(0) });
            }
            assert!(cmd == "env" || cmd == "docker");
            let observed = observed_command(args).expect("isolated shell");
            let binary = observed[0];
            assert_eq!(&observed[1..], &["--version"], "no model request may be used for availability");
            self.calls.lock().expect("calls").push(binary.into());
            if binary == "rustc" && self.slow_tool {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            let stdout = match binary {
                "claude" => "2.1.300 (Claude Code)",
                "codex" => "codex-cli 0.160.0",
                "contributed-crew" => "contributed-crew 3.1.0",
                "node" => "v24.0.0",
                _ => return Err("unavailable".into()),
            };
            Ok(CommandOutput { stdout: stdout.into(), stderr: String::new(), exit_code: Some(0) })
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            panic!("no credential discovery")
        }
    }

    // Unknown entitlement never causes inference; unrelated availability and
    // capacity facts survive errors, missing tools, and a timed-out tool.
    #[tokio::test(start_paused = true)]
    async fn independent_facts_never_request_models_or_inspect_login() {
        let runner = Arc::new(NoInference { slow_tool: true, ..Default::default() });
        let provider = command_provider(EnvironmentInput::HostDirect, runner.clone());
        let spec = FulfilmentKindSpec::builder()
            .host_ref("ssh-host".into())
            .pool("cleat".into())
            .realisation(FulfilmentRealisation::HostDirect)
            .build();
        let mut state = ModelProbeState { total_requests: 8, ..Default::default() };
        let facts = probe_kind(
            &spec,
            DetectionTarget {
                image: None,
                provider: provider.as_ref(),
                context: &Default::default(),
                harnesses: &built_in_harnesses(&TestEnvVars::default()).collect::<Vec<_>>(),
            },
            false,
            runner.as_ref(),
            &TestEnvVars::new([("HOME", "/fake-operator"), ("ANTHROPIC_API_KEY", "fake-unrelated-account")]),
            Path::new("/tmp/probe"),
            &mut state,
        )
        .await
        .expect("facts");
        assert_eq!(facts.harnesses["claude-code"].version, "2.1.300");
        assert_eq!(facts.harnesses["codex"].version, "0.160.0");
        assert!(facts.harnesses.values().all(|harness| harness.models.is_empty()), "entitlement stays unknown");
        assert_eq!(facts.toolchains["node"], "v24.0.0");
        assert_eq!(facts.free_vessel_slots, Some(0));
        assert_eq!(state, ModelProbeState::default());
        assert_eq!(runner.calls.lock().expect("calls").len(), TOOLCHAINS.len() + built_in_harnesses(&TestEnvVars::default()).count());
        assert_eq!(runner.inference_requests.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    // A harness a registered adapter contributes is looked for beside the built-in ones,
    // under its adapter's id; one no adapter contributes is never asked for.
    #[tokio::test]
    async fn contributed_harnesses_are_probed_under_their_adapter() {
        let runner = Arc::new(NoInference::default());
        let provider = command_provider(EnvironmentInput::HostDirect, runner.clone());
        let spec = FulfilmentKindSpec::builder()
            .host_ref("host".into())
            .pool("cleat".into())
            .realisation(FulfilmentRealisation::HostDirect)
            .build();
        let contributed = Harness { adapter: "contributed", binary: "contributed-crew" };
        for harnesses in [
            built_in_harnesses(&TestEnvVars::default()).collect::<Vec<_>>(),
            built_in_harnesses(&TestEnvVars::default()).chain([contributed]).collect(),
        ] {
            let facts = probe_kind(
                &spec,
                DetectionTarget { image: None, provider: provider.as_ref(), context: &Default::default(), harnesses: &harnesses },
                true,
                runner.as_ref(),
                &TestEnvVars::new([("HOME", "/fake-operator")]),
                Path::new("/tmp/probe"),
                &mut ModelProbeState::default(),
            )
            .await
            .expect("facts");
            assert_eq!(
                facts.harnesses.get("contributed").map(|harness| harness.version.as_str()),
                harnesses.contains(&contributed).then_some("3.1.0")
            );
            assert_eq!(facts.harnesses["claude-code"].version, "2.1.300");
        }
    }

    // A failed image inventory cannot suppress independent detection. A
    // confirmed missing image stays absent and is never pulled for observation.
    #[tokio::test]
    async fn image_inventory_unknown_and_absent_have_distinct_effects() {
        for missing_image in [false, true] {
            let runner = Arc::new(NoInference { missing_image, ..Default::default() });
            let provider = command_provider(EnvironmentInput::Docker, runner.clone());
            let spec = FulfilmentKindSpec::builder()
                .host_ref("host".into())
                .pool("cleat".into())
                .realisation(FulfilmentRealisation::DockerPerVessel { image: Some("crew:test".into()) })
                .build();
            let facts = probe_kind(
                &spec,
                DetectionTarget {
                    image: Some("crew:test"),
                    provider: provider.as_ref(),
                    context: &Default::default(),
                    harnesses: &built_in_harnesses(&TestEnvVars::default()).collect::<Vec<_>>(),
                },
                true,
                runner.as_ref(),
                &TestEnvVars::default(),
                Path::new("/tmp/probe"),
                &mut ModelProbeState::default(),
            )
            .await
            .expect("facts");
            assert_eq!(facts.image_present, missing_image.then_some(false));
            assert_eq!(facts.harnesses.is_empty(), missing_image);
            assert_eq!(facts.toolchains.is_empty(), missing_image);
            assert!(facts.free_vessel_slots.is_none());
            assert_eq!(runner.inference_requests.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }

    // Version parsing covers harness banners, optional v, and absent versions.
    #[test]
    fn harness_versions() {
        for (banner, version) in
            [("2.1.283 (Claude Code)", "2.1.283"), ("codex-cli 0.160.0", "0.160.0"), ("tool v2.1.300", "2.1.300"), ("no version", "")]
        {
            assert_eq!(harness_version(banner), version);
        }
    }
    // A provider can lack detection while retaining its real host lifecycle.
    // The wrapper withholds only that optional capability.
    struct WithoutDetection(Arc<dyn EnvironmentProvider>);
    #[async_trait::async_trait]
    impl EnvironmentProvider for WithoutDetection {
        flotilla_discovery_testkit::injected_environment_provider!();
        async fn prepare(
            &self,
            spec: &flotilla_resources::EnvironmentSpec,
            opts: &flotilla_environment::environment::PrepareOpts,
        ) -> Result<flotilla_environment::environment::PreparedEnvironment, String> {
            self.0.prepare(spec, opts).await
        }
        async fn provision(
            &self,
            id: flotilla_protocol::EnvironmentId,
            prepared: &flotilla_environment::environment::PreparedEnvironment,
            opts: flotilla_environment::environment::ProvisionOpts,
        ) -> Result<flotilla_environment::environment::EnvironmentHandle, EnvironmentProvisionError> {
            self.0.provision(id, prepared, opts).await
        }
        async fn list(&self) -> Result<Vec<flotilla_environment::environment::EnvironmentHandle>, String> {
            self.0.list().await
        }
        async fn destroy(&self, backing_id: &str) -> Result<(), String> {
            self.0.destroy(backing_id).await
        }
    }

    // ADR 0058: absent optional detection cannot attest tool/harness facts and
    // must not turn a successful host observation into an operational failure.
    #[tokio::test]
    async fn absent_detection_leaves_fulfilment_facts_unknown() {
        let scratch = Path::new("/tmp/flotilla-probe-test");
        let runner = Arc::new(DiscoveryMockRunner::builder().build());
        let provider = WithoutDetection(Arc::new(flotilla_environment::environment::host_direct::HostDirectEnvironmentProvider::new(
            runner.clone(),
            Default::default(),
        )));
        let mut spec = FulfilmentKindSpec::builder()
            .host_ref("host".into())
            .pool("cleat".into())
            .realisation(FulfilmentRealisation::HostDirect)
            .build();
        spec.realisation = FulfilmentRealisation::HostDirect;
        let mut models = ModelProbeState::default();
        let facts = super::probe_kind(
            &spec,
            DetectionTarget {
                image: None,
                provider: &provider,
                context: &Default::default(),
                harnesses: &built_in_harnesses(&TestEnvVars::default()).collect::<Vec<_>>(),
            },
            true,
            runner.as_ref(),
            &TestEnvVars::default(),
            scratch,
            &mut models,
        )
        .await
        .expect("observation succeeds without detection");
        assert!(facts.toolchains.is_empty());
        assert!(facts.harnesses.is_empty());
        assert!(facts.image_present.is_none());
        assert!(facts.free_vessel_slots.is_none());
        assert!(models.entries.is_empty());
    }
}
