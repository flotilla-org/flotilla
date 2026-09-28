use std::{collections::BTreeMap, path::Path, time::Duration};

use chrono::Utc;
use flotilla_core::providers::{discovery::EnvVars, ChannelLabel, CommandRunner};
use flotilla_resources::{
    FulfilmentFacts, FulfilmentGrant, FulfilmentKindSpec, FulfilmentRealisation, HarnessFacts, ModelFact, ModelFactSource,
};

// These are the fleet's candidate lists, not claims that every host can run
// them. Operators may extend the model list through the injected environment.
const DEFAULT_MODELS: &[&str] = &["sonnet", "opus"];
const TOOLCHAINS: &[&str] = &["rustc", "cargo", "node", "python3"];
const HARNESSES: &[(&str, &str)] = &[("claude-code", "claude"), ("codex", "codex")];

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
) -> Result<flotilla_core::providers::CommandOutput, String> {
    let cwd = Path::new("/");
    let label = ChannelLabel::Default;
    tokio::time::timeout(Duration::from_secs(15), async {
        match realisation {
            FulfilmentRealisation::HostDirect => runner.run_output(binary, args, cwd, &label).await,
            FulfilmentRealisation::DockerPerVessel { .. } => {
                let image = image.ok_or("docker image is unresolved")?;
                let mut command = vec!["run", "--rm", "--pull=never", image, binary];
                command.extend_from_slice(args);
                runner.run_output("docker", &command, cwd, &label).await
            }
        }
    })
    .await
    .map_err(|_| format!("timed out probing {binary}"))?
}

pub(crate) async fn probe_kind(
    spec: &FulfilmentKindSpec,
    image: Option<&str>,
    pool_available: bool,
    runner: &dyn CommandRunner,
    env: &dyn EnvVars,
) -> FulfilmentFacts {
    let mut facts = FulfilmentFacts { image: image.map(ToString::to_string), observed_at: Utc::now(), ..FulfilmentFacts::default() };
    if matches!(spec.realisation, FulfilmentRealisation::DockerPerVessel { .. }) {
        let Some(image) = image else { return facts };
        let present = tokio::time::timeout(
            Duration::from_secs(15),
            runner.run_output("docker", &["image", "inspect", image], Path::new("/"), &ChannelLabel::Default),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .map(|output| output.success);
        facts.image_present = present;
        if present != Some(true) {
            return facts;
        }
    }
    if matches!(spec.realisation, FulfilmentRealisation::HostDirect) {
        facts.gui_session_logged_in = env.get("DISPLAY").is_some() || env.get("WAYLAND_DISPLAY").is_some();
        if !facts.gui_session_logged_in && spec.grants.contains(&FulfilmentGrant::Platform("macos".to_string())) {
            // Aqua does not advertise a display variable. A logged-in user's
            // launchd GUI domain is the host-native signal for that session.
            if let Ok(output) = run_in_realisation(runner, &spec.realisation, image, "id", &["-u"]).await {
                let uid = output.stdout.trim();
                if output.success && uid.parse::<u32>().is_ok() {
                    facts.gui_session_logged_in =
                        run_in_realisation(runner, &spec.realisation, image, "launchctl", &["print", &format!("gui/{uid}")])
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
        if let Ok(output) = run_in_realisation(runner, &spec.realisation, image, tool, &["--version"]).await {
            if output.success {
                let version = output.stdout.lines().next().or_else(|| output.stderr.lines().next()).unwrap_or_default().trim();
                if !version.is_empty() {
                    facts.toolchains.insert((*tool).to_string(), version.to_string());
                }
            }
        }
    }
    for (harness, binary) in HARNESSES {
        let Ok(output) = run_in_realisation(runner, &spec.realisation, image, binary, &["--version"]).await else { continue };
        if !output.success {
            continue;
        }
        let version = output.stdout.split_whitespace().next().unwrap_or_default().to_string();
        if version.is_empty() {
            continue;
        }
        let mut observed = HarnessFacts { version, models: BTreeMap::new() };
        if *harness == "claude-code" {
            for model in declared_models(env) {
                // A real one-turn request tests the installed harness and its
                // current credentials together. Failed process launches are
                // left unknown; an explicit CLI rejection is unusable.
                if let Ok(result) = run_in_realisation(runner, &spec.realisation, image, binary, &[
                    "--model",
                    &model,
                    "--print",
                    "OK",
                    "--max-turns",
                    "1",
                    "--tools",
                    "",
                ])
                .await
                {
                    let diagnostic = format!("{} {}", result.stdout, result.stderr).to_ascii_lowercase();
                    let rejected_model = !result.success
                        && diagnostic.contains("model")
                        && ["unsupported", "unknown", "invalid", "unavailable", "not found", "denied"]
                            .iter()
                            .any(|reason| diagnostic.contains(reason));
                    if result.success || rejected_model {
                        observed.models.insert(model, ModelFact { usable: result.success, source: ModelFactSource::Probe });
                    }
                }
            }
        }
        facts.harnesses.insert((*harness).to_string(), observed);
    }
    facts
}

#[cfg(test)]
mod tests {
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
        let runner = DiscoveryMockRunner::builder().build();
        let env = TestEnvVars::new([("FLOTILLA_PROBE_MODELS", "")]);
        let facts = probe_kind(&docker_kind(), Some("crew:missing"), true, &runner, &env).await;
        assert_eq!(facts.image.as_deref(), Some("crew:missing"));
        assert_eq!(facts.image_present, Some(false));
        assert!(facts.harnesses.is_empty());
    }

    #[tokio::test]
    async fn image_and_host_versions_probe_different_model_availability() {
        let model = "claude-new-model";
        let env = TestEnvVars::new([("FLOTILLA_PROBE_MODELS", model), ("DISPLAY", ":0")]);
        let image = DiscoveryMockRunner::builder()
            .on_run("docker", &["image", "inspect", "crew:test"], Ok("[]".into()))
            .on_run("docker", &["run", "--rm", "--pull=never", "crew:test", "claude", "--version"], Ok("2.1.280 (Claude Code)".into()))
            .on_run(
                "docker",
                &[
                    "run",
                    "--rm",
                    "--pull=never",
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
                ],
                Err("model unsupported".into()),
            )
            .build();
        let host = DiscoveryMockRunner::builder()
            .on_run("rustc", &["--version"], Ok("rustc 1.94.1".into()))
            .on_run("claude", &["--version"], Ok("2.1.282 (Claude Code)".into()))
            .on_run("claude", &["--model", model, "--print", "OK", "--max-turns", "1", "--tools", ""], Ok("OK".into()))
            .build();
        let docker_facts = probe_kind(&docker_kind(), Some("crew:test"), true, &image, &env).await;
        let direct = FulfilmentKindSpec::builder()
            .host_ref("kiwi".to_string())
            .pool("cleat".to_string())
            .realisation(FulfilmentRealisation::HostDirect)
            .build();
        let host_facts = probe_kind(&direct, None, true, &host, &env).await;
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
            .on_run("id", &["-u"], Ok("501".into()))
            .on_run("launchctl", &["print", "gui/501"], Ok("gui/501 = { ... }".into()))
            .build();
        let logged_out = DiscoveryMockRunner::builder()
            .on_run("id", &["-u"], Ok("501".into()))
            .on_run("launchctl", &["print", "gui/501"], Err("domain absent".into()))
            .build();
        assert!(probe_kind(&spec, None, true, &logged_in, &env).await.gui_session_logged_in);
        assert!(!probe_kind(&spec, None, true, &logged_out, &env).await.gui_session_logged_in);
    }
}
