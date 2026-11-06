//! Narrow ports between the composition root and standing-convoy controller.
use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::{CanonicalHostId, LifecycleReading};
use flotilla_resources::lifecycle_reads::{LifecycleReads, NoLifecycleReads};
use flotilla_resources::{
    Convoy as ResourceConvoy, ConvoyEnsure, ConvoyEnsureStatusPatch, ConvoyProvisioningState, ReadResourceObject, ResourceObject,
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
        // The convoy's environments are those held for its vessels.
        let vessels = convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .map(|snapshot| {
                snapshot
                    .vessels
                    .iter()
                    .map(|requirement| flotilla_resources::vessel_resource_name(&convoy.metadata.name, &requirement.name))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let reads = Arc::clone(&self.lifecycle_reads);
        let environments = tokio::task::spawn_blocking(move || {
            let environments = reads.environments()?;
            let mut held = Vec::new();
            for vessel in &vessels {
                held.extend(environments.held(vessel)?);
            }
            Ok::<_, String>(held)
        })
        .await
        .map_err(|error| format!("could not inspect backing environments: {error}"))?
        .map_err(|error| format!("could not inspect backing environments: {error}"))?;
        if environments.is_empty() {
            if convoy.status.as_ref().and_then(|status| status.provisioning) == Some(ConvoyProvisioningState::NotStarted) {
                return Ok(());
            }
            return Err("no backing environment evidence is available".to_string());
        }
        let not_dead = environments
            .into_iter()
            // Lost or a failed backing is not positive teardown evidence; exhaustion is not yet GiveUp.
            .filter(|environment| !matches!(environment.reading, LifecycleReading::GaveUp { .. } | LifecycleReading::Released))
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
    use flotilla_provisioning_testkit::{Ending, EnvironmentFixture};
    use flotilla_resources::{
        ConvoySpec, ConvoyStatus, EnvironmentSpec, HostDirectEnvironmentSpec, InputMeta, VesselRequirement, WorkflowSnapshot,
    };
    use flotilla_store::ResourceBackend;

    // Teardown accepts an engine release or give-up, but Lost is not positive teardown evidence.
    #[tokio::test]
    async fn backing_inspection_reads_the_engine_without_a_status_copy() {
        for ending in [Ending::Lost("/work"), Ending::Released, Ending::GaveUp("失敗")] {
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
            let fixture = EnvironmentFixture::default();
            let inspector = ResourceStandingConvoyBackingInspector::new(backend).with_lifecycle_reads(fixture.reads.clone());
            assert!(inspector.verify_backing_dead(&convoy).await.is_err(), "a provisioning convoy with no environment has no evidence");
            let spec = EnvironmentSpec {
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/work".into() }),
                ..Default::default()
            };
            fixture.request("env", &spec, Some(&flotilla_resources::vessel_resource_name("convoy", "work")));
            assert!(inspector.verify_backing_dead(&convoy).await.is_err());
            fixture.finish("env", ending);
            assert_eq!(inspector.verify_backing_dead(&convoy).await.is_ok(), !matches!(ending, Ending::Lost(_)));
            fixture.poison();
            assert!(inspector.verify_backing_dead(&convoy).await.expect_err("read error").contains("poisoned"));
        }
    }
}
