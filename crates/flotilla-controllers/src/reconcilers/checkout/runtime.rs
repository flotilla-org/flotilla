//! Checkout materialisation, integration inspection, removal, and convoy-directory cleanup.

use std::{collections::BTreeSet, path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_core::{
    checkout_integration::{
        checkout_path_from_status_and_spec, convoy_change_request_id_for_checkout, inspect_checkout_integration,
        inspect_convoy_checkout_integration,
    },
    providers::{ChannelLabel, CommandRunner},
    vcs::CheckoutRegistration,
};
use flotilla_resources::{
    canonicalize_repo_url, ChangeRequest, ChangeRequestStatus, Checkout, CheckoutIntegrationStatus, Convoy, Environment, ForgeSpec,
    ReplicaReadResolver, ResourceBackend, ResourceObject,
};
use tracing::{debug, warn};

use super::super::clone::runtime::{clone_staging_path, controller_vcs, remove_checkout_path};
use crate::reconcilers::{
    checkout::managed_checkout_reason, checkout_path_component, BranchPreservationReason, CheckoutRemoval, CheckoutRemovalOutcome,
    CheckoutRuntime, PreparedCheckout,
};

pub struct CheckoutControllerRuntime {
    runner: Arc<dyn CommandRunner>,
    vcs: Option<Arc<dyn flotilla_core::vcs::Vcs>>,
    change_requests: Option<ReplicaReadResolver<ChangeRequest>>,
    forges: Vec<flotilla_resources::ForgeSpec>,
}

pub fn removal_source_path(removal: &CheckoutRemoval) -> &str {
    match removal {
        CheckoutRemoval::Worktree { clone_path, .. }
        | CheckoutRemoval::ForcedWorktree { clone_path, .. }
        | CheckoutRemoval::LandedWorktree { clone_path, .. } => clone_path,
        CheckoutRemoval::FreshClone { target_path } | CheckoutRemoval::OrphanedWorktree { target_path } => target_path,
    }
}

impl CheckoutControllerRuntime {
    fn local_runner(&self) -> Result<Arc<dyn CommandRunner>, String> {
        Ok(Arc::clone(&self.runner))
    }

    fn checkout_path<'a>(&self, checkout: &'a ResourceObject<Checkout>) -> Result<&'a str, String> {
        checkout_path_from_status_and_spec(checkout.status.as_ref(), &checkout.spec)
            .ok_or_else(|| format!("checkout {} has no resolved path", checkout.metadata.name))
    }

    async fn observed_change_request(
        &self,
        convoy: &ResourceObject<Convoy>,
        checkout: &ResourceObject<Checkout>,
        id: &str,
    ) -> Result<Option<ChangeRequestStatus>, String> {
        let Some(change_requests) = &self.change_requests else { return Ok(None) };
        let Some(repository) = convoy.spec.repositories.iter().find(|repository| repository.repo_ref == *checkout.spec.repo_ref()) else {
            return Ok(None);
        };
        let Ok(canonical) = canonicalize_repo_url(&repository.url) else { return Ok(None) };
        let qualified = canonical.split_once("://").map(|(_, qualified)| qualified).unwrap_or(&canonical);
        let Some((service, scope)) = qualified.split_once('/') else { return Ok(None) };
        let Ok(number) = id.parse::<u64>() else { return Ok(None) };
        Ok(change_requests
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .find(|record| {
                record.object.spec.service == service && record.object.spec.scope == scope && record.object.spec.number == number
            })
            .and_then(|record| record.object.status))
    }
}

