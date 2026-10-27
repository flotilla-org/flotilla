//! Core controller ports and environment-routing Clone/Checkout adapters.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::{Arc, Mutex as StdMutex},
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_core::in_process::OperatorReconciler;
use flotilla_crew::reconcilers::vessel::WorktreeMetadataResolver;
use flotilla_protocol::RepoSelector;
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provisioning::reconcilers::checkout::runtime::{removal_source_path, CheckoutControllerRuntime};
use flotilla_provisioning::reconcilers::clone::runtime::CloneControllerRuntime;
use flotilla_provisioning::reconcilers::{
    CheckoutRemoval, CheckoutRemovalOutcome, CheckoutRuntime, CloneRuntime, ForgeDefaultBranchResolver, PreparedCheckout,
};
use flotilla_resources::checkout_path::checkout_path_from_status_and_spec;
use flotilla_resources::{
    ChangeRequest, Checkout, CheckoutIntegrationStatus, CheckoutSpec, Clone, ClonePhase, Convoy, ConvoyTeardownRuntime, Demand, DemandKind,
    DemandSpec, Forge, ForgeIdentity, InputMeta, Resource, ResourceError, ResourceObject,
};
use flotilla_store::{ReplicaReadResolver, ResourceBackend};
use flotilla_vcs::vcs::CheckoutMaterialisationError;
use flotilla_vcs::vcs::CheckoutRegistration;
use flotilla_vcs::vcs::WorktreeMetadata;
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
    pub(super) manifests: Option<flotilla_config::ResourceManifestsConfig>,
    pub(super) local_root: String,
}

