//! Core controller ports and environment-routing Clone/Checkout adapters.

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, Mutex as StdMutex},
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::checkout::runtime::{removal_source_path, CheckoutControllerRuntime};
use flotilla_controllers::reconcilers::clone::runtime::CloneControllerRuntime;
use flotilla_controllers::reconcilers::{
    vessel::WorktreeMetadataResolver, CheckoutRemoval, CheckoutRemovalOutcome, CheckoutRuntime, CloneRuntime, ForgeDefaultBranchResolver,
    PreparedCheckout,
};
use flotilla_core::{
    checkout_integration::checkout_path_from_status_and_spec,
    in_process::{InProcessDaemon, OperatorReconciler},
    providers::{ChannelLabel, CommandRunner},
    vcs::{CheckoutMaterialisationError, CheckoutRegistration, WorktreeMetadata},
};
use flotilla_protocol::RepoSelector;
use flotilla_resources::{
    ChangeRequest, Checkout, CheckoutIntegrationStatus, CheckoutSpec, Clone, ClonePhase, Convoy, ConvoyTeardownRuntime, Demand, DemandKind,
    DemandSpec, Forge, ForgeIdentity, InputMeta, ManifestRoot, ReplicaReadResolver, Resource, ResourceBackend, ResourceError,
    ResourceObject,
};
use tracing::{info, warn};

use super::credentials::{reconcile_work_credentials_for_environment, record_credential_delivery_retry};
use super::state::ControllerRuntimeState;
use crate::resource_manifest::ResourceManifestReconciler;

pub(super) struct DaemonConvoyTeardownRuntime {
    pub(super) daemon: Arc<InProcessDaemon>,
    pub(super) reclaim_refusals: StdMutex<HashMap<String, ReclaimRefusal>>,
}

pub(super) struct ReclaimRefusal {
    pub(super) error: String,
    pub(super) attempts: u64,
}

pub(super) struct RuntimeOperatorReconciler {
    pub(super) state: Arc<ControllerRuntimeState>,
    pub(super) manifests: Option<flotilla_core::config::ResourceManifestsConfig>,
    pub(super) local_root: String,
}

#[async_trait]
impl OperatorReconciler for RuntimeOperatorReconciler {
    async fn reconcile_now(&self, namespace: &str, kind: &str, name: &str) -> Result<String, String> {
        let normalized = kind.to_ascii_lowercase().replace(['_', '-'], "");
        match normalized.as_str() {
            "convoyensure" | "convoyensures" => self.state.daemon.reconcile_convoy_ensure_now(namespace, name, &*self.state).await,
            "manifestroot" | "manifestroots" => {
                let root = self.state.daemon.resource_backend().using::<ManifestRoot>(namespace).get(name).await
                    .map_err(|error| error.to_string())?;
                if root.spec.host != self.local_root {
                    return Err(format!("manifest root `{name}` is owned by another host; route reconcile-now to that host"));
                }
                let manifests = self.manifests.as_ref().ok_or_else(|| "manifest reconciliation is not configured".to_string())?;
                if root.spec.path != manifests.dir.to_string_lossy() || root.spec.source != manifests.source || root.spec.binding != manifests.binding {
                    return Err(format!("manifest root `{name}` does not match the declared source"));
                }
                let mut reconciler =
                    ResourceManifestReconciler::new(self.state.daemon.resource_backend(), namespace, manifests.dir.clone())
                        .with_declared_source(manifests.source.clone(), manifests.reconciler_root.clone())
                        .with_binding(manifests.binding.clone())
                        .with_vcs(if manifests.binding.is_some() { self.state.daemon.local_charter_vcs()? }
                            else { self.state.daemon.local_vcs_for_checkout(&manifests.dir).await? });
                let report = reconciler.reconcile_once().await?;
                Ok(format!(
                    "manifest root {name}: {} created, {} updated, {} unchanged, {} errors",
                    report.created,
                    report.updated,
                    report.unchanged,
                    report.errors.len()
                ))
            }
            "repository" | "repositories" | "repo" => {
                self.state.daemon.refresh_strict(&RepoSelector::Query(name.to_string())).await?;
                Ok(format!("Repository/{name} refreshed"))
            }
            "clone" | "clones" => {
                let clones = self.state.daemon.resource_backend().using::<Clone>(namespace);
                let clone = clones.get(name).await.map_err(|error| error.to_string())?;
                if clone.status.as_ref().map(|status| status.phase) == Some(ClonePhase::Ready) {
                    return Ok(format!("Clone/{name} is already ready"));
                }
                flotilla_resources::apply_status_patch(&clones, name, &flotilla_resources::CloneStatusPatch::MarkCloning)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(format!("Clone/{name} retry requested"))
            }
            "credentialdelivery" | "workcredential" | "workcredentials" => {
                record_credential_delivery_retry(&self.state.daemon.resource_backend(), namespace, name, None, false).await?;
                reconcile_work_credentials_for_environment(&self.state, namespace, name).await?;
                Ok(format!("CredentialDelivery/{name} reconciled"))
            }
            "convoy" | "convoys" => wake_controller_resource::<Convoy>(&self.state.daemon.resource_backend(), namespace, name).await,
            _ => Err(format!(
                "resource kind `{kind}` does not support reconcile-now; expected Clone, Convoy, ConvoyEnsure, CredentialDelivery, ManifestRoot, or Repository"
            )),
        }
    }
}