#[async_trait]
impl CheckoutRuntime for CheckoutControllerRuntime {
    async fn create_worktree(
        &self,
        _clone_path: &str,
        branch: &str,
        base_ref: Option<&str>,
        target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        let vcs = controller_vcs(&self.vcs, &self.runner, _clone_path)?;
        let materialisation = vcs
            .materialise_checkout(
                branch,
                base_ref,
                target_path,
                &managed_checkout_reason(None, Path::new(target_path).file_name().unwrap_or_default().to_string_lossy().as_ref()),
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(PreparedCheckout { commit: materialisation.commit, branch_provenance: materialisation.provenance })
    }

    async fn create_fresh_clone(
        &self,
        repo_url: &str,
        branch: &str,
        base_ref: Option<&str>,
        target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        let vcs = controller_vcs(&self.vcs, &self.runner, target_path)?;
        let materialisation = vcs.materialise_fresh_clone(repo_url, branch, base_ref, target_path).await?;
        Ok(PreparedCheckout { commit: materialisation.commit, branch_provenance: materialisation.provenance })
    }

    async fn inspect_integration(
        &self,
        checkout: &ResourceObject<Checkout>,
        convoy: Option<&ResourceObject<Convoy>>,
    ) -> Result<CheckoutIntegrationStatus, String> {
        let request = convoy
            .and_then(|convoy| convoy_change_request_id_for_checkout(convoy, checkout, &self.forges))
            .or_else(|| checkout.metadata.labels.get(flotilla_resources::CHANGE_REQUEST_ID_LABEL).cloned());
        self.inspect_integration_with_request(checkout, convoy, request.as_deref()).await
    }

    async fn remove_checkout(&self, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        self.remove_checkout_impl(removal).await
    }
}

impl CheckoutControllerRuntime {
    pub async fn inspect_integration_with_request(
        &self,
        checkout: &ResourceObject<Checkout>,
        convoy: Option<&ResourceObject<Convoy>>,
        change_request_id: Option<&str>,
    ) -> Result<CheckoutIntegrationStatus, String> {
        let runner = self.local_runner()?;
        let path = Path::new(self.checkout_path(checkout)?);
        let vcs = controller_vcs(&self.vcs, &self.runner, self.checkout_path(checkout)?)?;
        if let Some(convoy) = convoy {
            let observed_change_request = match change_request_id {
                Some(id) => self.observed_change_request(convoy, checkout, id).await?,
                None => None,
            };
            return Ok(inspect_convoy_checkout_integration(
                &*runner,
                vcs.as_ref(),
                path,
                &checkout.spec,
                convoy,
                change_request_id,
                observed_change_request.as_ref(),
            )
            .await);
        }
        Ok(inspect_checkout_integration(&*runner, vcs.as_ref(), path, &checkout.spec, change_request_id).await)
    }

