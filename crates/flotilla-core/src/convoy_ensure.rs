//! Narrow ports between the composition root and standing-convoy controller.
use std::collections::BTreeMap;

use async_trait::async_trait;
use flotilla_protocol::{CanonicalHostId, PrincipalRef};
use flotilla_resources::{Convoy as ResourceConvoy, ConvoyEnsure, ReadResourceObject, ResourceObject};

use crate::in_process::PreparedConvoyAdmission;

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
    async fn abandon(&self, namespace: &str, name: &str, reason: &str, principal_ref: Option<&PrincipalRef>) -> Result<(), String>;
    async fn reap(&self, namespace: &str, name: &str, force: bool) -> Result<(), String>;
}
/// Verifies the provider backing of a terminal standing convoy before the
/// ensure controller may reclaim it. Implementations must fail closed: `Ok`
/// means the backing was positively observed dead, while any live, unknown,
/// or uninspectable state is an error that holds teardown.
#[async_trait]
pub trait StandingConvoyBackingInspector: Send + Sync {
    async fn verify_backing_dead(&self, convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String>;
}
