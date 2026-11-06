//! Host-local recovery for backing containers that lost their resource records.
//!
//! An environment is homed at the host whose provider realises it, so only this home's engine
//! records can prove absence: a backing whose environment is not live here (never requested,
//! released, lost or given up) is reclaimed after a grace period. The Environment controller
//! tears down every live one.
//! The Docker endpoint must have one authoritative Flotilla daemon: legacy
//! environment labels do not identify a config directory or daemon owner.
//! See docs/development.md for deployment isolation and recovery diagnostics.
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use flotilla_environment::environment::{EnvironmentBacking, EnvironmentProvider};
use flotilla_resources::environment_records::EnvironmentRecords;
use tokio::time::Instant;

use crate::reconcilers::EnvironmentRuntime;

pub const ORPHAN_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const ORPHAN_GRACE: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum StoreReadiness {
    #[default]
    Restoring,
    Ready,
}

#[derive(Default)]
pub struct EnvironmentOrphanSweep {
    readiness: StoreReadiness,
    absent_since: BTreeMap<EnvironmentBacking, Instant>,
    pending_cleanup: BTreeSet<EnvironmentBacking>,
}

/// The environments this home holds live: their backings are the controller's.
fn protected_environments(environments: &dyn EnvironmentRecords) -> Result<BTreeSet<String>, String> {
    Ok(environments.live()?.into_iter().map(|record| record.name).collect())
}

impl EnvironmentOrphanSweep {
    pub fn mark_ready(&mut self) {
        self.readiness = StoreReadiness::Ready;
    }

    pub async fn sweep(
        &mut self,
        environments: &dyn EnvironmentRecords,
        provider: &dyn EnvironmentProvider,
        runtime: &dyn EnvironmentRuntime,
        now: Instant,
    ) -> Result<(), String> {
        if self.readiness == StoreReadiness::Restoring {
            self.absent_since.clear();
            return Ok(());
        }
        self.sweep_ready(environments, provider, runtime, now).await
    }

