//! Cleat generation turnover through registered execution environments.
//!
//! Targets come from Flotilla's environment bags and recorded crew endpoints,
//! never from a process-wide environment or a rediscovered daemon inventory.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_discovery_api::EnvironmentBag;
use serde_json::Value;

use crate::environment::ControlledTerminalEnvironment;
use crate::generation::{
    BuildAssessment, DrainAttempt, DrainReport, GenerationTurnover, IncomingBuild, RollReport, Turnover, TurnoverEnvironment,
};
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandOutput;
use flotilla_provider_runtime::CommandRunner;

/// Release composition roles of the Cleat executable and its loader-relative library.
const CLEAT_ROLE: &str = "cleat";
const LIBRARY_ROLE: &str = "cleat-library";

/// Cleat's generation turnover: descriptor handover to the installed daemon generation.
pub struct CleatGenerationTurnover;

#[async_trait]
impl GenerationTurnover for CleatGenerationTurnover {
    fn condition_type(&self) -> &'static str {
        "CleatBuildSkew"
    }

    fn declared(&self, bag: &EnvironmentBag) -> bool {
        bag.find_binary("cleat").is_some()
    }

    async fn advance(&self, environments: &[TurnoverEnvironment], incoming: &IncomingBuild) -> Turnover {
        let (targets, errors) = inventory(environments);
        drain(incoming, &targets, errors).await
    }

    async fn assess(
        &self,
        host: &TurnoverEnvironment,
        environments: &[TurnoverEnvironment],
        previous: Option<&RollReport>,
        errors: Vec<String>,
    ) -> Option<BuildAssessment> {
        let binary = host.bag.find_binary("cleat")?;
        let output = host
            .runner
            .run_with_timeout(
                &binary.as_path().display().to_string(),
                &["version", "--json"],
                Path::new("/"),
                &ChannelLabel::Default,
                Duration::from_secs(5),
            )
            .await;
        let installed = output
            .ok()
            .and_then(|output| serde_json::from_str::<Value>(&output).ok())
            .and_then(|value| value["client"]["git_sha"].as_str().map(str::to_string));
        let (targets, target_errors) = inventory(environments);
        let mut report = previous.cloned();
        if let Some(report) = &mut report {
            // A relaunched/destroyed environment no longer carries the failed
            // handover. Keep failures only for still-registered runtime targets.
            report.attempts.retain(|attempt| {
                targets.iter().any(|target| {
                    target.environment == attempt.environment && target.runtime_root == attempt.runtime_root && target.name == attempt.name
                })
            });
        }
        let mut assessment = build_skew(installed.as_deref(), &targets, report.as_ref()).await;
        assessment.actionable.extend(errors);
        assessment.actionable.extend(target_errors);
        if let Some(report) = report.as_ref() {
            assessment.actionable.extend(assess_drain(report).actionable);
        }
        Some(assessment)
    }
}

fn inventory(environments: &[TurnoverEnvironment]) -> (Vec<CleatTarget>, Vec<String>) {
    let mut targets = Vec::new();
    let mut errors = Vec::new();
    for environment in environments {
        match crew_targets(environment) {
            Ok(environment_targets) => targets.extend(environment_targets),
            Err(error) => errors.push(format!("{}: {error}", environment.id)),
        }
    }
    (targets, errors)
}

#[derive(Clone, bon::Builder)]
struct CleatTarget {
    environment: String,
    runtime_root: PathBuf,
    name: String,
    binary: String,
    runner: Arc<dyn CommandRunner>,
    contained: bool,
    #[builder(default)]
    vessels: Vec<String>,
}