    async fn remove_checkout_impl(&self, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        let runner = self.local_runner()?;
        let outcome = match removal {
            CheckoutRemoval::FreshClone { target_path } => {
                let target_path = utf8_path(target_path)?;
                remove_checkout_path(&*runner, target_path).await?;
                remove_checkout_path(&*runner, &clone_staging_path(target_path)).await?;
                Ok(CheckoutRemovalOutcome::Removed)
            }
            CheckoutRemoval::OrphanedWorktree { target_path } => {
                // The Clone resource may be gone while the linked checkout and
                // its shared registration remain. Release it before deleting
                // the last path from which its VCS can still be discovered.
                let registration = if runner.path_exists(&Path::new(target_path).join(".git")).await? {
                    let vcs = controller_vcs(&self.vcs, &self.runner, target_path)?;
                    vcs.checkout_registration(target_path, CheckoutRegistration::Release).await?;
                    Some(vcs)
                } else {
                    None
                };
                if let Err(error) = remove_checkout_path(&*runner, utf8_path(target_path)?).await {
                    if let Some(vcs) = registration {
                        let reason = managed_checkout_reason(
                            None,
                            Path::new(target_path).file_name().unwrap_or_default().to_string_lossy().as_ref(),
                        );
                        if let Err(restore) =
                            vcs.checkout_registration(target_path, CheckoutRegistration::Protect { reason: &reason }).await
                        {
                            return Err(format!("{error}; registration protection restoration failed: {restore}"));
                        }
                    }
                    return Err(error);
                }
                Ok(CheckoutRemovalOutcome::Removed)
            }
            CheckoutRemoval::Worktree { branch, target_path, .. }
            | CheckoutRemoval::ForcedWorktree { branch, target_path, .. }
            | CheckoutRemoval::LandedWorktree { branch, target_path, .. } => {
                let vcs = controller_vcs(&self.vcs, &self.runner, removal_source_path(removal))?;
                let result = if matches!(removal, CheckoutRemoval::ForcedWorktree { .. }) {
                    vcs.force_remove_materialised_checkout(branch, target_path).await?
                } else {
                    vcs.remove_materialised_checkout(branch, target_path).await?
                };
                let result = if matches!(removal, CheckoutRemoval::LandedWorktree { .. })
                    && matches!(
                        result,
                        flotilla_core::vcs::CheckoutRemoval::PreservedCheckout {
                            reason: flotilla_core::vcs::CheckoutPreservationReason::DifferentBranch,
                            ..
                        }
                    ) {
                    // A merged PR may have deleted its head ref after a squash.
                    // Archive first so a checkout that changed since settlement
                    // evidence was observed still remains recoverable.
                    vcs.force_remove_materialised_checkout(branch, target_path).await?
                } else {
                    result
                };
                match result {
                    flotilla_core::vcs::CheckoutRemoval::Removed => Ok(CheckoutRemovalOutcome::Removed),
                    flotilla_core::vcs::CheckoutRemoval::ArchivedAndRemoved { archive_path } => {
                        tracing::warn!(checkout = %target_path, archive = %archive_path, "checkout archive saved before forced removal");
                        Ok(CheckoutRemovalOutcome::ArchivedAndRemoved { archive_path })
                    }
                    flotilla_core::vcs::CheckoutRemoval::PreservedBranch { branch, reason } => {
                        let reason = match reason {
                            flotilla_core::vcs::CheckoutPreservationReason::CommitsPastBase => BranchPreservationReason::CommitsPastBase,
                            flotilla_core::vcs::CheckoutPreservationReason::CheckedOutElsewhere => {
                                BranchPreservationReason::CheckedOutElsewhere
                            }
                            flotilla_core::vcs::CheckoutPreservationReason::NotCreatedForConvoy => {
                                BranchPreservationReason::NotCreatedForConvoy
                            }
                            other => return Err(format!("checkout branch {branch} preserved: {other:?}")),
                        };
                        Ok(CheckoutRemovalOutcome::PreservedBranch { branch, reason })
                    }
                    flotilla_core::vcs::CheckoutRemoval::PreservedCheckout { path, reason } => {
                        Err(format!("checkout {path} preserved: {reason:?}"))
                    }
                }
            }
        }?;
        let target_path = match removal {
            CheckoutRemoval::FreshClone { target_path }
            | CheckoutRemoval::OrphanedWorktree { target_path }
            | CheckoutRemoval::Worktree { target_path, .. }
            | CheckoutRemoval::ForcedWorktree { target_path, .. }
            | CheckoutRemoval::LandedWorktree { target_path, .. } => target_path,
        };
        cleanup_convoy_checkout_parents(&*runner, Path::new(target_path)).await;
        Ok(outcome)
    }
}

// Managed checkout provisioning names the parent after the Convoy resource:
// <repository root>/<convoy name>/<branch>[/<repository>]. Removal receives only
// the target path, so this uses that convention rather than an owner lookup.
// A legacy path with another convoy-prefixed ancestor may match it too; only
// empty directories up to that ancestor can be pruned, never its parent.
// rmdir is atomic with respect to emptiness, so concurrent files cannot be lost.
async fn cleanup_convoy_checkout_parents(runner: &dyn CommandRunner, target: &Path) {
    let Some(convoy_dir) = target
        .ancestors()
        .skip(1)
        .find(|path| path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("convoy-")))
    else {
        return;
    };
    let convoy_ref = convoy_dir.file_name().expect("matched convoy directory has a file name").to_string_lossy();
    for parent in target.ancestors().skip(1) {
        let path = parent.to_string_lossy();
        match runner.run_output("rmdir", &["--", &path], Path::new("/"), &ChannelLabel::Default).await {
            Ok(output) if output.success() => {}
            result => {
                // If the existence probe fails, keep the parent and stop:
                // uncertainty must never authorize further pruning.
                if runner.path_exists(parent).await.unwrap_or(true) {
                    let error = match result {
                        Ok(output) => output.stderr,
                        Err(error) => error,
                    };
                    warn!(%convoy_ref, path = %parent.display(), %error, "keeping convoy checkout parent after cleanup refusal");
                    break;
                }
            }
        }
        if parent == convoy_dir {
            break;
        }
    }
}

