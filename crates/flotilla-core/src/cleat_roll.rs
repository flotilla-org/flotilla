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

use flotilla_protocol::{result_set::CleatEndpoint, EnvironmentId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::providers::{discovery::EnvironmentBag, ChannelLabel, CommandOutput, CommandRunner};

#[derive(bon::Builder)]
pub struct CleatEnvironment {
    pub id: EnvironmentId,
    pub bag: EnvironmentBag,
    pub runner: Arc<dyn CommandRunner>,
    pub contained: bool,
}

#[derive(Clone, bon::Builder)]
pub struct CleatTarget {
    pub environment: String,
    pub runtime_root: PathBuf,
    pub name: String,
    pub binary: String,
    pub runner: Arc<dyn CommandRunner>,
    pub contained: bool,
    #[builder(default)]
    pub vessels: Vec<String>,
}

/// Add default and ambient names, then recorded session endpoints in this
/// environment. A root/name pair is addressed once, independent of generation.
pub fn crew_targets(environment: &CleatEnvironment, endpoints: &[CleatEndpoint]) -> Result<Vec<CleatTarget>, String> {
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
    for endpoint in endpoints {
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
                .runner(Arc::clone(&environment.runner))
                .contained(environment.contained)
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrainReport {
    pub changed: bool,
    pub installed: Value,
    pub old: Value,
    pub current: Value,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
pub struct DrainAttempt {
    pub environment: String,
    pub runtime_root: PathBuf,
    pub name: String,
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub report: Option<DrainReport>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
pub struct RollReport {
    pub host: String,
    pub generation: String,
    pub attempts: Vec<DrainAttempt>,
    pub errors: Vec<String>,
    /// ADR 0047: absent in previous roll reports; remove default after one roll.
    #[serde(default)]
    #[builder(default)]
    pub information: Vec<String>,
    pub diagnostics_path: Option<PathBuf>,
}

impl RollReport {
    pub fn failed(&self) -> bool {
        !self.errors.is_empty() || self.attempts.iter().any(|attempt| attempt.error.is_some())
    }
}

async fn command(target: &CleatTarget, binary: &str, verb: &[&str], timeout: Duration) -> Result<CommandOutput, String> {
    let root = target.runtime_root.to_str().ok_or("cleat runtime root is not UTF-8")?;
    let mut args = vec!["--runtime-root", root, "--server", target.name.as_str()];
    args.extend_from_slice(verb);
    tokio::time::timeout(timeout, target.runner.run_output(binary, &args, Path::new("/"), &ChannelLabel::Default))
        .await
        .map_err(|_| format!("cleat command timed out after {} seconds", timeout.as_secs()))?
}

/// Vessels keep their launch-time toolchain until restart. A roll updates only
/// host-direct daemons: runner availability and a copied CLI/library do not prove
/// compatibility with an already provisioned vessel's pinned environment.
pub async fn drain(host: String, generation: String, incoming: &Path, targets: &[CleatTarget], errors: Vec<String>) -> RollReport {
    let mut report = RollReport::builder().host(host).generation(generation).attempts(vec![]).errors(errors).build();
    if logical_name(&report.generation).is_err() || report.generation.contains('@') {
        report.errors.push("invalid fleet generation".to_string());
        return report;
    }
    for target in targets {
        if target.contained {
            report.information.push(format!("{}: vessel cleat refreshes on restart", target.label()));
            continue;
        }
        let output = match incoming.to_str() {
            Some(binary) => command(target, binary, &["server", "drain", "--json"], Duration::from_secs(30)).await,
            None => Err("installed cleat path is not UTF-8".into()),
        };
        let (stdout, stderr, success, mut error) = match output {
            Ok(output) => (output.stdout, output.stderr, output.success, None),
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
            error = Some(format!("cleat drain exited unsuccessfully: {stderr}"));
        }
        if error.is_none() {
            error = parsed_report.as_ref().and_then(|report| report.warning.clone());
        }
        // Discovery matches roots literally: alternate spellings can miss a live
        // daemon and suppress this connection failure, not just draining information.
        if !success && stderr.starts_with("connect daemon:") {
            let listing = parsed(command(target, &target.binary, &["daemons", "--json"], Duration::from_secs(5)).await);
            if listing.as_ref().and_then(Value::as_array).is_some_and(|daemons| {
                !daemons.iter().any(|daemon| {
                    daemon["alive"] == true
                        && daemon["runtime_root"].as_str() == target.runtime_root.to_str()
                        && daemon["name"].as_str().and_then(|name| logical_name(name).ok()) == Some(target.name.as_str())
                })
            }) {
                error = None;
                report.information.push(format!(
                    "{} {}/{}: no running host cleat daemon",
                    target.label(),
                    target.runtime_root.display(),
                    target.name
                ));
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

/// Retain successful and failed attempts alike. A storage failure is added to
/// the same report so the caller can still surface all captured CLI evidence.
pub async fn persist(report: &mut RollReport, directory: &Path) {
    let path = directory.join(format!("cleat-drain-{}.json", uuid::Uuid::new_v4()));
    report.diagnostics_path = Some(path.clone());
    let result: Result<(), String> = async {
        tokio::fs::create_dir_all(directory).await.map_err(|error| error.to_string())?;
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&path).await.map_err(|error| error.to_string())?;
        let bytes = serde_json::to_vec_pretty(report).map_err(|error| error.to_string())?;
        file.write_all(&bytes).await.map_err(|error| error.to_string())?;
        // Tokio file writes may still be buffered; the returned path must be
        // readable immediately by a caller using synchronous filesystem I/O.
        file.flush().await.map_err(|error| error.to_string())
    }
    .await;
    if let Err(error) = result {
        report.diagnostics_path = None;
        report.errors.push(format!("write roll diagnostics {}: {error}", path.display()));
    }
}

/// Health separates actionable host skew from expected old generations.
#[derive(Debug, Default)]
pub struct BuildAssessment {
    pub actionable: Vec<String>,
    pub information: Vec<String>,
}

impl BuildAssessment {
    pub fn into_condition(self, observed_at: chrono::DateTime<chrono::Utc>) -> Option<flotilla_resources::HostCondition> {
        let degraded = !self.actionable.is_empty();
        let mut messages = self.actionable;
        messages.extend(self.information);
        if messages.is_empty() {
            return None;
        }
        Some(
            flotilla_resources::HostCondition::builder()
                .condition_type("CleatBuildSkew")
                .value(if degraded { flotilla_resources::ConditionValue::False } else { flotilla_resources::ConditionValue::True })
                .reason(if degraded { "InstalledServingMismatch" } else { "ExpectedGenerationSkew" })
                .message(messages.join("; "))
                .observed_at(observed_at)
                .blocks_readiness(false)
                .build(),
        )
    }
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
pub fn assess_current(installed: Option<&str>, target: &CleatTarget, current: &Value) -> BuildAssessment {
    let mut assessment = BuildAssessment::default();
    let serving = sha(&current["daemon"]);
    let installed = installed.filter(|sha| !sha.is_empty() && *sha != "unknown");
    if target.contained {
        assessment.information.push(format!("{}: vessel cleat {}, refreshes on restart", target.label(), serving.unwrap_or("unknown")));
    } else if let (Some(installed), Some(serving)) = (installed, serving) {
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
pub fn assess_drain(report: &RollReport) -> BuildAssessment {
    let mut assessment = BuildAssessment { actionable: report.errors.clone(), information: report.information.clone() };
    for attempt in &report.attempts {
        if let Some(error) = &attempt.error {
            assessment.actionable.push(format!(
                "{} {}/{}: host cleat drain failed: {}",
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
    output.ok().filter(|output| output.success).and_then(|output| serde_json::from_str(&output.stdout).ok())
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

pub async fn build_skew(installed: Option<&str>, targets: &[CleatTarget], report: Option<&RollReport>) -> BuildAssessment {
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