/// Add default and ambient names, then recorded session endpoints in this
/// environment. A root/name pair is addressed once, independent of generation.
fn crew_targets(environment: &TurnoverEnvironment) -> Result<Vec<CleatTarget>, String> {
    let root = if let Some(root) = environment.bag.find_env_var("CLEAT_RUNTIME_DIR") {
        PathBuf::from(root)
    } else if let Some(state) = environment.bag.find_env_var("XDG_STATE_HOME").filter(|state| Path::new(state).is_absolute()) {
        PathBuf::from(state).join("cleat")
    } else {
        PathBuf::from(environment.bag.find_env_var("HOME").ok_or("cleat environment has no persistent runtime root")?)
            .join(".local/state/cleat")
    };
    let binary = environment.bag.find_binary("cleat").map_or_else(|| "cleat".to_string(), |path| path.as_path().display().to_string());
    let mut names = BTreeMap::new();
    names.insert((root.clone(), "default".to_string()), ());
    if let Some(name) = environment.bag.find_env_var("CLEAT_DAEMON") {
        names.insert((root, logical_name(name)?.to_string()), ());
    }
    for endpoint in &environment.endpoints {
        names.insert((PathBuf::from(&endpoint.runtime_root), logical_name(&endpoint.daemon)?.to_string()), ());
    }
    names
        .into_keys()
        .map(|(runtime_root, name)| {
            if !runtime_root.is_absolute() {
                return Err(format!("cleat runtime root must be absolute: {}", runtime_root.display()));
            }
            Ok(CleatTarget::builder()
                .environment(environment.id.to_string())
                .runtime_root(runtime_root)
                .name(name)
                .binary(binary.clone())
                .runner(ControlledTerminalEnvironment::from_bag(&environment.bag).runner(Arc::clone(&environment.runner)))
                .contained(environment.contained)
                .vessels(environment.vessels.clone())
                .build())
        })
        .collect()
}

fn logical_name(name: &str) -> Result<&str, String> {
    let logical = name.split('@').next().unwrap_or(name);
    if logical.is_empty()
        || matches!(logical, "." | "..")
        || !logical.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(format!("invalid cleat logical daemon: {name}"));
    }
    Ok(logical)
}

async fn command(target: &CleatTarget, binary: &str, verb: &[&str], timeout: Duration) -> Result<CommandOutput, String> {
    let root = target.runtime_root.to_str().ok_or("cleat runtime root is not UTF-8")?;
    let mut args = vec!["--runtime-root", root, "--server", target.name.as_str()];
    args.extend_from_slice(verb);
    tokio::time::timeout(timeout, target.runner.run_output(binary, &args, Path::new("/"), &ChannelLabel::Default))
        .await
        .map_err(|_| format!("cleat command timed out after {} seconds", timeout.as_secs()))?
}

async fn delivery_command(target: &CleatTarget, script: &str, args: Vec<&str>) -> Result<String, String> {
    let mut words = vec!["-c", script, "flotilla-cleat-delivery"];
    words.extend(args);
    target.runner.run_with_timeout("sh", &words, Path::new("/"), &ChannelLabel::Default, Duration::from_secs(30)).await
}

struct IncomingTools {
    cleat: PathBuf,
    library: PathBuf,
}

fn contained_incoming(guest: &Result<BTreeMap<String, PathBuf>, String>) -> Result<IncomingTools, String> {
    let files = guest.as_ref().map_err(String::clone)?;
    let file = |role: &str| files.get(role).cloned().ok_or_else(|| format!("incoming guest build has no {role} file"));
    Ok(IncomingTools { cleat: file(CLEAT_ROLE)?, library: file(LIBRARY_ROLE)? })
}