pub async fn sweep_host_empty_convoy_directories(backend: &ResourceBackend, host: &str) -> Result<(), String> {
    // Repository roots are shared across namespaces. Include replicated convoys,
    // and fail closed on any listing error before touching the filesystem.
    // VesselReconciler reads its existing Convoy before creating Checkouts;
    // checkout provisioning therefore starts after the owner is in this store.
    // A late Git materialisation after owner deletion recreates missing parents;
    // rmdir can race only while empty and cannot remove materialised files.
    let mut live = BTreeSet::new();
    for namespace in backend.stored_namespaces::<Convoy>().await.map_err(|error| error.to_string())? {
        live.extend(
            backend
                .including_replicas::<Convoy>(&namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .map(|convoy| checkout_path_component(&convoy.object.metadata.name)),
        );
    }
    let mut roots = BTreeSet::new();
    for namespace in backend.local_namespaces::<Environment>().await.map_err(|error| error.to_string())? {
        for environment in backend.using::<Environment>(&namespace).list().await.map_err(|error| error.to_string())?.items {
            if let Some(direct) = environment.spec.host_direct {
                if direct.host_ref == host {
                    roots.insert(direct.repo_default_dir);
                }
            }
        }
    }
    for root in roots {
        sweep_empty_convoy_directories(Path::new(&root), &live).await?;
    }
    Ok(())
}

// These roots belong to this daemon's host-direct environments, so local fs
// access is appropriate. Teardown instead uses the checkout environment runner.
async fn sweep_empty_convoy_directories(root: &Path, live: &BTreeSet<String>) -> Result<(), String> {
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    while let Some(entry) = entries.next_entry().await.map_err(|error| error.to_string())? {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Managed roots reserve this prefix for convoy directories. An empty
        // user-created directory with the same prefix is indistinguishable.
        if !name.starts_with("convoy-") || live.contains(&name) || !entry.file_type().await.map_err(|error| error.to_string())?.is_dir() {
            continue;
        }
        // Never recurse: nonempty leftovers (even empty child directories) and
        // symlinks are preserved, as required by the never-nonempty contract.
        if let Err(error) = tokio::fs::remove_dir(entry.path()).await {
            match error.kind() {
                std::io::ErrorKind::NotFound => {}
                std::io::ErrorKind::DirectoryNotEmpty => debug!(convoy_ref = %name, %error, "keeping nonempty stale convoy directory"),
                _ => warn!(convoy_ref = %name, path = %entry.path().display(), %error, "could not remove stale convoy directory"),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn bootstrap_branch_ref(branch: &str) -> String {
    format!("refs/flotilla/bootstrap/{branch}")
}

pub fn utf8_path(path: &str) -> Result<&str, String> {
    if Path::new(path).to_str().is_some() {
        Ok(path)
    } else {
        Err(format!("path is not valid utf-8: {path}"))
    }
}
impl CheckoutControllerRuntime {
    pub fn new(
        runner: Arc<dyn CommandRunner>,
        vcs: Option<Arc<dyn flotilla_core::vcs::Vcs>>,
        change_requests: Option<ReplicaReadResolver<ChangeRequest>>,
        forges: Vec<ForgeSpec>,
    ) -> Self {
        Self { runner, vcs, change_requests, forges }
    }
    pub fn vcs(&self, checkout: &str) -> Result<Arc<dyn flotilla_core::vcs::Vcs>, String> {
        controller_vcs(&self.vcs, &self.runner, checkout)
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
