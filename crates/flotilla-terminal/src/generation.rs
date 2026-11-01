//! Generation turnover: replacing or advancing a terminal backend generation
//! while its live sessions survive. Pools that offer it own every backend
//! detail; orchestration hands them environments and consumes these outcomes.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_protocol::result_set::CleatEndpoint;
use flotilla_provider_runtime::CommandRunner;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

/// One registered environment as a pool's turnover addresses it.
#[derive(Clone, bon::Builder)]
pub struct TurnoverEnvironment {
    pub id: String,
    pub bag: EnvironmentBag,
    pub runner: Arc<dyn CommandRunner>,
    pub contained: bool,
    /// Endpoints recorded for this pool's running sessions in the environment.
    #[builder(default)]
    pub endpoints: Vec<CleatEndpoint>,
    /// Vessels whose sessions run here, naming the environment in diagnostics.
    #[builder(default)]
    pub vessels: Vec<String>,
}

/// The installed build a turnover advances to.
#[derive(Clone, Debug)]
pub struct IncomingBuild {
    pub generation: String,
    /// The installed host executable, inside its release.
    pub installed: PathBuf,
    /// The verified guest build, or why contained environments cannot receive one.
    pub guest: Result<BTreeMap<String, PathBuf>, String>,
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

impl DrainAttempt {
    pub fn same_target(&self, other: &DrainAttempt) -> bool {
        self.environment == other.environment && self.runtime_root == other.runtime_root && self.name == other.name
    }
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

/// Retain successful and failed attempts alike. A storage failure is added to
/// the same report so the caller can still surface all captured evidence.
/// Fleet tooling reads these as `cleat-drain-*.json`.
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

/// What one pool's turnover did across its environments.
#[derive(Debug, Default)]
pub struct Turnover {
    pub attempts: Vec<DrainAttempt>,
    pub errors: Vec<String>,
    pub information: Vec<String>,
}

/// Health separates actionable host skew from expected old generations.
#[derive(Debug, Default)]
pub struct BuildAssessment {
    pub actionable: Vec<String>,
    pub information: Vec<String>,
}

#[async_trait]
pub trait GenerationTurnover: Send + Sync {
    /// The host condition that reports this backend's generation health.
    fn condition_type(&self) -> &'static str;

    /// Whether the environment declares this backend without any recorded session.
    fn declared(&self, bag: &EnvironmentBag) -> bool;

    /// Hand live sessions to the incoming generation. A failure leaves them on
    /// their current generation and is reported; sessions are never dropped.
    async fn advance(&self, environments: &[TurnoverEnvironment], incoming: &IncomingBuild) -> Turnover;

    /// Compare serving generations with the build installed in `host`; `None`
    /// when the host has no such build. `errors` are actionable inventory failures.
    async fn assess(
        &self,
        host: &TurnoverEnvironment,
        environments: &[TurnoverEnvironment],
        previous: Option<&RollReport>,
        errors: Vec<String>,
    ) -> Option<BuildAssessment>;
}
