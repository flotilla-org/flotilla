use std::{collections::BTreeMap, path::Path, time::Duration};

use chrono::Utc;
use flotilla_core::providers::{discovery::EnvVars, ChannelLabel, CommandRunner};
use flotilla_resources::{
    CachedModelProbe, FulfilmentFacts, FulfilmentGrant, FulfilmentKindSpec, FulfilmentRealisation, HarnessFacts, ModelFact,
    ModelFactSource, ModelProbeState,
};
use sha2::{Digest, Sha256};

// These are the fleet's candidate lists, not claims that every host can run
// them. Operators may extend the model list through the injected environment.
const DEFAULT_MODELS: &[&str] = &["sonnet", "opus"];
const TOOLCHAINS: &[&str] = &["rustc", "cargo", "node", "python3"];
const HARNESSES: &[(&str, &str)] = &[("claude-code", "claude"), ("codex", "codex")];
/// A hard host-wide ceiling, persisted in Host status across daemon restarts.
const MAX_MODEL_REQUESTS_PER_DAY: u32 = 8;
const MODEL_CACHE_MAX_AGE: chrono::Duration = chrono::Duration::days(7);

fn declared_models(env: &dyn EnvVars) -> Vec<String> {
    let mut models = env.get("FLOTILLA_PROBE_MODELS").map_or_else(
        || DEFAULT_MODELS.iter().map(|model| (*model).to_string()).collect::<Vec<_>>(),
        |declared| declared.split(',').map(str::trim).filter(|model| !model.is_empty()).map(ToString::to_string).collect(),
    );
    models.sort();
    models.dedup();
    models
}

async fn run_in_realisation(
    runner: &dyn CommandRunner,
    realisation: &FulfilmentRealisation,
    image: Option<&str>,
    binary: &str,
    args: &[&str],
    scratch: &Path,
) -> Result<flotilla_core::providers::CommandOutput, String> {
    let cwd = scratch;
    let label = ChannelLabel::Default;
    tokio::time::timeout(Duration::from_secs(15), async {
        match realisation {
            FulfilmentRealisation::HostDirect => runner.run_output(binary, args, cwd, &label).await,
            FulfilmentRealisation::DockerPerVessel { .. } => {
                let image = image.ok_or("docker image is unresolved")?;
                let mut command = vec!["run", "--rm", "--pull=never", "--workdir", "/probe", "--tmpfs", "/probe", image, binary];
                command.extend_from_slice(args);
                runner.run_output("docker", &command, cwd, &label).await
            }
        }
    })
    .await
    .map_err(|_| format!("timed out probing {binary}"))?
}