#[async_trait]
impl OperatorReconciler for RuntimeOperatorReconciler {
    async fn reconcile_now(&self, namespace: &str, kind: &str, name: &str) -> Result<String, String> {
        let normalized = kind.to_ascii_lowercase().replace(['_', '-'], "");
        match normalized.as_str() {
            "convoyensure" | "convoyensures" => self.state.daemon.reconcile_convoy_ensure_now(namespace, name, &*self.state).await,
            "manifestroot" | "manifestroots" => {
                let roots = self.state.daemon.manifest_roots();
                let root = roots.root(namespace, name).await?.ok_or_else(|| format!("ManifestRoot {name} not found"))?;
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
                            else { self.state.daemon.local_vcs_for_checkout(&manifests.dir).await? })
                        .with_roots(roots);
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
                let ready = match self.state.daemon.lifecycle_reads().try_reading(
                    flotilla_resources::lifecycle_reads::ProvisioningKind::Clone, name, None,
                )? {
                    Some(reading) => matches!(reading, flotilla_protocol::LifecycleReading::Ready { .. }),
                    None => clone.status.as_ref().map(|status| status.phase) == Some(ClonePhase::Ready),
                };
                if ready {
                    return Ok(format!("Clone/{name} is already ready"));
                }
                flotilla_store::apply_status_patch(&clones, name, &flotilla_resources::CloneStatusPatch::MarkCloning)
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

/// Process-local six-hour successes, keyed by service/repository. Per-key locks
/// coalesce concurrent prepares without blocking unrelated repositories.
/// [Workload note](https://github.com/flotilla-org/flotilla/blob/main/docs/implementation-choices/forge-budget-bleed.md).
pub(super) struct GhForgeDefaultBranchResolver {
    runner: Arc<dyn CommandRunner>,
    cache: StdMutex<DefaultBranchCache>,
}
#[derive(Default)]
struct DefaultBranchCache {
    last_prune: Option<tokio::time::Instant>,
    entries: HashMap<(String, String), Arc<tokio::sync::Mutex<BranchLookup>>>,
}
#[derive(Default)]
struct BranchLookup {
    success: Option<(tokio::time::Instant, String)>,
    retry_at: Option<chrono::DateTime<Utc>>,
}
impl GhForgeDefaultBranchResolver {
    pub(super) fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner, cache: Default::default() }
    }
}
#[async_trait]
impl ForgeDefaultBranchResolver for GhForgeDefaultBranchResolver {
    #[tracing::instrument(name = "repository_default_branch", skip_all)]
    async fn default_branch(&self, forge: &ForgeIdentity) -> Result<Option<String>, String> {
        if forge.service_url.trim_end_matches('/') != "https://github.com" {
            return Ok(None);
        }
        let key = (forge.service_url.trim_end_matches('/').to_string(), forge.repository.clone());
        let lookup = {
            let mut cache = self.cache.lock().expect("default branch cache lock");
            let now = tokio::time::Instant::now();
            // A concurrent Arc drop can only retain an idle entry for one extra
            // pruning pass. Removing the map's Arc never invalidates a caller's Arc.
            if cache.last_prune.is_none_or(|at| now.duration_since(at) >= std::time::Duration::from_secs(60)) {
                cache.entries.retain(|_, entry| {
                    Arc::strong_count(entry) > 1
                        || entry.try_lock().map_or(true, |lookup| {
                            lookup.success.as_ref().is_some_and(|(expires, _)| *expires > now)
                                || lookup.retry_at.is_some_and(|at| at > Utc::now())
                        })
                });
                cache.last_prune = Some(now);
            }
            Arc::clone(cache.entries.entry(key).or_default())
        };
        let mut lookup = lookup.lock().await;
        if let Some((_, branch)) = lookup.success.as_ref().filter(|(at, _)| *at > tokio::time::Instant::now()) {
            return Ok(Some(branch.clone()));
        }
        lookup.success = None;
        if let Some(retry_at) = lookup.retry_at.filter(|at| *at > Utc::now()) {
            return Err(format!("github rate limited (budget=REST core, identity=host gh login, retry_at={})", retry_at.to_rfc3339()));
        }
        lookup.retry_at = None;
        let endpoint = format!("repos/{}", forge.repository);
        let output =
            match self.runner.run("gh", &["api", &endpoint, "--jq", ".default_branch"], Path::new("/"), &ChannelLabel::Default).await {
                Ok(output) => output,
                Err(error) => {
                    lookup.retry_at = error
                        .strip_prefix("github rate limited (")
                        .and_then(|fields| fields.strip_suffix(')'))
                        .and_then(|fields| fields.rsplit_once("retry_at=").or_else(|| fields.rsplit_once("reset_at=")))
                        .and_then(|(_, deadline)| deadline.parse().ok());
                    return Err(error);
                }
            };
        let branch = output.trim();
        if branch.is_empty() {
            return Ok(None);
        }
        // Owner-ruled TTL: branch changes can lag six hours; a repository rename
        // with a new service/repository key is a cache miss immediately.
        lookup.success = Some((tokio::time::Instant::now() + std::time::Duration::from_secs(6 * 60 * 60), branch.to_string()));
        Ok(Some(branch.to_string()))
    }
}

/// The first resolver that knows the forge answers. A failing resolver does
/// not hide a later one's answer; its error is reported only when none answers.
pub(super) struct ChainedForgeDefaultBranchResolver(pub(super) Vec<Arc<dyn ForgeDefaultBranchResolver>>);

#[async_trait]
impl ForgeDefaultBranchResolver for ChainedForgeDefaultBranchResolver {
    async fn default_branch(&self, forge: &ForgeIdentity) -> Result<Option<String>, String> {
        let mut first_error = None;
        for resolver in &self.0 {
            match resolver.default_branch(forge).await {
                Ok(Some(branch)) => return Ok(Some(branch)),
                Ok(None) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(None), Err)
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
            .ok_or_else(|| format!("checkout environment {env_ref} is unavailable on this host"))?;
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
    async fn validate_new_branch(
        &self,
        checkout: &ResourceObject<Checkout>,
    ) -> Result<Option<String>, flotilla_provider_runtime::ObservationError> {
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
            CheckoutSpec::FreshClone(spec) if spec.source_clone_ref.is_none() => {
                vcs.continue_fresh_clone(&spec.url, &spec.r#ref, target).await?
            }
            CheckoutSpec::FreshClone(_) => {
                return Err("local clones continue through host-local clone preparation with a managed source".to_string().into())
            }
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

    async fn create_local_clone_in(
        &self,
        env_ref: &str,
        request: flotilla_vcs::vcs::LocalCloneRequest<'_>,
    ) -> Result<(PreparedCheckout, Option<String>), String> {
        self.runtime_for(env_ref, request.source).await?.create_local_clone_in(env_ref, request).await
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
        runtime.inspect_integration(checkout, convoy).await
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

#[cfg(test)]
mod chained_default_branch_tests {
    use super::*;

    // Process boundary fake: count only the default-branch gh command.
    struct DefaultBranchRunner {
        calls: std::sync::atomic::AtomicUsize,
        failures: std::sync::atomic::AtomicUsize,
        error: Option<String>,
    }
    #[async_trait]
    impl CommandRunner for DefaultBranchRunner {
        async fn run_output(
            &self,
            cmd: &str,
            args: &[&str],
            cwd: &Path,
            label: &ChannelLabel,
        ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
            self.run(cmd, args, cwd, label).await.map(|stdout| flotilla_provider_runtime::CommandOutput {
                stdout,
                stderr: String::new(),
                exit_code: Some(0),
            })
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            true
        }
        async fn run(&self, cmd: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
            assert_eq!(cmd, "gh");
            assert_eq!(args, ["api", "repos/org/repo", "--jq", ".default_branch"]);
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self
                .failures
                .try_update(std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst, |left| left.checked_sub(1))
                .is_ok()
            {
                Err(self.error.clone().unwrap_or_else(|| "temporary forge failure".into()))
            } else {
                Ok("main\n".into())
            }
        }
    }
    async fn default_branch_fixture(
        failures: usize,
    ) -> (Arc<DefaultBranchRunner>, flotilla_provisioning::reconcilers::RepositoryReconciler, ResourceObject<flotilla_resources::Repository>)
    {
        let runner = Arc::new(DefaultBranchRunner { calls: Default::default(), failures: failures.into(), error: None });
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let spec = flotilla_resources::RepositorySpec::remote("https://github.com/org/repo.git").expect("spec");
        let repository = backend
            .using::<flotilla_resources::Repository>("ops")
            .create(&InputMeta::builder().name(spec.key().to_string()).build(), &spec)
            .await
            .expect("repository");
        let reconciler = flotilla_provisioning::reconcilers::RepositoryReconciler::new(backend.clone(), backend, "ops")
            .with_forge_default_branch_resolver(Arc::new(GhForgeDefaultBranchResolver::new(runner.clone())));
        (runner, reconciler, repository)
    }
    // #3710: reconciliation resync and repeated prepares reuse six-hour successes.
    #[tokio::test(start_paused = true)]
    async fn forge_default_branch_cached_across_prepares() {
        use flotilla_store::controller::Reconciler;
        let (runner, reconciler, repository) = default_branch_fixture(0).await;
        for _ in 0..10 {
            assert_eq!(reconciler.prepare(&repository).await.expect("prepare").default_branch.as_deref(), Some("main"));
            tokio::time::advance(std::time::Duration::from_secs(60)).await;
        }
        assert_eq!(runner.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    // #3710: an ordinary failure is retried on the very next prepare.
    #[tokio::test]
    async fn failed_default_branch_lookup_not_cached() {
        use flotilla_store::controller::Reconciler;
        let (runner, reconciler, repository) = default_branch_fixture(1).await;
        assert!(reconciler.prepare(&repository).await.expect("failed lookup").default_branch.is_none());
        assert_eq!(reconciler.prepare(&repository).await.expect("retry").default_branch.as_deref(), Some("main"));
        assert_eq!(runner.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
    // #3710: a success is valid strictly before six hours, then refreshed.
    #[tokio::test(start_paused = true)]
    async fn default_branch_refreshes_after_ttl() {
        use flotilla_store::controller::Reconciler;
        let (runner, reconciler, repository) = default_branch_fixture(0).await;
        reconciler.prepare(&repository).await.expect("first");
        tokio::time::advance(std::time::Duration::from_secs(6 * 60 * 60 - 1)).await;
        reconciler.prepare(&repository).await.expect("cached");
        assert_eq!(runner.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        reconciler.prepare(&repository).await.expect("refreshed");
        assert_eq!(runner.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    // Pin both public producers: the host budget runner's reset_at refusal and
    // the HTTP classifier's retry_at error must suppress the next prepare.
    #[tokio::test]
    async fn default_branch_honours_cooldowns_from_github_error_producers() {
        use flotilla_forge_github::{
            forge_budget::{BudgetedRunner, ForgeBudgets},
            github::{rate_limit_error_from_response, rate_limit_reset},
        };
        use std::sync::atomic::Ordering;
        let raw = "HTTP/2 429 Too Many Requests\r\nRetry-After: 600\r\n\r\n{\"message\":\"secondary rate limit\"}";
        let producer = BudgetedRunner {
            inner: Arc::new(DefaultBranchRunner { calls: 0.into(), failures: 1.into(), error: Some(raw.into()) }),
            budgets: ForgeBudgets::default(),
        };
        let args = ["api", "repos/org/repo", "--jq", ".default_branch"];
        producer.run("gh", &args, Path::new("/"), &ChannelLabel::Default).await.expect_err("learn cooldown");
        let reset_error = producer.run("gh", &args, Path::new("/"), &ChannelLabel::Default).await.expect_err("budget producer");
        let retry_error = rate_limit_error_from_response(raw, "REST core").expect("HTTP producer");
        assert!(reset_error.contains("reset_at="));
        assert!(retry_error.contains("retry_at="));
        for error in [reset_error, retry_error] {
            let expected_retry = rate_limit_reset(&error).expect("producer retry date");
            let runner = Arc::new(DefaultBranchRunner { calls: 0.into(), failures: 2.into(), error: Some(error) });
            let resolver = GhForgeDefaultBranchResolver::new(runner.clone());
            let forge = ForgeIdentity { service_url: "https://github.com".into(), repository: "org/repo".into() };
            resolver.default_branch(&forge).await.expect_err("first limit");
            let cooldown = resolver.default_branch(&forge).await.expect_err("cooldown");
            assert_eq!(rate_limit_reset(&cooldown), Some(expected_retry));
            assert_eq!(runner.calls.load(Ordering::SeqCst), 1, "cooldown must avoid another command");
        }
    }

    struct Answer(Result<Option<&'static str>, &'static str>);

    #[async_trait]
    impl ForgeDefaultBranchResolver for Answer {
        async fn default_branch(&self, _forge: &ForgeIdentity) -> Result<Option<String>, String> {
            self.0.map(|branch| branch.map(str::to_string)).map_err(str::to_string)
        }
    }

    async fn resolve(answers: Vec<Result<Option<&'static str>, &'static str>>) -> Result<Option<String>, String> {
        let chain = ChainedForgeDefaultBranchResolver(
            answers.into_iter().map(|answer| Arc::new(Answer(answer)) as Arc<dyn ForgeDefaultBranchResolver>).collect(),
        );
        chain.default_branch(&ForgeIdentity { service_url: "https://forge.example".into(), repository: "team/app".into() }).await
    }

    // Behaviour: the first resolver with an answer wins; an earlier failure
    // neither hides a later answer nor is lost when nobody answers.
    #[tokio::test]
    async fn the_first_answer_wins_and_errors_surface_only_without_one() {
        assert_eq!(resolve(vec![Ok(None), Ok(Some("main")), Ok(Some("trunk"))]).await, Ok(Some("main".into())));
        assert_eq!(resolve(vec![Err("unreachable"), Ok(Some("main"))]).await, Ok(Some("main".into())));
        assert_eq!(resolve(vec![Ok(None), Err("first"), Err("second")]).await, Err("first".into()));
        assert_eq!(resolve(vec![Ok(None), Ok(None)]).await, Ok(None));
        assert_eq!(resolve(Vec::new()).await, Ok(None));
    }
}

#[derive(bon::Builder)]
pub(super) struct ProviderImageIo {
    pub provider: Arc<dyn flotilla_environment::environment::EnvironmentProvider>,
}

#[async_trait]
impl flotilla_provisioning::image_inventory::ImageInventoryIo for ProviderImageIo {
    async fn inventory(&self) -> Result<BTreeSet<String>, String> {
        self.provider.local_image_cache().ok_or("provider has no local image cache")?.inventory().await
    }
}
