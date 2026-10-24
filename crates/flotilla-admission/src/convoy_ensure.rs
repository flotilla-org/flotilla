//! Narrow ports between the composition root and standing-convoy controller.
use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::{CanonicalHostId, LifecycleReading};
use flotilla_resources::lifecycle_reads::{LifecycleReads, NoLifecycleReads, ProvisioningKind};
use flotilla_resources::{
    Convoy as ResourceConvoy, ConvoyEnsure, ConvoyEnsureStatusPatch, ConvoyProvisioningState, Environment as ResourceEnvironment,
    EnvironmentPhase, ReadResourceObject, ResourceObject, CONVOY_LABEL,
};

use crate::convoy_admission::PreparedConvoyAdmission;

/// Controller entry points injected by the runtime; core only delegates.
#[async_trait]
pub trait ConvoyEnsureReconciler: Send + Sync {
    async fn active_ensured_convoys(
        &self,
        admission: &dyn ConvoyEnsureAdmission,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<ReadResourceObject<ResourceConvoy>>, String>;
    async fn reconcile_convoy_ensures_once_with_backing_inspector(
        &self,
        admission: &dyn ConvoyEnsureAdmission,
        namespace: &str,
        backing_inspector: &dyn StandingConvoyBackingInspector,
    ) -> Result<Vec<String>, String>;
    async fn reconcile_convoy_ensure_now(
        &self,
        admission: &dyn ConvoyEnsureAdmission,
        namespace: &str,
        name: &str,
        backing_inspector: &dyn StandingConvoyBackingInspector,
    ) -> Result<String, String>;
    async fn roll_convoy_ensure(&self, admission: &dyn ConvoyEnsureAdmission, namespace: &str, name: &str) -> Result<String, String>;
    async fn reap_ensured_convoy(
        &self,
        admission: &dyn ConvoyEnsureAdmission,
        namespace: &str,
        ensure_name: &str,
        convoy_name: &str,
        force: bool,
    ) -> Result<(), String>;
}

/// Admission and lifecycle operations used by the standing-convoy controller.
/// Preparation is read-only with respect to convoy creation; commit admits under
/// the admission transaction guard. The controller owns the ensure guard.
#[async_trait]
pub trait ConvoyEnsureAdmission: Send + Sync {
    fn local_host_id(&self) -> Option<CanonicalHostId>;
    async fn prepare(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
    ) -> Result<(ResourceObject<ConvoyEnsure>, PreparedConvoyAdmission), String>;
    async fn commit(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        admission: PreparedConvoyAdmission,
        annotations: BTreeMap<String, String>,
    ) -> Result<String, String>;
}

/// The `[primitives]` switch's Ensure path (#3629 layer 7). With it on, the
/// engine's Ensure decides each standing-convoy status write: the pass reports
/// the patch it would write, the engine applies what it means (record, create,
/// attempt, bind, unbind, hold, resume) and answers with the patch the old
/// status may take. The copy runs one way, engine to old. Absent, nothing
/// changes.
#[async_trait]
pub trait EngineEnsures: Send + Sync {
    /// The patch `ensure` may take for `patch`: `patch` itself when the engine
    /// took what it means, or `None` when the engine refused it and the old
    /// status stays as it is. An Ensure the engine does not hold (one written
    /// before the switch, until the import) keeps the old path: `patch` passes.
    async fn decide(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        patch: ConvoyEnsureStatusPatch,
    ) -> Result<Option<ConvoyEnsureStatusPatch>, String>;
}

/// Verifies the provider backing of a terminal standing convoy before the
/// ensure controller may reclaim it. Implementations must fail closed: `Ok`
/// means the backing was positively observed dead, while any live, unknown,
/// or uninspectable state is an error that holds teardown.
#[async_trait]
pub trait StandingConvoyBackingInspector: Send + Sync {
    async fn verify_backing_dead(&self, convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String>;
}

/// Default fail-closed inspection of a standing convoy's backing Environment records.
pub struct ResourceStandingConvoyBackingInspector {
    pub resource_backend: flotilla_store::ResourceBackend,
    lifecycle_reads: Arc<dyn LifecycleReads>,
}

impl ResourceStandingConvoyBackingInspector {
    pub fn new(resource_backend: flotilla_store::ResourceBackend) -> Self {
        Self { resource_backend, lifecycle_reads: Arc::new(NoLifecycleReads) }
    }

