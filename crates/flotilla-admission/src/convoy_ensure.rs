//! Narrow ports between the composition root and standing-convoy controller.
use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::CanonicalHostId;
use flotilla_resources::lifecycle_reads::LifecycleReads;
use flotilla_resources::{Convoy as ResourceConvoy, ConvoyEnsure, ConvoyProvisioningState, ReadResourceObject, ResourceObject};

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
    /// The earliest time the last full pass deferred an ensure to (a strike's backoff or an
    /// admission refusal's retry): nothing else wakes the pass then.
    fn next_due(&self, namespace: &str) -> Option<chrono::DateTime<chrono::Utc>>;
    /// Role `role` of project `project`, as derived from its generations (#3108); `None` when
    /// no ensure declares it.
    async fn standing_role(&self, namespace: &str, project: &str, role: &str) -> Result<Option<crate::standing::StandingRole>, String>;
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

/// Verifies the provider backing of a terminal standing convoy before the
/// ensure controller may reclaim it. Implementations must fail closed: `Ok`
/// means the backing was positively observed dead, while any live, unknown,
/// or uninspectable state is an error that holds teardown.
#[async_trait]
pub trait StandingConvoyBackingInspector: Send + Sync {
    async fn verify_backing_dead(&self, convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String>;
}

/// Whether standing convoy `convoy` never began provisioning, `launched` saying whether any of its
/// works launched a vessel. The engine's view derives no provisioning state (#3108), so its
/// launches are the evidence; a status that says so is too.
pub fn never_provisioned(convoy: &ResourceObject<ResourceConvoy>, launched: bool) -> bool {
    !launched || convoy.status.as_ref().and_then(|status| status.provisioning) == Some(ConvoyProvisioningState::NotStarted)
}

/// Default fail-closed inspection of a standing convoy's backing Environment records.
pub struct ResourceStandingConvoyBackingInspector {
    pub resource_backend: flotilla_store::ResourceBackend,
    lifecycle_reads: Arc<dyn LifecycleReads>,
}

impl ResourceStandingConvoyBackingInspector {
    pub fn new(resource_backend: flotilla_store::ResourceBackend, lifecycle_reads: Arc<dyn LifecycleReads>) -> Self {
        Self { resource_backend, lifecycle_reads }
    }
}

#[async_trait]
impl StandingConvoyBackingInspector for ResourceStandingConvoyBackingInspector {
    async fn verify_backing_dead(&self, convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String> {
        // The convoy's environments are those held for its works' launches.
        let works = convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .map(|snapshot| snapshot.vessels.iter().map(|requirement| requirement.name.clone()).collect::<std::collections::BTreeSet<_>>())
            .unwrap_or_default();
        let convoy_name = convoy.metadata.name.clone();
        let reads = Arc::clone(&self.lifecycle_reads);
        let (launched, environments) = tokio::task::spawn_blocking(move || {
            let environments = reads.environments()?;
            let vessels = reads.vessels()?.of_convoy(&convoy_name)?;
            let mut launched = false;
            let mut held = Vec::new();
            for vessel in vessels.iter().filter(|vessel| works.contains(vessel.work())).map(|vessel| &vessel.name) {
                launched = true;
                held.extend(environments.held(vessel)?);
            }
            Ok::<_, String>((launched, held))
        })
        .await
        .map_err(|error| format!("could not inspect backing environments: {error}"))?
        .map_err(|error| format!("could not inspect backing environments: {error}"))?;
        if environments.is_empty() {
            if never_provisioned(convoy, launched) {
                return Ok(());
            }
            return Err("no backing environment evidence is available".to_string());
        }
        let not_dead = environments
            .into_iter()
            // An ended environment is dead: released, given up, or observed gone (Lost, or Failed
            // when its home said why). Exhaustion is not yet GiveUp.
            .filter(|environment| !environment.ended())
            .map(|environment| format!("Environment/{} is {}", environment.name, environment.reading.label()))
            .collect::<Vec<_>>();
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
    use flotilla_provisioning_testkit::{Ending, EngineFixture};
    use flotilla_resources::{
        ConvoySpec, ConvoyStatus, EnvironmentSpec, HostDirectEnvironmentSpec, InputMeta, VesselRequirement, WorkflowSnapshot,
    };
    use flotilla_store::ResourceBackend;

    // Teardown accepts any ended environment: a release, a give-up, or one observed gone.
    #[tokio::test]
    async fn backing_inspection_reads_the_engine_without_a_status_copy() {
        for ending in [Ending::Lost("/work"), Ending::Failed("container is not running"), Ending::Released, Ending::GaveUp("失敗")] {
            let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
            let convoys = backend.using::<ResourceConvoy>("flotilla");
            let created = convoys
                .create(&InputMeta::builder().name("convoy".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
                .await
                .expect("convoy");
            let snapshot = WorkflowSnapshot {
                cascade: None,
                exit: None,
                turn_delivery: Default::default(),
                stall_nudges: Default::default(),
                supervision: None,
                vessels: vec![VesselRequirement::builder().name("work".into()).crew(Vec::new()).build()],
            };
            let status = ConvoyStatus { workflow_snapshot: Some(snapshot), ..Default::default() };
            let convoy = convoys.update_status("convoy", &created.metadata.resource_version, &status).await.expect("snapshot");
            let engine = EngineFixture::default();
            let fixture = &engine.environments;
            let inspector = ResourceStandingConvoyBackingInspector::new(backend, engine.reads());
            assert!(inspector.verify_backing_dead(&convoy).await.is_ok(), "a convoy that launched no vessel never provisioned");
            let spec = EnvironmentSpec {
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/work".into() }),
                ..Default::default()
            };
            // The work's launch holds its environment.
            let launch = engine.vessels.request("convoy", "work", "host");
            assert!(inspector.verify_backing_dead(&convoy).await.is_err(), "a provisioning convoy with no environment has no evidence");
            fixture.request("env", &spec, Some(&launch));
            assert!(inspector.verify_backing_dead(&convoy).await.is_err());
            fixture.finish("env", ending);
            assert!(inspector.verify_backing_dead(&convoy).await.is_ok(), "an ended environment is verified dead");
            fixture.poison();
            assert!(inspector.verify_backing_dead(&convoy).await.expect_err("read error").contains("poisoned"));
        }
    }
}