// Host-local tools already reside in the verified installed release, including
// its loader-relative library. Contained environments need a private payload
// reachable through their runner, independent of their original tool mounts.
async fn incoming_for_target(target: &CleatTarget, incoming: &IncomingBuild) -> Result<PathBuf, String> {
    if !target.contained {
        return Ok(incoming.installed.clone());
    }
    if !target.runtime_root.is_absolute() {
        return Err("cleat runtime root must be absolute".into());
    }
    let generation = incoming.generation.as_str();
    let tools = contained_incoming(&incoming.guest)?;
    let mut files = Vec::new();
    for (name, source) in [("bin/cleat", tools.cleat.as_path()), ("lib/libghostty-vt.so.0", tools.library.as_path())] {
        let bytes = tokio::fs::read(source).await.map_err(|error| format!("read incoming {}: {error}", source.display()))?;
        if bytes.is_empty() {
            return Err(format!("incoming {} is empty", source.display()));
        }
        files.push((name, bytes));
    }
    use sha2::{Digest, Sha256};
    let manifest = files.iter().map(|(name, bytes)| format!("{:x}  {name}", Sha256::digest(bytes))).collect::<Vec<_>>().join("\n");
    let parent = target.runtime_root.join(".fleet-bin");
    let destination = parent.join(generation);
    let temporary = parent.join(format!(".staging-{}", uuid::Uuid::new_v4()));
    let destination_str = destination.to_str().ok_or("cleat payload path is not UTF-8")?;
    let temporary_str = temporary.to_str().ok_or("cleat staging path is not UTF-8")?;
    // A same-generation directory is immutable: reuse only verified content.
    // An interrupted copy is never published under the generation name.
    const VERIFY: &str = "set -eu; cd \"$1\"; test -x bin/cleat; test \"$(sha256sum bin/cleat lib/libghostty-vt.so.0)\" = \"$2\"; bin/cleat version --json >/dev/null";
    if delivery_command(target, VERIFY, vec![destination_str, &manifest]).await.is_ok() {
        return Ok(destination.join("bin/cleat"));
    }
    let result = async {
        delivery_command(target, "set -eu; umask 077; mkdir -p \"$1/bin\" \"$1/lib\"", vec![temporary_str]).await?;
        for (name, bytes) in &files {
            let path = temporary.join(name);
            let path = path.to_str().ok_or("cleat staging file is not UTF-8")?;
            tokio::time::timeout(
                Duration::from_secs(30),
                target.runner.run_with_input(
                    "sh",
                    &["-c", "set -eu; cat > \"$1\"", "flotilla-cleat-copy", path],
                    Path::new("/"),
                    &ChannelLabel::Default,
                    bytes,
                ),
            )
            .await
            .map_err(|_| "cleat delivery timed out".to_string())??;
        }
        delivery_command(target, "set -eu; chmod 755 \"$1/bin/cleat\"; chmod 644 \"$1/lib/libghostty-vt.so.0\"", vec![temporary_str])
            .await?;
        delivery_command(target, VERIFY, vec![temporary_str, &manifest]).await?;
        // -T refuses to move a concurrent publisher's directory inside another
        // payload. A losing publisher may reuse only identical complete bytes.
        let published = delivery_command(target, "set -eu; mv -T \"$1\" \"$2\"", vec![temporary_str, destination_str]).await;
        if published.is_err() {
            delivery_command(target, VERIFY, vec![destination_str, &manifest]).await?;
        }
        Ok(destination.join("bin/cleat"))
    }
    .await;
    let _ = delivery_command(target, "rm -rf -- \"$1\"", vec![temporary_str]).await;
    result.map_err(|error: String| format!("deliver incoming cleat: {error}"))
}

