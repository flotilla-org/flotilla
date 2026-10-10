//! Host-local recovery for backing containers that lost their resource records.
//!
//! Environments do not replicate (ADR 0016): only a successful read of this
//! host's embedded authoritative store can prove absence. Never substitute a
//! fleet/replica view or a remote HTTP backend here.
//! The Docker endpoint must have one authoritative Flotilla daemon: legacy
//! environment labels do not identify a config directory or daemon owner.
//! See docs/development.md for deployment isolation and recovery diagnostics.
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use flotilla_controllers::reconcilers::DockerEnvironmentRuntime;
use flotilla_core::providers::environment::{EnvironmentBacking, EnvironmentProvider};
use flotilla_resources::{Environment, Resource};
use flotilla_store::ResourceBackend;
use tokio::time::Instant;

pub(crate) const ORPHAN_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const ORPHAN_GRACE: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum StoreReadiness {
    #[default]
    Restoring,
    Ready,
}

#[derive(Default)]
pub(crate) struct EnvironmentOrphanSweep {
    readiness: StoreReadiness,
    absent_since: BTreeMap<EnvironmentBacking, Instant>,
    pending_cleanup: BTreeSet<EnvironmentBacking>,
}

async fn protected_environments(backend: &ResourceBackend) -> Result<BTreeSet<String>, String> {
    // HTTP is a view of someone else's authority, even if its list is empty.
    backend.local_root().map_err(|error| error.to_string())?;
    let mut protected = BTreeSet::new();
    for namespace in backend.local_namespaces::<Environment>().await.map_err(|error| error.to_string())? {
        let list = backend.using::<Environment>(&namespace).list().await.map_err(|error| error.to_string())?;
        // Protect every phase, including soft deletion and foreign-host specs:
        // the normal finalizer owns these, never the absence sweep.
        protected.extend(list.items.into_iter().map(|object| object.metadata.name));
    }
    if let Some(diagnostics) = backend.diagnostics().await.map_err(|error| error.to_string())? {
        // A record quarantined during a roll is not proof of absence.
        protected.extend(
            diagnostics
                .decode_quarantines
                .into_iter()
                .filter(|record| record.kind == Environment::API_PATHS.kind)
                .map(|record| record.name),
        );
    }
    Ok(protected)
}

impl EnvironmentOrphanSweep {
    pub(crate) fn mark_ready(&mut self) {
        self.readiness = StoreReadiness::Ready;
    }

    pub(crate) async fn sweep(
        &mut self,
        backend: &ResourceBackend,
        provider: &dyn EnvironmentProvider,
        runtime: &dyn DockerEnvironmentRuntime,
        now: Instant,
    ) -> Result<(), String> {
        if self.readiness == StoreReadiness::Restoring {
            self.absent_since.clear();
            return Ok(());
        }
        self.sweep_ready(backend, provider, runtime, now).await
    }

