//! Bounded, shared controller credential checks; diagnostics never retain tool output.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_provider_runtime::{default_channel_label, CommandRunner};
use flotilla_resources::{ForgeKind, ForgeSpec, RepositoryIdentity, RepositorySpec};
use tokio::time::Instant;

const MAX_AGE: Duration = Duration::from_secs(30);

/// The process boundary for checking the identity currently used by GitHub observations.
#[async_trait]
pub trait ObservationCredentialProbe: Send + Sync {
    async fn available(&self, runner: &dyn CommandRunner, host: &str) -> bool;
}

struct GhAuthProbe;
#[async_trait]
impl ObservationCredentialProbe for GhAuthProbe {
    async fn available(&self, runner: &dyn CommandRunner, host: &str) -> bool {
        runner
            .run_output("gh", &["auth", "status", "--hostname", host], Path::new("/"), &default_channel_label())
            .await
            .is_ok_and(|output| output.exit_code == Some(0))
    }
}

struct CheckedCredential {
    at: Instant,
    result: Result<(), String>,
}

pub struct ObservationCredentials {
    probe: Arc<dyn ObservationCredentialProbe>,
    checked: Mutex<BTreeMap<String, CheckedCredential>>,
    flight: tokio::sync::Mutex<()>,
}

impl Default for ObservationCredentials {
    fn default() -> Self {
        Self::new(Arc::new(GhAuthProbe))
    }
}

impl ObservationCredentials {
    pub fn new(probe: Arc<dyn ObservationCredentialProbe>) -> Self {
        Self { probe, checked: Mutex::new(BTreeMap::new()), flight: tokio::sync::Mutex::new(()) }
    }

    pub fn repository_services<'a>(
        repositories: impl IntoIterator<Item = &'a RepositorySpec>,
        forges: &[ForgeSpec],
    ) -> Result<BTreeSet<String>, String> {
        let mut services = BTreeSet::new();
        for repository in repositories {
            if let RepositoryIdentity::Forge { forge_ref, .. } = repository.identity() {
                let forge =
                    forges.iter().find(|forge| &forge.forge_id == forge_ref).ok_or_else(|| format!("Forge {forge_ref} unavailable"))?;
                services.insert(forge.https_url.trim_end_matches('/').to_string());
            }
            services.extend(repository.forge().map(|forge| forge.service_url.clone()));
            services.extend(repository.issue_source_forge().map(|forge| forge.service_url));
        }
        Ok(services)
    }

    pub fn github_service(service: &str, forges: &[ForgeSpec]) -> bool {
        forges
            .iter()
            .find(|forge| forge.owns_issue_service(service))
            .map_or(service.trim_end_matches('/') == "https://github.com", |forge| forge.kind == ForgeKind::Github)
    }

    pub async fn require_for_bag(&self, runner: &dyn CommandRunner, bag: &EnvironmentBag) -> Result<(), String> {
        if bag.uses_forgejo() {
            return Ok(());
        }
        let service = match bag.find_origin_forge() {
            Some(forge) if forge.kind == ForgeKind::Github => forge.https_url.as_str(),
            _ if bag.find_origin_remote().is_some_and(|(host, _, _)| host.eq_ignore_ascii_case("github.com")) => "https://github.com",
            _ => return Ok(()),
        };
        self.require(runner, service).await
    }

    pub async fn require(&self, runner: &dyn CommandRunner, service: &str) -> Result<(), String> {
        // Coalesce admission, issue and change-request checks. Surface reads only
        // inspect the completed cache and never wait for this process boundary.
        let _flight = self.flight.lock().await;
        let service = service.trim_end_matches('/');
        if let Some(cached) = self.checked.lock().expect("credential checks").get(service).filter(|cached| cached.at.elapsed() < MAX_AGE) {
            return cached.result.clone();
        }
        let host = service.strip_prefix("https://").and_then(|rest| rest.split('/').next()).unwrap_or("");
        let available =
            !host.is_empty() && tokio::time::timeout(Duration::from_secs(2), self.probe.available(runner, host)).await.unwrap_or(false);
        let result = if available {
            Ok(())
        } else {
            Err(format!(
            "GitHub observation credential unavailable for {service}: controller gh login is missing, invalid or could not be checked; configure or repair the controller's GitHub observation credential"
        ))
        };
        self.checked
            .lock()
            .expect("credential checks")
            .insert(service.into(), CheckedCredential { at: Instant::now(), result: result.clone() });
        result
    }

    pub async fn refresh_known(&self, runner: &dyn CommandRunner) {
        let services = self.checked.lock().expect("credential checks").keys().cloned().collect::<Vec<_>>();
        for service in services {
            let _ = self.require(runner, &service).await;
        }
    }

    pub fn failures(&self) -> BTreeMap<String, String> {
        self.checked
            .lock()
            .expect("credential checks")
            .iter()
            .filter_map(|(service, cached)| cached.result.as_ref().err().map(|error| (service.clone(), error.clone())))
            .collect()
    }
}