/// Transfer live descriptors to the installed generation. Failure leaves old
/// sessions alive and becomes actionable CleatBuildSkew; never request drops.
async fn drain(incoming: &IncomingBuild, targets: &[CleatTarget], errors: Vec<String>) -> Turnover {
    let mut report = Turnover { errors, ..Turnover::default() };
    if logical_name(&incoming.generation).is_err() || incoming.generation.contains('@') {
        report.errors.push("invalid fleet generation".to_string());
        return report;
    }
    for target in targets {
        let selected_incoming = incoming_for_target(target, incoming).await;
        let output = match &selected_incoming {
            Ok(incoming) => match incoming.to_str() {
                Some(binary) => command(target, binary, &["server", "handover", "--json"], Duration::from_secs(30)).await,
                None => Err("installed cleat path is not UTF-8".into()),
            },
            Err(error) => Err(error.clone()),
        };
        let (stdout, stderr, success, mut error) = match output {
            Ok(output) => {
                let success = output.success();
                (output.stdout, output.stderr, success, None)
            }
            Err(error) => (String::new(), String::new(), false, Some(error)),
        };
        // Execution failures take precedence over parsing and report warnings.
        // Parse failures are errors only for successful commands; warnings are
        // surfaced only after successful execution and valid report decoding.
        // Raw output and any decoded report remain retained in every case.
        let decoded = serde_json::from_str::<DrainReport>(&stdout);
        let parsed_report = match decoded {
            Ok(parsed) => Some(parsed),
            Err(parse_error) => {
                if error.is_none() && success {
                    error = Some(format!("invalid drain JSON: {parse_error}"));
                }
                None
            }
        };
        if error.is_none() && !success {
            error = Some(format!("cleat handover exited unsuccessfully: {stderr}"));
        }
        if error.is_none() {
            error = parsed_report.as_ref().and_then(|report| report.warning.clone());
        }
        // Handover adds per-session results to the existing drain shape. Raw
        // output remains durable; supplement its error without changing stored
        // decoded reports, which must still accept the previous generation.
        if let Ok(value) = serde_json::from_str::<Value>(&stdout) {
            let mut failures = Vec::new();
            for session in value["stayed"].as_array().into_iter().flatten() {
                failures.push(format!(
                    "{} stayed: {}",
                    session["session_id"].as_str().unwrap_or("unknown"),
                    session["reason"].as_str().unwrap_or("handover refused")
                ));
            }
            for session in value["moved"].as_array().into_iter().flatten() {
                if let Some(warning) = session["warning"].as_str() {
                    if parsed_report.as_ref().and_then(|report| report.warning.as_deref()) != Some(warning) {
                        let message = format!("{}: {warning}", session["session_id"].as_str().unwrap_or("unknown"));
                        if !failures.contains(&message) {
                            failures.push(message);
                        }
                    }
                }
            }
            if !failures.is_empty() {
                let mut message = error.take().unwrap_or_default();
                if !message.is_empty() {
                    message.push_str("; ");
                }
                message.push_str(&failures.join("; "));
                error = Some(message);
            }
        }
        // Discovery matches registered roots literally. A missing current
        // socket does not imply failure: older generations can retain sessions
        // after a roll, even when nothing has started the current generation.
        // Read-only discovery avoids rotating an already-draining generation.
        if !success
            && stderr.starts_with("connect daemon:")
            && (stderr.contains("No such file or directory") || stderr.contains("Connection refused"))
        {
            let listing = match selected_incoming.as_ref().ok().and_then(|path| path.to_str()) {
                Some(binary) => parsed(command(target, binary, &["daemons", "--json"], Duration::from_secs(5)).await),
                None => None,
            };
            if let Some(daemons) = listing.as_ref().and_then(Value::as_array) {
                let live: Vec<_> = daemons
                    .iter()
                    .filter(|daemon| {
                        daemon["alive"] == true
                            && daemon["runtime_root"].as_str() == target.runtime_root.to_str()
                            && daemon["name"].as_str().and_then(|name| logical_name(name).ok()) == Some(target.name.as_str())
                    })
                    .collect();
                error = None;
                if live.is_empty() {
                    report.information.push(format!(
                        "{} {}/{}: no running host cleat daemon",
                        target.label(),
                        target.runtime_root.display(),
                        target.name
                    ));
                } else {
                    for daemon in live {
                        report.information.push(format!(
                            "{} {}/{}: current socket unavailable; waiting for sessions to end on {} ({})",
                            target.label(),
                            target.runtime_root.display(),
                            target.name,
                            daemon["name"].as_str().unwrap_or("unknown"),
                            daemon["drain_state"].as_str().unwrap_or("unknown drain state")
                        ));
                    }
                }
            }
        }
        report.attempts.push(
            DrainAttempt::builder()
                .environment(target.environment.clone())
                .runtime_root(target.runtime_root.clone())
                .name(target.name.clone())
                .stdout(stdout)
                .stderr(stderr)
                .success(success)
                .maybe_report(parsed_report)
                .maybe_error(error)
                .build(),
        );
    }
    report
}