async fn credential_fingerprint(runner: &dyn CommandRunner, env: &dyn EnvVars, scratch: &Path) -> String {
    let mut hash = Sha256::new();
    for name in ["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CONFIG_DIR"] {
        hash.update(name.as_bytes());
        hash.update(env.get(name).unwrap_or_default().as_bytes());
    }
    let credential_path = env
        .get("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.trim().is_empty())
        .map(|dir| format!("{dir}/.credentials.json"))
        .or_else(|| env.get("HOME").map(|home| format!("{home}/.claude/.credentials.json")));
    if let Some(path) = credential_path {
        if let Ok(contents) = runner.run("cat", &[&path], scratch, &ChannelLabel::Default).await {
            hash.update(contents.as_bytes());
        }
    }
    format!("{:x}", hash.finalize())
}

fn cached_or_budgeted_model(state: &mut ModelProbeState, key: &str, now: chrono::DateTime<Utc>) -> Option<Option<ModelFact>> {
    state.entries.retain(|_, entry| now.signed_duration_since(entry.observed_at) < MODEL_CACHE_MAX_AGE);
    if let Some(entry) = state.entries.get(key) {
        return Some(entry.fact.clone());
    }
    if state.window_started_at.is_none_or(|start| now.signed_duration_since(start) >= chrono::Duration::days(1)) {
        state.window_started_at = Some(now);
        state.requests_in_window = 0;
    }
    if state.requests_in_window >= MAX_MODEL_REQUESTS_PER_DAY {
        return Some(None);
    }
    state.requests_in_window += 1;
    state.total_requests += 1;
    // Reserve before launching: cancellation must not turn a started request
    // into another request on the next observation.
    state.entries.insert(key.to_string(), CachedModelProbe { observed_at: now, fact: None });
    None
}

fn harness_version(stdout: &str) -> String {
    stdout
        .split_whitespace()
        .map(|token| token.strip_prefix('v').unwrap_or(token))
        .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or_default()
        .to_string()
}

pub(crate) async fn probe_kind(
    spec: &FulfilmentKindSpec,
    image: Option<&str>,
    pool_available: bool,
    runner: &dyn CommandRunner,
    env: &dyn EnvVars,
    scratch: &Path,
    model_probes: &mut ModelProbeState,
) -> FulfilmentFacts {
    let mut facts = FulfilmentFacts { image: image.map(ToString::to_string), observed_at: Utc::now(), ..FulfilmentFacts::default() };
    // This daemon-owned directory has no checkout. Never let a harness
    // discover a project from the root or the operator's home directory.
    let scratch_name = scratch.to_string_lossy();
    let Some(parent) = scratch.parent().filter(|parent| *parent != Path::new("/") && *parent != Path::new("")) else {
        return facts;
    };
    if env.get("HOME").is_some_and(|home| scratch == Path::new(&home)) {
        return facts;
    }
    if !runner.run_output("mkdir", &["-p", &scratch_name], parent, &ChannelLabel::Default).await.is_ok_and(|output| output.success) {
        return facts;
    }
    let mut image_digest = None;
    if matches!(spec.realisation, FulfilmentRealisation::DockerPerVessel { .. }) {
        let Some(image) = image else { return facts };
        let inspected = tokio::time::timeout(
            Duration::from_secs(15),
            runner.run_output("docker", &["image", "inspect", image], scratch, &ChannelLabel::Default),
        )
        .await
        .ok()
        .and_then(Result::ok);
        facts.image_present = inspected.as_ref().map(|output| output.success);
        image_digest = inspected.as_ref().and_then(|output| {
            serde_json::from_str::<serde_json::Value>(&output.stdout).ok()?.get(0)?.get("Id")?.as_str().map(ToString::to_string)
        });
        if facts.image_present != Some(true) {
            return facts;
        }
    }
    if matches!(spec.realisation, FulfilmentRealisation::HostDirect) {
        facts.gui_session_logged_in = env.get("DISPLAY").is_some() || env.get("WAYLAND_DISPLAY").is_some();
        if !facts.gui_session_logged_in && spec.grants.contains(&FulfilmentGrant::Platform("macos".to_string())) {
            // Aqua does not advertise a display variable. A logged-in user's
            // launchd GUI domain is the host-native signal for that session.
            if let Ok(output) = run_in_realisation(runner, &spec.realisation, image, "id", &["-u"], scratch).await {
                let uid = output.stdout.trim();
                if output.success && uid.parse::<u32>().is_ok() {
                    facts.gui_session_logged_in =
                        run_in_realisation(runner, &spec.realisation, image, "launchctl", &["print", &format!("gui/{uid}")], scratch)
                            .await
                            .is_ok_and(|session| session.success);
                }
            }
        }
    }
    // Terminal pools in the current fleet have no configured slot ceiling.
    // None means unbounded; an unavailable pool has zero usable slots.
    facts.free_vessel_slots = (!pool_available).then_some(0);
    for tool in TOOLCHAINS {
        if let Ok(output) = run_in_realisation(runner, &spec.realisation, image, tool, &["--version"], scratch).await {
            if output.success {
                let version = output.stdout.lines().next().or_else(|| output.stderr.lines().next()).unwrap_or_default().trim();
                if !version.is_empty() {
                    facts.toolchains.insert((*tool).to_string(), version.to_string());
                }
            }
        }
    }
    for (harness, binary) in HARNESSES {
        let Ok(output) = run_in_realisation(runner, &spec.realisation, image, binary, &["--version"], scratch).await else { continue };
        if !output.success {
            continue;
        }
        // `claude --version` prints "2.1.283 (Claude Code)" and `codex --version`
        // prints "codex-cli 0.157.1": take the first token that starts with a digit
        // (after an optional `v`).
        let version = harness_version(&output.stdout);
        if version.is_empty() {
            continue;
        }
        let mut observed = HarnessFacts { version, models: BTreeMap::new() };
        if *harness == "claude-code" {
            let credential = credential_fingerprint(runner, env, scratch).await;
            for model in declared_models(env) {
                let key =
                    format!("{harness}:{}:{}:{credential}:{model}", observed.version, image_digest.as_deref().or(image).unwrap_or("host"));
                if let Some(cached) = cached_or_budgeted_model(model_probes, &key, Utc::now()) {
                    if let Some(fact) = cached {
                        observed.models.insert(model, fact);
                    }
                    continue;
                }
                // A real one-turn request tests the installed harness and its
                // current credentials together. Failed process launches are
                // left unknown; an explicit CLI rejection is unusable.
                if let Ok(result) = run_in_realisation(
                    runner,
                    &spec.realisation,
                    image,
                    binary,
                    &["--model", &model, "--print", "OK", "--max-turns", "1", "--tools", "", "--setting-sources", "user"],
                    scratch,
                )
                .await
                {
                    let diagnostic = format!("{} {}", result.stdout, result.stderr).to_ascii_lowercase();
                    let rejected_model = !result.success
                        && diagnostic.contains("model")
                        && ["unsupported", "unknown", "invalid", "unavailable", "not found", "denied"]
                            .iter()
                            .any(|reason| diagnostic.contains(reason));
                    let fact =
                        (result.success || rejected_model).then_some(ModelFact { usable: result.success, source: ModelFactSource::Probe });
                    model_probes.entries.insert(key, CachedModelProbe { observed_at: Utc::now(), fact: fact.clone() });
                    if let Some(fact) = fact {
                        observed.models.insert(model, fact);
                    }
                } else {
                    model_probes.entries.insert(key, CachedModelProbe { observed_at: Utc::now(), fact: None });
                }
            }
        }
        facts.harnesses.insert((*harness).to_string(), observed);
    }
    facts
}

#[cfg(test)]
mod tests {
    #[test]
    fn harness_version_takes_the_first_numeric_token() {
        assert_eq!(super::harness_version("2.1.283 (Claude Code)\n"), "2.1.283");
        assert_eq!(super::harness_version("codex-cli 0.157.1\n"), "0.157.1");
        assert_eq!(super::harness_version("tool v2.1.300\n"), "2.1.300");
        assert_eq!(super::harness_version("no version here"), "");
    }

    use std::collections::BTreeSet;

    use flotilla_core::providers::discovery::test_support::{DiscoveryMockRunner, TestEnvVars};
    use flotilla_resources::{FulfilmentGrant, FulfilmentKindSpec};

    use super::*;

    fn docker_kind() -> FulfilmentKindSpec {
        FulfilmentKindSpec::builder()
            .host_ref("feta".to_string())
            .pool("cleat".to_string())
            .grants(BTreeSet::from([FulfilmentGrant::Platform("linux".to_string())]))
            .realisation(FulfilmentRealisation::DockerPerVessel { image: "crew:test".into() })
            .build()
    }

    #[tokio::test]
    async fn missing_image_is_a_fact_and_does_not_pull() {
        let runner = DiscoveryMockRunner::builder().on_run("mkdir", &["-p", "/tmp/flotilla-probe-test"], Ok(String::new())).build();
        let env = TestEnvVars::new([("FLOTILLA_PROBE_MODELS", "")]);
        let facts = probe_kind(
            &docker_kind(),
            Some("crew:missing"),
            true,
            &runner,
            &env,
            Path::new("/tmp/flotilla-probe-test"),
            &mut ModelProbeState::default(),
        )
        .await;
        assert_eq!(facts.image.as_deref(), Some("crew:missing"));
        assert_eq!(facts.image_present, Some(false));
        assert!(facts.harnesses.is_empty());
    }

    #[tokio::test]
    async fn image_and_host_versions_probe_different_model_availability() {
        let model = "claude-new-model";
        let env = TestEnvVars::new([("FLOTILLA_PROBE_MODELS", model), ("DISPLAY", ":0")]);
        let image = DiscoveryMockRunner::builder()
            .on_run("mkdir", &["-p", "/tmp/flotilla-probe-test"], Ok(String::new()))
            .on_run("docker", &["image", "inspect", "crew:test"], Ok(r#"[{"Id":"sha256:image-a"}]"#.into()))
            .on_run(
                "docker",
                &["run", "--rm", "--pull=never", "--workdir", "/probe", "--tmpfs", "/probe", "crew:test", "claude", "--version"],
                Ok("2.1.280 (Claude Code)".into()),
            )
            .on_run(
                "docker",
                &[
                    "run",
                    "--rm",
                    "--pull=never",
                    "--workdir",
                    "/probe",
                    "--tmpfs",
                    "/probe",
                    "crew:test",
                    "claude",
                    "--model",
                    model,
                    "--print",
                    "OK",
                    "--max-turns",
                    "1",
                    "--tools",
                    "",
                    "--setting-sources",
                    "user",
                ],
                Err("model unsupported".into()),
            )
            .build();
        let host = DiscoveryMockRunner::builder()
            .on_run("mkdir", &["-p", "/tmp/flotilla-probe-test"], Ok(String::new()))
            .on_run("rustc", &["--version"], Ok("rustc 1.94.1".into()))
            .on_run("claude", &["--version"], Ok("2.1.282 (Claude Code)".into()))
            .on_run(
                "claude",
                &["--model", model, "--print", "OK", "--max-turns", "1", "--tools", "", "--setting-sources", "user"],
                Ok("OK".into()),
            )
            .build();
        let docker_facts = probe_kind(
            &docker_kind(),
            Some("crew:test"),
            true,
            &image,
            &env,
            Path::new("/tmp/flotilla-probe-test"),
            &mut ModelProbeState::default(),
        )
        .await;
        let direct = FulfilmentKindSpec::builder()
            .host_ref("kiwi".to_string())
            .pool("cleat".to_string())
            .realisation(FulfilmentRealisation::HostDirect)
            .build();
        let host_facts =
            probe_kind(&direct, None, true, &host, &env, Path::new("/tmp/flotilla-probe-test"), &mut ModelProbeState::default()).await;
        assert_eq!(docker_facts.harnesses["claude-code"].version, "2.1.280");
        assert!(!docker_facts.harnesses["claude-code"].models[model].usable);
        assert_eq!(host_facts.harnesses["claude-code"].version, "2.1.282");
        assert!(host_facts.harnesses["claude-code"].models[model].usable);
        assert_eq!(host_facts.harnesses["claude-code"].models[model].source, ModelFactSource::Probe);
        assert_eq!(host_facts.toolchains["rustc"], "rustc 1.94.1");
        assert!(host_facts.gui_session_logged_in);
    }

    #[tokio::test]
    async fn macos_gui_session_uses_launchd_domain_through_the_runner() {
        let spec = FulfilmentKindSpec::builder()
            .host_ref("kiwi".to_string())
            .pool("cleat".to_string())
            .grants(BTreeSet::from([FulfilmentGrant::Platform("macos".to_string())]))
            .realisation(FulfilmentRealisation::HostDirect)
            .build();
        let env = TestEnvVars::new([("FLOTILLA_PROBE_MODELS", "")]);
        let logged_in = DiscoveryMockRunner::builder()
            .on_run("mkdir", &["-p", "/tmp/flotilla-probe-test"], Ok(String::new()))
            .on_run("id", &["-u"], Ok("501".into()))
            .on_run("launchctl", &["print", "gui/501"], Ok("gui/501 = { ... }".into()))
            .build();
        let logged_out = DiscoveryMockRunner::builder()
            .on_run("mkdir", &["-p", "/tmp/flotilla-probe-test"], Ok(String::new()))
            .on_run("id", &["-u"], Ok("501".into()))
            .on_run("launchctl", &["print", "gui/501"], Err("domain absent".into()))
            .build();
        assert!(
            probe_kind(&spec, None, true, &logged_in, &env, Path::new("/tmp/flotilla-probe-test"), &mut ModelProbeState::default())
                .await
                .gui_session_logged_in
        );
        assert!(
            !probe_kind(&spec, None, true, &logged_out, &env, Path::new("/tmp/flotilla-probe-test"), &mut ModelProbeState::default())
                .await
                .gui_session_logged_in
        );
    }
    #[derive(Default)]
    struct CountingRunner {
        version: std::sync::Mutex<String>,
        model_requests: std::sync::Mutex<Vec<(String, std::path::PathBuf)>>,
        harness_cwds: std::sync::Mutex<Vec<std::path::PathBuf>>,
    }

    #[async_trait::async_trait]
    impl CommandRunner for CountingRunner {
        async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            Err("unused".into())
        }

        async fn run_output(
            &self,
            cmd: &str,
            args: &[&str],
            cwd: &Path,
            _label: &ChannelLabel,
        ) -> Result<flotilla_core::providers::CommandOutput, String> {
            let stdout = match (cmd, args.first().copied()) {
                ("mkdir", _) => String::new(),
                ("claude", Some("--version")) => {
                    self.harness_cwds.lock().expect("cwd lock").push(cwd.to_path_buf());
                    self.version.lock().expect("version lock").clone()
                }
                ("claude", Some("--model")) => {
                    self.harness_cwds.lock().expect("cwd lock").push(cwd.to_path_buf());
                    self.model_requests.lock().expect("requests lock").push((args[1].to_string(), cwd.to_path_buf()));
                    "OK".into()
                }
                _ => return Err("unavailable".into()),
            };
            Ok(flotilla_core::providers::CommandOutput { stdout, stderr: String::new(), success: true })
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn model_requests_are_cached_by_version_and_run_only_in_scratch() {
        let runner = CountingRunner::default();
        *runner.version.lock().expect("version lock") = "2.1.282 (Claude Code)".into();
        let env = TestEnvVars::new([("ANTHROPIC_API_KEY", "credential-a")]);
        let spec = FulfilmentKindSpec::builder()
            .host_ref("kiwi".to_string())
            .pool("cleat".to_string())
            .realisation(FulfilmentRealisation::HostDirect)
            .build();
        let scratch = Path::new("/tmp/flotilla-test-state/probe-cwd");
        let mut state = ModelProbeState::default();
        let first = probe_kind(&spec, None, true, &runner, &env, scratch, &mut state).await;
        assert_eq!(first.harnesses["claude-code"].models.len(), 2);
        assert_eq!(state.total_requests, 2);
        state = serde_json::from_str(&serde_json::to_string(&state).expect("encode cache")).expect("restore cache");
        let second = probe_kind(&spec, None, true, &runner, &env, scratch, &mut state).await;
        assert_eq!(second.harnesses["claude-code"].models.len(), 2);
        assert_eq!(state.total_requests, 2);
        *runner.version.lock().expect("version lock") = "2.1.283 (Claude Code)".into();
        let changed = probe_kind(&spec, None, true, &runner, &env, scratch, &mut state).await;
        assert_eq!(changed.harnesses["claude-code"].models.len(), 2);
        assert_eq!(state.total_requests, 4);
        {
            let requests = runner.model_requests.lock().expect("requests lock");
            assert_eq!(requests.iter().filter(|(model, _)| model == "sonnet").count(), 2);
            assert_eq!(requests.iter().filter(|(model, _)| model == "opus").count(), 2);
            assert!(requests.iter().all(|(_, cwd)| cwd == scratch));
            assert!(runner.harness_cwds.lock().expect("cwd lock").iter().all(|cwd| cwd == scratch));
        }
        let changed_credential = TestEnvVars::new([("ANTHROPIC_API_KEY", "credential-b")]);
        probe_kind(&spec, None, true, &runner, &changed_credential, scratch, &mut state).await;
        assert_eq!(state.total_requests, 6);
        let restored: ModelProbeState = serde_json::from_str(&serde_json::to_string(&state).expect("encode cache")).expect("decode cache");
        assert_eq!(restored, state);
    }

    #[tokio::test]
    async fn model_request_ceiling_bounds_repeated_version_changes() {
        let runner = CountingRunner::default();
        let env = TestEnvVars::new([("ANTHROPIC_API_KEY", "credential-a")]);
        let spec = FulfilmentKindSpec::builder()
            .host_ref("kiwi".to_string())
            .pool("cleat".to_string())
            .realisation(FulfilmentRealisation::HostDirect)
            .build();
        let mut state = ModelProbeState::default();
        for version in 0..20 {
            *runner.version.lock().expect("version lock") = format!("2.1.{version} (Claude Code)");
            probe_kind(&spec, None, true, &runner, &env, Path::new("/tmp/flotilla-test-state/probe-cwd"), &mut state).await;
        }
        assert_eq!(state.total_requests, MAX_MODEL_REQUESTS_PER_DAY as u64);
        assert_eq!(runner.model_requests.lock().expect("requests lock").len(), MAX_MODEL_REQUESTS_PER_DAY as usize);
    }
}