    async fn sweep_ready(
        &mut self,
        environments: &dyn EnvironmentRecords,
        provider: &dyn EnvironmentProvider,
        runtime: &dyn EnvironmentRuntime,
        now: Instant,
    ) -> Result<(), String> {
        let observations = async {
            let protected = protected_environments(environments)?;
            let backings = provider.list_backings().await?.into_iter().collect::<BTreeSet<_>>();
            Ok::<_, String>((protected, backings))
        }
        .await;
        let (protected, backings) = match observations {
            Ok(observations) => observations,
            Err(error) => {
                // Failed observations break continuous proof of absence.
                self.absent_since.clear();
                return Err(error);
            }
        };
        let mut errors = Vec::new();
        self.absent_since.retain(|backing, _| backings.contains(backing) && !protected.contains(backing.environment_id.as_str()));
        let mut retried = BTreeSet::new();
        for backing in self.pending_cleanup.clone() {
            let environment = backing.environment_id.as_str();
            let latest = match protected_environments(environments) {
                Ok(latest) => latest,
                Err(error) => {
                    self.absent_since.clear();
                    return Err(error);
                }
            };
            // A replacement gets its own grace. Never use an old cleanup retry
            // to destroy a new container or remove its environment-scoped files.
            if latest.contains(environment)
                || backings.iter().any(|other| other.environment_id == backing.environment_id && other.backing_id != backing.backing_id)
            {
                self.pending_cleanup.remove(&backing);
                self.absent_since.remove(&backing);
                continue;
            }
            retried.insert(backing.clone());
            let result = if backings.contains(&backing) {
                runtime.destroy(environment, &backing.backing_id).await
            } else {
                runtime.cleanup(environment).await
            };
            match result {
                Ok(()) => {
                    self.pending_cleanup.remove(&backing);
                    self.absent_since.remove(&backing);
                    tracing::info!(%environment, container = %backing.backing_id, "finished orphan environment teardown");
                }
                Err(error) => errors.push(format!("orphan {environment} cleanup: {error}")),
            }
        }
        for backing in backings {
            let environment = backing.environment_id.as_str();
            if protected.contains(environment) || retried.contains(&backing) {
                continue;
            }
            let since = self.absent_since.entry(backing.clone()).or_insert(now);
            if now.duration_since(*since) < ORPHAN_GRACE {
                continue;
            }
            // Re-read authority immediately before destruction: an Environment
            // may have appeared while Docker listing or previous cleanup awaited.
            let latest = match protected_environments(environments) {
                Ok(latest) => latest,
                Err(error) => {
                    self.absent_since.clear();
                    return Err(error);
                }
            };
            if latest.contains(environment) {
                self.absent_since.remove(&backing);
                continue;
            }
            tracing::warn!(%environment, container = %backing.backing_id, grace_seconds = ORPHAN_GRACE.as_secs(),
                "reclaiming orphan environment backing");
            self.pending_cleanup.insert(backing.clone());
            if let Err(error) = runtime.destroy(environment, &backing.backing_id).await {
                errors.push(format!("orphan {environment} teardown: {error}"));
                continue;
            }
            self.pending_cleanup.remove(&backing);
            self.absent_since.remove(&backing);
            tracing::info!(%environment, container = %backing.backing_id, "reclaimed orphan environment backing and local state");
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use flotilla_environment::environment::EnvironmentProvisionError;
    use flotilla_environment::environment::{EnvironmentHandle, ProvisionOpts};
    use flotilla_protocol::{EnvironmentId, LifecycleReading};
    use flotilla_resources::{environment_records::EnvironmentRecord, EnvironmentSpec};
    use hegel::generators as gs;

    use super::*;

    /// This home's live environments, by name: the sweep's whole authority.
    #[derive(Default)]
    struct Authority(Mutex<BTreeSet<String>>);

    impl Authority {
        fn create(&self, name: &str) {
            self.0.lock().expect("authority").insert(name.to_owned());
        }

        fn end(&self, name: &str) {
            self.0.lock().expect("authority").remove(name);
        }
    }

    impl EnvironmentRecords for Authority {
        fn get(&self, _: &str, _: Option<&str>) -> Result<Option<EnvironmentRecord>, String> {
            unreachable!("the sweep reads live environments only")
        }

        fn find(&self, _: &str) -> Result<Option<EnvironmentRecord>, String> {
            unreachable!("the sweep reads live environments only")
        }

        fn live(&self) -> Result<Vec<EnvironmentRecord>, String> {
            let record = |name: &String| EnvironmentRecord {
                name: name.clone(),
                home: "home".into(),
                spec: EnvironmentSpec::default(),
                vessel: None,
                reading: LifecycleReading::InProgress,
                launch: None,
            };
            Ok(self.0.lock().expect("authority").iter().map(record).collect())
        }

        fn held(&self, _: &str) -> Result<Vec<EnvironmentRecord>, String> {
            unreachable!("the sweep reads live environments only")
        }

        fn request(&self, _: &str, _: &EnvironmentSpec, _: Option<&str>) -> Result<bool, String> {
            unreachable!("the sweep requests nothing")
        }
    }

    // This fake stands in for the Docker subprocess boundary.
    #[derive(Default, bon::Builder)]
    struct DockerBoundary {
        backings: Mutex<Vec<EnvironmentBacking>>,
        destroyed: Mutex<Vec<String>>,
        fail_listing: Mutex<bool>,
        create_on_list: Mutex<Option<Arc<Authority>>>,
        fail_cleanup: Mutex<bool>,
        cleaned: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl EnvironmentProvider for DockerBoundary {
        flotilla_discovery_testkit::injected_environment_provider!();
        async fn prepare(
            &self,
            _spec: &flotilla_resources::EnvironmentSpec,
            _opts: &flotilla_environment::environment::PrepareOpts,
        ) -> Result<flotilla_environment::environment::PreparedEnvironment, String> {
            Ok(flotilla_environment::environment::PreparedEnvironment::new(&Arc::new(()), ()))
        }

        async fn provision(
            &self,
            _: EnvironmentId,
            _: &flotilla_environment::environment::PreparedEnvironment,
            _: ProvisionOpts,
        ) -> Result<EnvironmentHandle, EnvironmentProvisionError> {
            unreachable!()
        }
        async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
            unreachable!("recovery must not parse mount metadata")
        }
        async fn list_backings(&self) -> Result<Vec<EnvironmentBacking>, String> {
            if *self.fail_listing.lock().expect("listing") {
                return Err("Docker unavailable".into());
            }
            let create = self.create_on_list.lock().expect("create on list").take();
            if let Some(authority) = create {
                authority.create("env");
            }
            Ok(self.backings.lock().expect("backings").clone())
        }
        async fn destroy(&self, _: &str) -> Result<(), String> {
            unreachable!()
        }
    }
    #[async_trait]
    impl EnvironmentRuntime for DockerBoundary {
        async fn provision_environment(
            &self,
            _: &str,
            _spec: &flotilla_resources::EnvironmentSpec,
        ) -> Result<crate::reconcilers::EnvironmentProvisioning, EnvironmentProvisionError> {
            unreachable!()
        }
        async fn destroy(&self, environment: &str, id: &str) -> Result<(), String> {
            self.destroyed.lock().expect("destroyed").push(id.to_string());
            self.backings.lock().expect("backings").retain(|backing| backing.backing_id != id);
            self.cleanup(environment).await
        }
        async fn cleanup(&self, environment: &str) -> Result<(), String> {
            if *self.fail_cleanup.lock().expect("cleanup") {
                return Err("state cleanup unavailable".into());
            }
            self.cleaned.lock().expect("cleaned").push(environment.into());
            Ok(())
        }
    }
    fn backing(id: &str) -> EnvironmentBacking {
        EnvironmentBacking { environment_id: EnvironmentId::new("env"), backing_id: id.into() }
    }
    // #2840: a backing with no live Environment is reaped after ten
    // minutes of successful absence observations, and only once.
    #[tokio::test]
    async fn orphan_sweep_reaps_after_grace_and_is_idempotent() {
        let backend = Authority::default();
        let docker = DockerBoundary::default();
        docker.backings.lock().expect("backings").extend([backing("id"), backing("id")]);
        let mut sweep = EnvironmentOrphanSweep::default();
        sweep.mark_ready();
        let now = Instant::now();
        for seconds in [0, 599, 600, 601] {
            sweep.sweep(&backend, &docker, &docker, now + Duration::from_secs(seconds)).await.expect("sweep");
            assert_eq!(*docker.destroyed.lock().expect("destroyed"), if seconds < 600 { vec![] } else { vec!["id"] });
        }
    }

    // #2840: startup does nothing, and a live record at this home protects
    // its backing.
    #[tokio::test]
    async fn orphan_sweep_preserves_startup_and_authority() {
        let home = Authority::default();
        let docker = DockerBoundary::default();
        docker.backings.lock().expect("backings").push(backing("id"));
        let mut sweep = EnvironmentOrphanSweep::default();
        let now = Instant::now();
        for seconds in [0, 600, 1200] {
            sweep.sweep(&home, &docker, &docker, now + Duration::from_secs(seconds)).await.expect("startup");
            assert!(docker.destroyed.lock().expect("destroyed").is_empty());
        }
        sweep.mark_ready();
        home.create("env");
        for seconds in [1201, 1801] {
            sweep.sweep(&home, &docker, &docker, now + Duration::from_secs(seconds)).await.expect("authority");
            assert!(docker.destroyed.lock().expect("destroyed").is_empty());
        }
    }

    // An Environment arriving during Docker listing must be checked again at
    // the destructive boundary, even after the full grace period elapsed.
    #[tokio::test]
    async fn orphan_sweep_rechecks_authority_after_docker_listing() {
        let backend = Arc::new(Authority::default());
        let docker = DockerBoundary::default();
        docker.backings.lock().expect("backings").push(backing("id"));
        let mut sweep = EnvironmentOrphanSweep::default();
        sweep.mark_ready();
        let now = Instant::now();
        sweep.sweep(backend.as_ref(), &docker, &docker, now).await.expect("first observation");
        *docker.create_on_list.lock().expect("create on list") = Some(Arc::clone(&backend));
        sweep.sweep(backend.as_ref(), &docker, &docker, now + ORPHAN_GRACE).await.expect("racing observation");
        assert!(docker.destroyed.lock().expect("destroyed").is_empty());
    }

    // Teardown may remove Docker backing and then fail on local state. Retry
    // that state even though the next Docker inventory is empty. A replacement
    // container must instead get its own complete grace period.
    #[tokio::test]
    async fn orphan_sweep_retries_cleanup_without_reaping_replacements() {
        for replace in [false, true] {
            let backend = Authority::default();
            let docker = DockerBoundary::default();
            docker.backings.lock().expect("backings").push(backing("old"));
            let mut sweep = EnvironmentOrphanSweep::default();
            sweep.mark_ready();
            let now = Instant::now();
            sweep.sweep(&backend, &docker, &docker, now).await.expect("first observation");
            *docker.fail_cleanup.lock().expect("cleanup") = true;
            assert!(sweep.sweep(&backend, &docker, &docker, now + ORPHAN_GRACE).await.is_err());
            assert_eq!(*docker.destroyed.lock().expect("destroyed"), vec!["old"]);
            *docker.fail_cleanup.lock().expect("cleanup") = false;
            if replace {
                docker.backings.lock().expect("backings").push(backing("new"));
            }
            sweep.sweep(&backend, &docker, &docker, now + ORPHAN_GRACE + Duration::from_secs(1)).await.expect("retry");
            assert_eq!(*docker.destroyed.lock().expect("destroyed"), vec!["old"], "retry must not reap replacement");
            assert_eq!(docker.cleaned.lock().expect("cleaned").len(), usize::from(!replace));
            if replace {
                sweep.sweep(&backend, &docker, &docker, now + ORPHAN_GRACE * 2 + Duration::from_secs(1)).await.expect("new grace");
                assert_eq!(*docker.destroyed.lock().expect("destroyed"), vec!["old", "new"]);
            }
        }
    }

    // Generate create/delete, restore, Docker failures, container replacement,
    // and time advances crossing the grace boundary. No unsafe observation may
    // count toward uninterrupted absence; invariants are checked after each step.
    #[hegel::test]
    fn orphan_sweep_requires_uninterrupted_authoritative_absence(tc: hegel::TestCase) {
        let operations: Vec<_> = (0..tc.draw(gs::integers::<usize>().min_value(1).max_value(20)))
            .map(|_| tc.draw(gs::integers::<u8>().min_value(0).max_value(6)))
            .collect();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let backend = Authority::default();
            let docker = DockerBoundary::default();
            docker.backings.lock().expect("backings").push(backing("id"));
            let mut sweep = EnvironmentOrphanSweep::default();
            let mut now = Instant::now();
            let mut exists = false;
            let mut generation = 0;
            let mut absent_since = None;
            for operation in operations {
                let mut readiness = StoreReadiness::Ready;
                match operation {
                    0 if !exists => {
                        backend.create("env");
                        exists = true;
                    }
                    1 if exists => {
                        backend.end("env");
                        exists = false;
                    }
                    2 => readiness = StoreReadiness::Restoring,
                    3 => *docker.fail_listing.lock().expect("listing") = true,
                    4 => {
                        docker.backings.lock().expect("backings").clear();
                        generation += 1;
                        docker.backings.lock().expect("backings").push(backing(&format!("id-{generation}")));
                        absent_since = None;
                    }
                    5 => now += Duration::from_secs(599),
                    6 => now += Duration::from_secs(600),
                    _ => {}
                }
                let failed = *docker.fail_listing.lock().expect("listing");
                let has_backing = !docker.backings.lock().expect("backings").is_empty();
                let before = docker.destroyed.lock().expect("destroyed").len();
                let expected = if exists || failed || readiness == StoreReadiness::Restoring || !has_backing {
                    absent_since = None;
                    false
                } else {
                    let since = *absent_since.get_or_insert(now);
                    now.duration_since(since) >= ORPHAN_GRACE
                };
                sweep.readiness = readiness;
                let result = sweep.sweep(&backend, &docker, &docker, now).await;
                assert_eq!(result.is_err(), failed && readiness == StoreReadiness::Ready);
                assert_eq!(docker.destroyed.lock().expect("destroyed").len() - before, usize::from(expected));
                if expected {
                    absent_since = None;
                }
                *docker.fail_listing.lock().expect("listing") = false;
            }
        });
    }
}