impl CleatTarget {
    fn label(&self) -> String {
        if self.vessels.is_empty() {
            self.environment.clone()
        } else {
            self.vessels.join(", ")
        }
    }
}

fn sha(build: &Value) -> Option<&str> {
    build["git_sha"].as_str().filter(|sha| !sha.is_empty() && *sha != "unknown")
}

/// Derive health from the current alias observation, never the oldest listed
/// daemon or the `old` generation in a successful drain report.
fn assess_current(installed: Option<&str>, target: &CleatTarget, current: &Value) -> BuildAssessment {
    let mut assessment = BuildAssessment::default();
    let serving = sha(&current["daemon"]);
    let installed = installed.filter(|sha| !sha.is_empty() && *sha != "unknown");
    if let (Some(installed), Some(serving)) = (installed, serving) {
        if installed != serving {
            assessment.actionable.push(format!(
                "{} {}/{}: installed {} vs serving {}",
                target.label(),
                target.runtime_root.display(),
                target.name,
                installed,
                serving
            ));
        }
    } else {
        assessment.information.push(format!(
            "{} {}/{}: host cleat build unavailable (installed {}, serving {})",
            target.label(),
            target.runtime_root.display(),
            target.name,
            installed.unwrap_or("unknown"),
            serving.unwrap_or("unknown")
        ));
    }
    assessment
}

/// Failed host drain attempts remain actionable even when no build can be
/// observed. Expected inventory/skip observations remain informational.
fn assess_drain(report: &RollReport) -> BuildAssessment {
    let mut assessment = BuildAssessment { actionable: report.errors.clone(), information: report.information.clone() };
    for attempt in &report.attempts {
        if let Some(error) = &attempt.error {
            assessment.actionable.push(format!(
                "{} {}/{}: cleat handover failed: {}",
                attempt.environment,
                attempt.runtime_root.display(),
                attempt.name,
                error
            ));
        }
    }
    assessment
}

fn parsed(output: Result<CommandOutput, String>) -> Option<Value> {
    output.ok().filter(|output| output.success()).and_then(|output| serde_json::from_str(&output.stdout).ok())
}

/// Observe the alias explicitly. This also avoids any older CLI's ambient
/// generation selection. Legacy directories and non-Unix aliases fall back to
/// the CLI's logical-name resolution; no observation starts a daemon.
async fn current_target(target: &CleatTarget) -> CleatTarget {
    let mut current = target.clone();
    for alias in [target.name.clone(), format!(".{}.current", target.name)] {
        let path = target.runtime_root.join(alias);
        let Some(path) = path.to_str() else { continue };
        let result =
            target.runner.run_with_timeout("readlink", &[path], Path::new("/"), &ChannelLabel::Default, Duration::from_secs(5)).await;
        if let Ok(name) = result {
            let name = name.trim();
            if name.starts_with(&format!("{}@", target.name))
                && logical_name(name).is_ok()
                && name.split_once('@').is_some_and(|(_, generation)| generation.parse::<u64>().is_ok())
            {
                current.name = name.to_string();
                break;
            }
        }
    }
    current
}