pub(super) async fn wake_controller_resource<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
) -> Result<String, String> {
    let resources = backend.clone().using::<T>(namespace);
    let object = resources.get(name).await.map_err(|error| error.to_string())?;
    let mut meta = InputMeta::from(&object.metadata);
    meta.annotations.insert(RECONCILE_NOW_ANNOTATION.to_string(), Utc::now().to_rfc3339());
    resources.update(&meta, &object.metadata.resource_version, &object.spec).await.map_err(|error| error.to_string())?;
    Ok(format!("{}/{} woken", T::API_PATHS.kind, name))
}

impl DaemonConvoyTeardownRuntime {
    pub(super) fn new(daemon: Arc<InProcessDaemon>) -> Self {
        Self { daemon, reclaim_refusals: StdMutex::new(HashMap::new()) }
    }

    pub(super) fn attention_name(convoy: &ResourceObject<Convoy>) -> String {
        format!("reclaim-refusal-{}", convoy.metadata.name)
    }

    pub(super) async fn raise_attention(&self, convoy: &ResourceObject<Convoy>, reason: &str) -> Result<(), String> {
        let demands = self.daemon.resource_backend().using::<Demand>(&convoy.metadata.namespace);
        let name = Self::attention_name(convoy);
        let target = flotilla_protocol::ResourceRef::new(
            flotilla_resources::api_version(Convoy::API_PATHS),
            Convoy::API_PATHS.kind,
            &convoy.metadata.namespace,
            &convoy.metadata.name,
        );
        let meta = InputMeta::builder()
            .name(name)
            .annotations(BTreeMap::from([(RECLAIM_REFUSAL_REASON_ANNOTATION.to_string(), reason.to_string())]))
            .build();
        let spec = DemandSpec::for_dispatching_principal(target, DemandKind::HumanGate, convoy.spec.dispatching_principal_ref.clone());
        match demands.create(&meta, &spec).await {
            Ok(_) => Ok(()),
            Err(ResourceError::Conflict { .. }) => {
                let current = demands.get(&meta.name).await.map_err(|error| error.to_string())?;
                demands.update(&meta, &current.metadata.resource_version, &spec).await.map(|_| ()).map_err(|error| error.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    pub(super) async fn clear_attention(&self, convoy: &ResourceObject<Convoy>) -> Result<(), String> {
        let demands = self.daemon.resource_backend().using::<Demand>(&convoy.metadata.namespace);
        match demands.delete(&Self::attention_name(convoy)).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}

#[async_trait]
impl ConvoyTeardownRuntime for DaemonConvoyTeardownRuntime {
    async fn verify_reclaim(&self, convoy: &ResourceObject<Convoy>, checkouts: &[ResourceObject<Checkout>]) -> Result<(), String> {
        let result = self.daemon.verify_convoy_teardown_gate_for_checkouts(convoy, checkouts, false).await;
        let key = format!("{}/{}", convoy.metadata.namespace, convoy.metadata.name);
        match &result {
            Err(error) => {
                let attempts = {
                    let mut refusals = self.reclaim_refusals.lock().expect("reclaim refusal lock poisoned");
                    match refusals.get_mut(&key) {
                        Some(refusal) => {
                            refusal.error.clone_from(error);
                            refusal.attempts += 1;
                            refusal.attempts
                        }
                        _ => {
                            refusals.insert(key, ReclaimRefusal { error: error.clone(), attempts: 1 });
                            1
                        }
                    }
                };
                if attempts == 1 {
                    warn!(
                        namespace = %convoy.metadata.namespace,
                        convoy = %convoy.metadata.name,
                        attempts,
                        %error,
                        "automatic convoy reclaim refused"
                    );
                }
                if attempts >= RECLAIM_REFUSAL_ATTENTION_AFTER {
                    if let Err(attention_error) = self.raise_attention(convoy, error).await {
                        warn!(
                            namespace = %convoy.metadata.namespace,
                            convoy = %convoy.metadata.name,
                            %attention_error,
                            "could not raise persistent reclaim refusal attention"
                        );
                    }
                }
            }
            Ok(()) => {
                if let Some(refusal) = self.reclaim_refusals.lock().expect("reclaim refusal lock poisoned").remove(&key) {
                    if refusal.attempts > 1 {
                        info!(
                            namespace = %convoy.metadata.namespace,
                            convoy = %convoy.metadata.name,
                            refused_attempts = refusal.attempts,
                            "automatic convoy reclaim recovered after repeated refusal"
                        );
                    }
                }
                if let Err(attention_error) = self.clear_attention(convoy).await {
                    warn!(
                        namespace = %convoy.metadata.namespace,
                        convoy = %convoy.metadata.name,
                        %attention_error,
                        "could not clear recovered reclaim refusal attention"
                    );
                }
            }
        }
        result
    }
}

pub(super) struct GhForgeDefaultBranchResolver {
    pub(super) runner: Arc<dyn CommandRunner>,
}

#[async_trait]
impl ForgeDefaultBranchResolver for GhForgeDefaultBranchResolver {
    async fn default_branch(&self, forge: &ForgeIdentity) -> Result<Option<String>, String> {
        if forge.service_url.trim_end_matches('/') != "https://github.com" {
            return Ok(None);
        }
        let endpoint = format!("repos/{}", forge.repository);
        let output = self.runner.run("gh", &["api", &endpoint, "--jq", ".default_branch"], Path::new("/"), &ChannelLabel::Default).await?;
        let branch = output.trim();
        Ok((!branch.is_empty()).then(|| branch.to_string()))
    }
}

pub(super) struct RoutingCloneRuntime {
    pub(super) state: Arc<ControllerRuntimeState>,
}

impl RoutingCloneRuntime {
    pub(super) async fn runtime_for(&self, env_ref: &str, checkout: &str) -> Result<CloneControllerRuntime, String> {
        let environment = self
            .state
            .daemon
            .resolve_environment_ref(env_ref)
            .ok_or_else(|| format!("command runner unavailable for clone environment {env_ref}"))?;
        let vcs = self.state.daemon.vcs_for_checkout(&environment.id, Path::new(checkout)).await?;
        let runner = environment.runner;
        let namespace = self.state.daemon.provisioning_namespace().await;
        let forges = self
            .state
            .daemon
            .resource_backend()
            .including_replicas::<Forge>(&namespace)
            .list()
            .await
            .map_err(|error| format!("list Forge definitions: {error}"))?
            .items
            .into_iter()
            .map(|forge| forge.object.spec)
            .collect();
        Ok(CloneControllerRuntime::new(runner, Some(vcs), Arc::clone(&self.state.clone_flights), forges))
    }
}

#[async_trait]
impl CloneRuntime for RoutingCloneRuntime {
    async fn clone_and_inspect(&self, repo_url: &str, target_path: &str) -> Result<Option<String>, String> {
        self.runtime_for(&self.state.host_direct_environment_name, target_path).await?.clone_and_inspect(repo_url, target_path).await
    }

    async fn inspect_existing(&self, target_path: &str) -> Result<Option<String>, String> {
        self.runtime_for(&self.state.host_direct_environment_name, target_path).await?.inspect_existing(target_path).await
    }

    async fn clone_and_inspect_in(&self, env_ref: &str, repo_url: &str, target_path: &str) -> Result<Option<String>, String> {
        self.runtime_for(env_ref, target_path).await?.clone_and_inspect(repo_url, target_path).await
    }

    async fn inspect_existing_in(&self, env_ref: &str, target_path: &str) -> Result<Option<String>, String> {
        self.runtime_for(env_ref, target_path).await?.inspect_existing(target_path).await
    }
}

pub(super) struct RoutingCheckoutRuntime {
    pub(super) state: Arc<ControllerRuntimeState>,
    pub(super) change_requests: Option<ReplicaReadResolver<ChangeRequest>>,
}

#[async_trait]
impl WorktreeMetadataResolver for RoutingCheckoutRuntime {
    async fn worktree_metadata(&self, env_ref: &str, target: &str) -> Result<WorktreeMetadata, String> {
        let runtime = self.runtime_for(env_ref, target).await?;
        runtime.vcs(target)?.worktree_metadata(Path::new(target)).await
    }
}

impl RoutingCheckoutRuntime {
    pub(super) async fn runtime_for(&self, env_ref: &str, checkout: &str) -> Result<CheckoutControllerRuntime, String> {
        let environment = self
            .state
            .daemon
            .resolve_environment_ref(env_ref)
            .ok_or_else(|| format!("command runner unavailable for checkout environment {env_ref}"))?;
        let vcs = self.state.daemon.vcs_for_checkout(&environment.id, Path::new(checkout)).await?;
        let runner = environment.runner;
        let namespace = self.state.daemon.provisioning_namespace().await;
        let forges = self
            .state
            .daemon
            .resource_backend()
            .definitions::<Forge>(&namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect();
        Ok(CheckoutControllerRuntime::new(runner, Some(vcs), self.change_requests.clone(), forges))
    }
}

#[async_trait]
impl CheckoutRuntime for RoutingCheckoutRuntime {
    async fn validate_new_branch(&self, checkout: &ResourceObject<Checkout>) -> Result<Option<String>, String> {
        let Some(target) = checkout.spec.target_path() else { return Ok(None) };
        let env_ref = checkout.spec.env_ref().ok_or_else(|| "checkout environment unavailable".to_string())?;
        let environment = self.state.daemon.resolve_environment_ref(env_ref).ok_or("checkout environment unavailable")?;
        if environment.runner.path_exists(Path::new(target)).await? {
            return Ok(None);
        }
        self.state.daemon.validate_new_checkout_branch(checkout).await
    }

    async fn continue_checkout_in(
        &self,
        checkout: &ResourceObject<Checkout>,
        clone_path: Option<&str>,
        reason: &str,
    ) -> Result<PreparedCheckout, CheckoutMaterialisationError> {
        let env_ref = checkout.spec.env_ref().ok_or_else(|| "checkout environment unavailable".to_string())?;
        let target = checkout.spec.target_path().ok_or_else(|| "checkout target unavailable".to_string())?;
        let path = clone_path.unwrap_or(target);
        let runtime = self.runtime_for(env_ref, path).await?;
        let vcs = runtime.vcs(path)?;
        let result = match &checkout.spec {
            CheckoutSpec::Worktree(spec) => vcs.continue_checkout(&spec.r#ref, target, reason).await?,
            CheckoutSpec::FreshClone(spec) => vcs.continue_fresh_clone(&spec.url, &spec.r#ref, target).await?,
            CheckoutSpec::Observed(_) => return Err("observed checkout cannot be continued".to_string().into()),
        };
        Ok(PreparedCheckout { commit: result.commit, branch_provenance: result.provenance })
    }

    async fn protect_worktree_in(&self, env_ref: &str, clone_path: &str, target: &str, reason: &str) -> Result<(), String> {
        let runtime = self.runtime_for(env_ref, clone_path).await?;
        let vcs = runtime.vcs(clone_path)?;
        vcs.checkout_registration(target, CheckoutRegistration::Protect { reason }).await
    }

    async fn checkout_path_exists_in(&self, env_ref: &str, path: &str) -> Result<Option<bool>, String> {
        if env_ref != self.state.host_direct_environment_name {
            return Ok(None);
        }
        // `test -e` conflates ENOENT with permission failures. Only the host's
        // filesystem error can make absence authoritative for teardown.
        match tokio::fs::symlink_metadata(path).await {
            Ok(_) => Ok(Some(true)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Some(false)),
            Err(error) => Err(format!("inspect worktree path {path}: {error}")),
        }
    }

    async fn create_worktree(
        &self,
        clone_path: &str,
        branch: &str,
        base_ref: Option<&str>,
        target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        self.runtime_for(&self.state.host_direct_environment_name, clone_path)
            .await?
            .create_worktree(clone_path, branch, base_ref, target_path)
            .await
    }

    async fn create_fresh_clone(
        &self,
        repo_url: &str,
        branch: &str,
        base_ref: Option<&str>,
        target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        self.runtime_for(&self.state.host_direct_environment_name, target_path)
            .await?
            .create_fresh_clone(repo_url, branch, base_ref, target_path)
            .await
    }

    async fn inspect_integration(
        &self,
        checkout: &ResourceObject<Checkout>,
        convoy: Option<&ResourceObject<Convoy>>,
    ) -> Result<CheckoutIntegrationStatus, String> {
        self.inspect_integration_in(&self.state.host_direct_environment_name, checkout, convoy).await
    }

    async fn remove_checkout(&self, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        self.remove_checkout_in(&self.state.host_direct_environment_name, removal).await
    }

    async fn create_worktree_in(
        &self,
        env_ref: &str,
        clone_path: &str,
        branch: &str,
        base_ref: Option<&str>,
        target_path: &str,
        registration_reason: &str,
    ) -> Result<PreparedCheckout, CheckoutMaterialisationError> {
        let runtime = self.runtime_for(env_ref, clone_path).await?;
        let vcs = runtime.vcs(clone_path)?;
        let materialisation = vcs.materialise_checkout(branch, base_ref, target_path, registration_reason).await?;
        Ok(PreparedCheckout { commit: materialisation.commit, branch_provenance: materialisation.provenance })
    }

    async fn create_fresh_clone_in(
        &self,
        env_ref: &str,
        repo_url: &str,
        branch: &str,
        base_ref: Option<&str>,
        target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        self.runtime_for(env_ref, target_path).await?.create_fresh_clone(repo_url, branch, base_ref, target_path).await
    }

    async fn inspect_integration_in(
        &self,
        env_ref: &str,
        checkout: &ResourceObject<Checkout>,
        convoy: Option<&ResourceObject<Convoy>>,
    ) -> Result<CheckoutIntegrationStatus, String> {
        let path = checkout_path_from_status_and_spec(checkout.status.as_ref(), &checkout.spec).ok_or("checkout path unavailable")?;
        let runtime = self.runtime_for(env_ref, path).await?;
        if convoy.is_none() {
            return runtime.inspect_integration(checkout, convoy).await;
        }
        let vcs = runtime.vcs(path)?;
        let request = self.state.daemon.resolve_live_checkout_change_request(checkout, vcs.as_ref(), Path::new(path)).await?;
        runtime.inspect_integration_with_request(checkout, convoy, request.as_deref()).await
    }

    async fn remove_checkout_in(&self, env_ref: &str, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        // Shared by every namespace and execution environment on this daemon.
        // Hold capacity through archive creation and every deletion in the
        // removal, releasing it on success, failure, or cancellation.
        let _permit = self.state.checkout_removals.acquire().await.map_err(|error| error.to_string())?;
        if let Err(error) = self.state.register_checkout_archive_roots(env_ref, removal).await {
            warn!(%error, "could not record checkout archive root for retention sweep");
        }
        self.runtime_for(env_ref, removal_source_path(removal)).await?.remove_checkout(removal).await
    }
}

pub(super) const RECLAIM_REFUSAL_ATTENTION_AFTER: u64 = 3;

pub(super) const RECLAIM_REFUSAL_REASON_ANNOTATION: &str = "flotilla.work/reclaim-refusal-reason";

pub(super) const RECONCILE_NOW_ANNOTATION: &str = "flotilla.work/reconcile-now-at";