    async fn sweep_ready(
        &mut self,
        backend: &ResourceBackend,
        provider: &dyn EnvironmentProvider,
        runtime: &dyn DockerEnvironmentRuntime,
        now: Instant,
    ) -> Result<(), String> {
        let observations = async {
            let protected = protected_environments(backend).await?;
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
            let latest = match protected_environments(backend).await {
                Ok(latest) => latest,
                Err(error) => {
                    self.absent_since.clear();
                    return Err(error);
                }
            };
            // A replacement gets its own grace. Never use an old cleanup retry
            // to destroy a new container or remove its environment-scoped files.
            if latest.contains(environment)
                || backings.iter().any(|other| other.environment_id == backing.environment_id && other.container_id != backing.container_id)
            {
                self.pending_cleanup.remove(&backing);
                self.absent_since.remove(&backing);
                continue;
            }
            retried.insert(backing.clone());
            let result = if backings.contains(&backing) {
                runtime.destroy(environment, &backing.container_id).await
            } else {
                runtime.cleanup(environment).await
            };
            match result {
                Ok(()) => {
                    self.pending_cleanup.remove(&backing);
                    self.absent_since.remove(&backing);
                    tracing::info!(%environment, container = %backing.container_id, "finished orphan environment teardown");
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
            let latest = match protected_environments(backend).await {
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
            tracing::warn!(%environment, container = %backing.container_id, grace_seconds = ORPHAN_GRACE.as_secs(),
                "reclaiming orphan environment backing");
            self.pending_cleanup.insert(backing.clone());
            if let Err(error) = runtime.destroy(environment, &backing.container_id).await {
                errors.push(format!("orphan {environment} teardown: {error}"));
                continue;
            }
            self.pending_cleanup.remove(&backing);
            self.absent_since.remove(&backing);
            tracing::info!(%environment, container = %backing.container_id, "reclaimed orphan environment backing and local state");
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
    use flotilla_core::providers::environment::{EnvironmentHandle, ProvisionOpts};
    use flotilla_protocol::EnvironmentId;
    use flotilla_resources::{EnvironmentSpec, HostDirectEnvironmentSpec, InputMeta};
    use hegel::generators as gs;

    use super::*;

    // This fake stands in for the Docker subprocess boundary. Store operations
    // below use the real in-memory authoritative backend.
    #[derive(Default, bon::Builder)]
    struct DockerBoundary {
        backings: Mutex<Vec<EnvironmentBacking>>,
        destroyed: Mutex<Vec<String>>,
        fail_listing: Mutex<bool>,
        create_on_list: Mutex<Option<ResourceBackend>>,
        fail_cleanup: Mutex<bool>,
        cleaned: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl EnvironmentProvider for DockerBoundary {
        fn kind(&self) -> flotilla_core::providers::environment::EnvironmentKind {
            flotilla_core::providers::environment::EnvironmentKind::Docker
        }
        async fn prepare(
            &self,
            _spec: &flotilla_resources::EnvironmentSpec,
            _opts: &flotilla_core::providers::environment::PrepareOpts,
        ) -> Result<flotilla_core::providers::environment::PreparedEnvironment, String> {
            Ok(flotilla_core::providers::environment::PreparedEnvironment::new(&Arc::new(()), ()))
        }

        async fn provision(
            &self,
            _: EnvironmentId,
            _: &flotilla_core::providers::environment::PreparedEnvironment,
            _: ProvisionOpts,
        ) -> Result<EnvironmentHandle, String> {
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
            if let Some(backend) = create {
                create_environment(&backend).await;
            }
            Ok(self.backings.lock().expect("backings").clone())
        }
        async fn destroy(&self, _: &str) -> Result<(), String> {
            unreachable!()
        }
    }
    #[async_trait]
    impl DockerEnvironmentRuntime for DockerBoundary {
        async fn provision(
            &self,
            _: &str,
            _: &flotilla_resources::DockerEnvironmentSpec,
        ) -> Result<flotilla_controllers::reconcilers::DockerProvisioning, String> {
            unreachable!()
        }
        async fn destroy(&self, environment: &str, id: &str) -> Result<(), String> {
            self.destroyed.lock().expect("destroyed").push(id.to_string());
            self.backings.lock().expect("backings").retain(|backing| backing.container_id != id);
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
        EnvironmentBacking { environment_id: EnvironmentId::new("env"), container_id: id.into() }
    }
    async fn create_environment(backend: &ResourceBackend) {
        backend
            .using::<Environment>("another-namespace")
            .create(
                &InputMeta::builder().name("env".into()).build(),
                &EnvironmentSpec {
                    host_direct: Some(HostDirectEnvironmentSpec { host_ref: "home".into(), repo_default_dir: "/".into() }),
                    docker: None,
                },
            )
            .await
            .expect("authoritative environment");
    }

    // #2840: a backing with no authoritative Environment is reaped after ten
    // minutes of successful absence observations, and only once.
    #[tokio::test]
    async fn orphan_sweep_reaps_after_grace_and_is_idempotent() {
        let backend = ResourceBackend::InMemory(Default::default());
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

    // #2840: startup does nothing, and a live record in home authority protects
    // a backing even while another host has no replicated record at all.
    #[tokio::test]
    async fn orphan_sweep_preserves_startup_and_authority_despite_replication_lag() {
        let home = ResourceBackend::InMemory(Default::default());
        let lagging_peer = ResourceBackend::InMemory(Default::default());
        let docker = DockerBoundary::default();
        docker.backings.lock().expect("backings").push(backing("id"));
        let mut sweep = EnvironmentOrphanSweep::default();
        let now = Instant::now();
        for seconds in [0, 600, 1200] {
            sweep.sweep(&home, &docker, &docker, now + Duration::from_secs(seconds)).await.expect("startup");
            assert!(docker.destroyed.lock().expect("destroyed").is_empty());
        }
        sweep.mark_ready();
        create_environment(&home).await;
        assert!(lagging_peer.using::<Environment>("another-namespace").list().await.expect("lagging view").items.is_empty());
        for seconds in [1201, 1801] {
            sweep.sweep(&home, &docker, &docker, now + Duration::from_secs(seconds)).await.expect("authority");
            assert!(docker.destroyed.lock().expect("destroyed").is_empty());
        }
    }

    // An Environment arriving during Docker listing must be checked again at
    // the destructive boundary, even after the full grace period elapsed.
    #[tokio::test]
    async fn orphan_sweep_rechecks_authority_after_docker_listing() {
        let backend = ResourceBackend::InMemory(Default::default());
        let docker = DockerBoundary::default();
        docker.backings.lock().expect("backings").push(backing("id"));
        let mut sweep = EnvironmentOrphanSweep::default();
        sweep.mark_ready();
        let now = Instant::now();
        sweep.sweep(&backend, &docker, &docker, now).await.expect("first observation");
        *docker.create_on_list.lock().expect("create on list") = Some(backend.clone());
        sweep.sweep(&backend, &docker, &docker, now + ORPHAN_GRACE).await.expect("racing observation");
        assert!(docker.destroyed.lock().expect("destroyed").is_empty());
    }

    // A decode quarantine is evidence of ownership, even when the Environment
    // cannot be listed as a typed record after a roll. Exercise real SQLite.
    #[tokio::test]
    async fn orphan_sweep_preserves_quarantined_environment() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("resources.sqlite");
        let backend = ResourceBackend::Sqlite(flotilla_store::SqliteBackend::open(&path).expect("store"));
        let connection = rusqlite::Connection::open(&path).expect("raw store");
        connection
            .execute(
                "INSERT INTO resource_decode_quarantine (group_name, version, kind, namespace, name, body_json, error, quarantined_at) \
             VALUES (?1, ?2, ?3, 'flotilla', 'env', '{}', 'old generation', ?4)",
                rusqlite::params![
                    Environment::API_PATHS.group,
                    Environment::API_PATHS.version,
                    Environment::API_PATHS.kind,
                    chrono::Utc::now().to_rfc3339()
                ],
            )
            .expect("quarantine");
        let docker = DockerBoundary::default();
        docker.backings.lock().expect("backings").push(backing("id"));
        let mut sweep = EnvironmentOrphanSweep::default();
        sweep.mark_ready();
        let now = Instant::now();
        for elapsed in [Duration::ZERO, ORPHAN_GRACE] {
            sweep.sweep(&backend, &docker, &docker, now + elapsed).await.expect("quarantine observation");
            assert!(docker.destroyed.lock().expect("destroyed").is_empty());
        }
    }

    // Teardown may remove Docker backing and then fail on local state. Retry
    // that state even though the next Docker inventory is empty. A replacement
    // container must instead get its own complete grace period.
    #[tokio::test]
    async fn orphan_sweep_retries_cleanup_without_reaping_replacements() {
        for replace in [false, true] {
            let backend = ResourceBackend::InMemory(Default::default());
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
            let backend = ResourceBackend::InMemory(Default::default());
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
                        create_environment(&backend).await;
                        exists = true;
                    }
                    1 if exists => {
                        backend.using::<Environment>("another-namespace").delete("env").await.expect("delete");
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