async fn observe(installed: Option<&str>, target: &CleatTarget, report: Option<&RollReport>) -> BuildAssessment {
    let current = current_target(target).await;
    let version =
        parsed(command(&current, &target.binary, &["version", "--daemon", "--json"], Duration::from_secs(5)).await).unwrap_or(Value::Null);
    let mut assessment = assess_current(installed, target, &version);
    let listing = parsed(command(target, &target.binary, &["daemons", "--json"], Duration::from_secs(5)).await);
    for daemon in listing.as_ref().and_then(Value::as_array).into_iter().flatten() {
        let Some(name) = daemon["name"].as_str() else { continue };
        if daemon["alive"] != true
            || daemon["drain_state"] != "draining"
            || daemon["runtime_root"].as_str() != target.runtime_root.to_str()
            || logical_name(name).ok() != Some(target.name.as_str())
        {
            continue;
        }
        // `cleat list` may start/adopt daemons or sweep recordings, so health
        // observations use only the read-only listing and captured drain count.
        let count = report
            .into_iter()
            .flat_map(|report| &report.attempts)
            .filter(|attempt| {
                attempt.environment == target.environment && attempt.runtime_root == target.runtime_root && attempt.name == target.name
            })
            .filter_map(|attempt| attempt.report.as_ref())
            .find(|report| report.old["name"].as_str() == Some(name))
            .and_then(|report| report.old["session_count"].as_u64());
        // A partial handover can leave live sessions behind a healthy current
        // alias. Observe the old daemon's read-only endpoint: `list`/`inspect`
        // may start daemons, and a surviving child reaper alone is not skew.
        if target.contained
            && installed.is_some_and(|installed| sha(&daemon["build"]).is_some_and(|serving| installed != serving))
            && name.split_once('@').is_some_and(|(_, generation)| generation.parse::<u64>().is_ok())
        {
            let socket = target.runtime_root.join(name).join("socket");
            let status = match socket.to_str() {
                Some(socket) => target
                    .runner
                    .run_with_timeout(
                        "curl",
                        &[
                            "--disable",
                            "--noproxy",
                            "*",
                            "--max-time",
                            "4",
                            "--fail",
                            "--silent",
                            "--unix-socket",
                            socket,
                            "http://localhost/healthz",
                        ],
                        Path::new("/"),
                        &ChannelLabel::Default,
                        Duration::from_secs(5),
                    )
                    .await
                    .map_err(|error| format!("old daemon health probe unavailable: {error}"))
                    .and_then(|body| {
                        serde_json::from_str::<Value>(&body).map_err(|error| format!("invalid old daemon health JSON: {error}"))
                    })
                    .and_then(|status| {
                        status["session_count"].as_u64().ok_or_else(|| "old daemon health response lacks session_count".into())
                    }),
                None => Err("old daemon socket path is not UTF-8".into()),
            };
            match status {
                Ok(remaining) if remaining > 0 => assessment.actionable.push(format!(
                    "{} {}/{}: {remaining} sessions remain on old cleat {}; relaunch to clear build skew",
                    target.label(),
                    target.runtime_root.display(),
                    name,
                    sha(&daemon["build"]).unwrap_or("unknown")
                )),
                Ok(_) => {}
                Err(error) => assessment.actionable.push(format!(
                    "{} {}/{}: cannot confirm old generation has no sessions ({error})",
                    target.label(),
                    target.runtime_root.display(),
                    name
                )),
            }
        }
        let count_note = if count.is_some() { " (count at drain)" } else { "" };
        let count = count.map_or("unknown".to_string(), |count| count.to_string());
        assessment.information.push(format!(
            "{} {}/{}: draining {} sessions on {}{}",
            target.label(),
            target.runtime_root.display(),
            name,
            count,
            sha(&daemon["build"]).unwrap_or("unknown"),
            count_note
        ));
    }
    assessment
}

async fn build_skew(installed: Option<&str>, targets: &[CleatTarget], report: Option<&RollReport>) -> BuildAssessment {
    let mut assessment = BuildAssessment::default();
    // Observe runtimes concurrently so unavailable vessels do not multiply the
    // heartbeat's timeout by the number of environments.
    for observed in futures::future::join_all(targets.iter().map(|target| observe(installed, target, report))).await {
        assessment.actionable.extend(observed.actionable);
        assessment.information.extend(observed.information);
    }
    assessment
}

#[cfg(test)]
mod tests;