    pub fn with_lifecycle_reads(mut self, reads: Arc<dyn LifecycleReads>) -> Self {
        self.lifecycle_reads = reads;
        self
    }
}

#[async_trait]
impl StandingConvoyBackingInspector for ResourceStandingConvoyBackingInspector {
    async fn verify_backing_dead(&self, convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String> {
        let environments = self
            .resource_backend
            .using::<ResourceEnvironment>(&convoy.metadata.namespace)
            .list()
            .await
            .map_err(|error| format!("could not inspect backing environments: {error}"))?
            .items
            .into_iter()
            .filter(|environment| environment.metadata.labels.get(CONVOY_LABEL) == Some(&convoy.metadata.name))
            .collect::<Vec<_>>();
        if environments.is_empty() {
            if convoy.status.as_ref().and_then(|status| status.provisioning) == Some(ConvoyProvisioningState::NotStarted) {
                return Ok(());
            }
            return Err("no backing environment evidence is available".to_string());
        }
        let reads = Arc::clone(&self.lifecycle_reads);
        let not_dead = tokio::task::spawn_blocking(move || {
            let mut not_dead = Vec::new();
            for environment in environments {
                let phase = match reads.try_reading(ProvisioningKind::Environment, &environment.metadata.name, None)? {
                    Some(LifecycleReading::GaveUp { .. } | LifecycleReading::Released) => continue,
                    // Lost or snapshot-only Gone is not positive teardown evidence; exhaustion is not yet GiveUp.
                    Some(reading) => reading.label().to_string(),
                    None => {
                        let phase = environment.status.as_ref().map(|status| status.phase).unwrap_or_default();
                        if phase == EnvironmentPhase::Failed {
                            continue;
                        }
                        format!("{phase:?}")
                    }
                };
                not_dead.push(format!("Environment/{} is {phase}", environment.metadata.name));
            }
            Ok::<_, String>(not_dead)
        })
        .await
        .map_err(|error| format!("could not inspect backing lifecycles: {error}"))??;
        if not_dead.is_empty() {
            Ok(())
        } else {
            Err(format!("backing is not verified dead: {}", not_dead.join(", ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_provisioning::engine::{
        environment::EnvironmentBinding,
        reads::{LifecycleReadPort, StoreReads},
        Binding, LifecycleHome, Lifecycles, Step,
    };
    use flotilla_resources::{ConvoySpec, EnvironmentSpec, EnvironmentStatus, HostDirectEnvironmentSpec, InputMeta};
    use flotilla_schema::Timestamp;
    use flotilla_state_machines::{Cause, CauseCategory, Outcome};
    use std::sync::Mutex;

    // Teardown requires an engine-confirmed give-up or release, even beside a stale old phase.
    // Every old phase, both Gone endings and empty/Unicode give-up causes are enumerated.
    #[tokio::test]
    async fn backing_inspection_reads_the_engine_without_a_status_copy() {
        for old_phase in [
            EnvironmentPhase::Pending,
            EnvironmentPhase::Provisioning,
            EnvironmentPhase::Ready,
            EnvironmentPhase::Failed,
            EnvironmentPhase::Lost,
        ] {
            for ending in ["ready", "released", "lost", "", "失敗"] {
                let backend = flotilla_store::ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
                let convoy = backend
                    .using::<ResourceConvoy>("flotilla")
                    .create(
                        &InputMeta::builder().name("convoy".into()).build(),
                        &ConvoySpec::builder().workflow_ref("workflow".into()).build(),
                    )
                    .await
                    .expect("convoy");
                let envs = backend.using::<ResourceEnvironment>("flotilla");
                let spec = EnvironmentSpec {
                    host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/work".into() }),
                    ..Default::default()
                };
                let mut environment = envs
                    .create(&InputMeta::builder().name("env".into()).labels([(CONVOY_LABEL.into(), "convoy".into())].into()).build(), &spec)
                    .await
                    .expect("environment");
                let old_status = EnvironmentStatus { phase: old_phase, message: Some("stale".into()), ..Default::default() };
                environment = envs.update_status("env", &environment.metadata.resource_version, &old_status).await.expect("old status");
                envs.create(
                    &InputMeta::builder().name("unrelated".into()).labels([(CONVOY_LABEL.into(), "other".into())].into()).build(),
                    &spec,
                )
                .await
                .expect("unrelated environment");
                let legacy = ResourceStandingConvoyBackingInspector::new(backend.clone());
                assert_eq!(legacy.verify_backing_dead(&convoy).await.is_ok(), old_phase == EnvironmentPhase::Failed);
                let mut registry = flotilla_schema::KindRegistry::new();
                flotilla_provisioning::stored::register(&mut registry).expect("registry");
                let store = Arc::new(Mutex::new(
                    flotilla_schema_sqlite::Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", registry)
                        .expect("store"),
                ));
                let engine: LifecycleHome<flotilla_provisioning::stored::environment::EnvironmentKind, _> = LifecycleHome::new(
                    "home",
                    flotilla_state_machines::Home::with_log("home", flotilla_engine_store::StoreLog::new(Arc::clone(&store)))
                        .expect("home"),
                );
                let reads = Arc::new(LifecycleReadPort::new(StoreReads::new(Arc::clone(&store)), "home".into()));
                let inspector = ResourceStandingConvoyBackingInspector::new(backend).with_lifecycle_reads(reads);
                assert_eq!(inspector.verify_backing_dead(&convoy).await.is_ok(), old_phase == EnvironmentPhase::Failed);
                engine
                    .request("env", EnvironmentBinding::request(&environment, "home").expect("request"), Timestamp(1))
                    .expect("requested");
                assert!(inspector.verify_backing_dead(&convoy).await.expect_err("progress holds teardown").contains("InProgress"));
                engine
                    .step(
                        "env",
                        &Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::Transient, "retry"), retryable: true }),
                        Timestamp(2),
                    )
                    .expect("retry");
                assert!(inspector.verify_backing_dead(&convoy).await.expect_err("retry holds teardown").contains("Retrying"));
                if matches!(ending, "ready" | "released" | "lost") {
                    engine.step("env", &Step::Realised { identity: "/work".into() }, Timestamp(3)).expect("ready");
                    assert!(inspector.verify_backing_dead(&convoy).await.expect_err("ready holds teardown").contains("Ready"));
                    if ending != "ready" {
                        engine.step("env", &if ending == "lost" { Step::Vanished } else { Step::Released }, Timestamp(4)).expect("ended");
                    }
                } else {
                    engine
                        .step(
                            "env",
                            &Step::Attempted(Outcome::Failed { cause: Cause::new(CauseCategory::External, ending), retryable: false }),
                            Timestamp(3),
                        )
                        .expect("gave up");
                }
                for _ in 0..2 {
                    assert_eq!(inspector.verify_backing_dead(&convoy).await.is_ok(), !matches!(ending, "ready" | "lost"));
                }
                assert_eq!(envs.get("env").await.expect("old environment").status, Some(old_status));
                flotilla_store_testkit::poison_mutex(store);
                assert!(inspector.verify_backing_dead(&convoy).await.expect_err("read error").contains("poisoned"));
            }
        }
    }
}
