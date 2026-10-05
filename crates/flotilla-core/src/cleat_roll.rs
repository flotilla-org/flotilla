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

#[derive(Debug, Serialize, Deserialize, bon::Builder)]
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

#[derive(Debug, Serialize, Deserialize, bon::Builder)]
pub struct RollReport {
    pub host: String,
    pub generation: String,
    pub attempts: Vec<DrainAttempt>,
    pub errors: Vec<String>,
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
    let library_path;
    let executable;
    let command_binary = if target.contained && binary != target.binary {
        let library_dir = Path::new(binary).parent().and_then(Path::parent).ok_or("invalid delivered cleat path")?.join("lib");
        library_path = format!("LD_LIBRARY_PATH={}", library_dir.display());
        executable = binary.to_string();
        args.insert(0, &executable);
        args.insert(0, &library_path);
        "env"
    } else {
        binary
    };
    tokio::time::timeout(timeout, target.runner.run_output(command_binary, &args, Path::new("/"), &ChannelLabel::Default))
        .await
        .map_err(|_| format!("cleat command timed out after {} seconds", timeout.as_secs()))?
}

/// A contained environment has a pinned read-only CLI mount. Deliver the
/// incoming CLI into its durable runtime before asking it to start a successor;
/// running the host CLI against a container's state would spawn outside it.
async fn installed_binary(target: &CleatTarget, incoming: &Path, generation: &str) -> Result<String, String> {
    if !target.contained {
        return incoming.to_str().map(str::to_string).ok_or("installed cleat path is not UTF-8".into());
    }
    let directory = target.runtime_root.join(".fleet-bin").join(generation);
    let bin_dir = directory.join("bin");
    let lib_dir = directory.join("lib");
    let binary = bin_dir.join("cleat");
    let bin = bin_dir.to_str().ok_or("contained cleat directory is not UTF-8")?;
    let lib = lib_dir.to_str().ok_or("contained cleat library directory is not UTF-8")?;
    let path = binary.to_str().ok_or("contained cleat path is not UTF-8")?;
    target.runner.run("mkdir", &["-p", bin, lib], Path::new("/"), &ChannelLabel::Default).await?;
    target.runner.write_file_from(incoming, &binary).await?;
    // Fleet Linux packages carry this exact library; preserve bin/../lib and
    // override the container's previous LD_LIBRARY_PATH for the new CLI only.
    let incoming_lib =
        incoming.parent().and_then(Path::parent).ok_or("invalid incoming cleat package path")?.join("lib/libghostty-vt.so.0");
    target.runner.write_file_from(&incoming_lib, &lib_dir.join("libghostty-vt.so.0")).await?;
    target.runner.run("chmod", &["755", path], Path::new("/"), &ChannelLabel::Default).await?;
    Ok(path.to_string())
}

pub async fn drain(host: String, generation: String, incoming: &Path, targets: &[CleatTarget], errors: Vec<String>) -> RollReport {
    let mut report = RollReport::builder().host(host).generation(generation).attempts(vec![]).errors(errors).build();
    if logical_name(&report.generation).is_err() || report.generation.contains('@') {
        report.errors.push("invalid fleet generation".to_string());
        return report;
    }
    let mut delivered = BTreeMap::<(String, PathBuf), Result<String, String>>::new();
    for target in targets {
        let key = (target.environment.clone(), target.runtime_root.clone());
        let binary = if let Some(binary) = delivered.get(&key) {
            binary.clone()
        } else {
            let binary = installed_binary(target, incoming, &report.generation).await;
            delivered.insert(key, binary.clone());
            binary
        };
        let output = match binary {
            Ok(binary) => command(target, &binary, &["server", "drain", "--json"], Duration::from_secs(30)).await,
            Err(error) => Err(error),
        };
        let (stdout, stderr, success, mut error) = match output {
            Ok(output) => (output.stdout, output.stderr, output.success, None),
            Err(error) => (String::new(), String::new(), false, Some(error)),
        };
        let parsed = serde_json::from_str::<DrainReport>(&stdout);
        let parsed_report = match parsed {
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
        use tokio::io::AsyncWriteExt;
        file.write_all(&bytes).await.map_err(|error| error.to_string())
    }
    .await;
    if let Err(error) = result {
        report.diagnostics_path = None;
        report.errors.push(format!("write roll diagnostics {}: {error}", path.display()));
    }
}

/// Compare the host's installed build with current serving builds in every
/// known crew runtime. Missing build evidence is visible rather than current.
pub async fn build_skew(installed: Option<&str>, targets: &[CleatTarget]) -> Vec<String> {
    let mut messages = Vec::new();
    // Observe runtimes concurrently so unavailable containers do not multiply
    // the heartbeat's timeout by the number of crew environments.
    let results = futures::future::join_all(
        targets.iter().map(|target| command(target, &target.binary, &["version", "--daemon", "--json"], Duration::from_secs(5))),
    )
    .await;
    for (target, result) in targets.iter().zip(results) {
        let serving = result
            .as_ref()
            .ok()
            .filter(|output| output.success)
            .and_then(|output| serde_json::from_str::<Value>(&output.stdout).ok())
            .and_then(|value| value["daemon"]["git_sha"].as_str().map(str::to_string));
        if serving.as_deref().is_none() || installed.is_none() || serving.as_deref() != installed {
            messages.push(format!(
                "{} {}/{}: installed {} vs serving {}",
                target.environment,
                target.runtime_root.display(),
                target.name,
                installed.unwrap_or("unknown"),
                serving.as_deref().unwrap_or("unknown")
            ));
        }
    }
    messages
}

#[cfg(test)]
mod tests;
