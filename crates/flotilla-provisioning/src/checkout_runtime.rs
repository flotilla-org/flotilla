//! A host's checkout work (#3108): making, inspecting and removing checkouts through the
//! environment's VCS, for the engine's Checkout controller, and the host's sweep of empty convoy
//! directories.

use std::{collections::BTreeSet, path::Path, sync::Arc};

use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandRunner;
use flotilla_resources::{
    canonicalize_repo_url, ChangeRequest, ChangeRequestStatus, CheckoutBranchProvenance, CheckoutIntegrationStatus, CheckoutSpec,
    LandedEvidence,
};
use flotilla_store::ReplicaReadResolver;
use flotilla_vcs::checkout_integration::{inspect_recorded_checkout_integration, ChangeRequestIntegrationInput, RecordedAssociation};
use flotilla_vcs::vcs as core_vcs;
use flotilla_vcs::vcs::CheckoutRegistration;
use flotilla_vcs::vcs::Vcs;
use tracing::{debug, warn};

use crate::reconcilers::clone::runtime::{clone_staging_path, controller_vcs, remove_checkout_path};

/// How a checkout is removed: a worktree from its clone (plainly, archiving first, or as landed),
/// a worktree whose clone is gone, or a clone of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutRemoval {
    Worktree { clone_path: String, branch: String, target_path: String },
    ForcedWorktree { clone_path: String, branch: String, target_path: String },
    LandedWorktree { clone_path: String, branch: String, target_path: String },
    OrphanedWorktree { target_path: String },
    FreshClone { target_path: String },
    LocalClone { target_path: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchPreservationReason {
    CommitsPastBase,
    CheckedOutElsewhere,
    NotCreatedForConvoy,
}

impl std::fmt::Display for BranchPreservationReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::CommitsPastBase => "has commits beyond the recorded base",
            Self::CheckedOutElsewhere => "is checked out in another worktree",
            Self::NotCreatedForConvoy => "was not created for this convoy",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutRemovalOutcome {
    Removed,
    ArchivedAndRemoved { archive_path: String, preserved_branch: Option<(String, BranchPreservationReason)> },
    PreservedBranch { branch: String, reason: BranchPreservationReason },
    PreservedOrphanedCheckout { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCheckout {
    pub commit: Option<String>,
    pub branch_provenance: CheckoutBranchProvenance,
}

/// Diagnostic identity shared by convoy and standalone registration callers.
pub fn managed_checkout_reason(owner: Option<&str>, checkout: &str) -> String {
    format!("flotilla-managed: {}/{}", owner.unwrap_or("standalone"), checkout)
}

pub struct CheckoutControllerRuntime {
    runner: Arc<dyn CommandRunner>,
    vcs: Option<Arc<dyn Vcs>>,
    change_requests: Option<ReplicaReadResolver<ChangeRequest>>,
}

/// Selects the VCS discovery path used by the daemon’s environment-routing adapter.
pub fn removal_source_path(removal: &CheckoutRemoval) -> &str {
    match removal {
        CheckoutRemoval::Worktree { clone_path, .. }
        | CheckoutRemoval::ForcedWorktree { clone_path, .. }
        | CheckoutRemoval::LandedWorktree { clone_path, .. } => clone_path,
        CheckoutRemoval::LocalClone { target_path }
        | CheckoutRemoval::FreshClone { target_path }
        | CheckoutRemoval::OrphanedWorktree { target_path } => target_path,
    }
}

impl CheckoutControllerRuntime {
    fn local_runner(&self) -> Result<Arc<dyn CommandRunner>, String> {
        Ok(Arc::clone(&self.runner))
    }

    /// The replicated observation of change request `id` of the repository at `url`.
    async fn observed_change_request_of(&self, url: &str, id: &str) -> Result<Option<ChangeRequestStatus>, String> {
        let Some(change_requests) = &self.change_requests else { return Ok(None) };
        let Ok(canonical) = canonicalize_repo_url(url) else { return Ok(None) };
        let qualified = canonical.split_once("://").map(|(_, qualified)| qualified).unwrap_or(&canonical);
        let Some((service, scope)) = qualified.split_once('/') else { return Ok(None) };
        let Ok(number) = id.parse::<u64>() else { return Ok(None) };
        let copies = change_requests.list().await.map_err(|error| error.to_string())?;
        // A demanded local placeholder may precede the observing host's copy.
        // Match the subject first, then use the shared whole-observation selection.
        Ok(flotilla_resources::pick_replica_read(
            copies.items.into_iter().filter(|record| {
                record.object.spec.service == service && record.object.spec.scope == scope && record.object.spec.number == number
            }),
            |record| (&record.object, matches!(record.provenance, flotilla_resources::ResourceProvenance::Local)),
        )
        .and_then(|record| record.object.status))
    }
}

impl CheckoutControllerRuntime {
    pub async fn create_worktree(
        &self,
        clone_path: &str,
        branch: &str,
        base_ref: Option<&str>,
        target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        let vcs = controller_vcs(&self.vcs, &self.runner, clone_path)?;
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

    pub async fn create_fresh_clone(
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

    pub async fn create_local_clone(&self, request: core_vcs::LocalCloneRequest<'_>) -> Result<(PreparedCheckout, Option<String>), String> {
        let vcs = controller_vcs(&self.vcs, &self.runner, request.source)?;
        let (result, message) = vcs.materialise_local_clone(request).await?;
        Ok((PreparedCheckout { commit: result.commit, branch_provenance: result.provenance }, message))
    }

    pub async fn remove_checkout(&self, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        self.remove_checkout_impl(removal).await
    }

    /// Inspects the checkout of `spec` at `path` for settlement, as the engine's Checkout
    /// controller releases it: against the change request its holder associates with its branch
    /// (`association`), whose replicated observation of the repository at `repository_url` is
    /// read, and the landing evidence held from an earlier inspection.
    pub async fn inspect_at(
        &self,
        path: &str,
        spec: &CheckoutSpec,
        repository_url: Option<&str>,
        association: RecordedAssociation<'_>,
        landed_evidence: Option<&LandedEvidence>,
    ) -> Result<CheckoutIntegrationStatus, String> {
        let runner = self.local_runner()?;
        let vcs = controller_vcs(&self.vcs, &self.runner, path)?;
        let observed = match (repository_url, association.change_request_id) {
            (Some(url), Some(id)) => self.observed_change_request_of(url, id).await?,
            _ => None,
        };
        Ok(inspect_recorded_checkout_integration(
            &*runner,
            vcs.as_ref(),
            Path::new(path),
            spec,
            association,
            ChangeRequestIntegrationInput { status: observed.as_ref(), landed_evidence },
        )
        .await)
    }

    async fn remove_checkout_impl(&self, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        let runner = self.local_runner()?;
        let outcome = match removal {
            CheckoutRemoval::LocalClone { target_path } => {
                let vcs = controller_vcs(&self.vcs, &self.runner, target_path)?;
                vcs.verify_local_clone_reclaimable(target_path).await?;
                remove_checkout_path(&*runner, utf8_path(target_path)?).await?;
                remove_checkout_path(&*runner, &clone_staging_path(utf8_path(target_path)?)).await?;
                Ok(CheckoutRemovalOutcome::Removed)
            }
            CheckoutRemoval::FreshClone { target_path } => {
                // A fresh clone is the only copy of its unpublished work, so apply
                // the guard local clones get: hold the finalizer rather than remove it.
                if runner.path_exists(Path::new(target_path)).await? {
                    let vcs = controller_vcs(&self.vcs, &self.runner, target_path)?;
                    vcs.verify_local_clone_reclaimable(target_path).await?;
                }
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
                    if let Err(error) = vcs.checkout_registration(target_path, CheckoutRegistration::Release).await {
                        return match vcs.orphaned_worktree(target_path).await {
                            Ok(true) => Ok(CheckoutRemovalOutcome::PreservedOrphanedCheckout { path: target_path.to_string() }),
                            Ok(false) => Err(error),
                            Err(probe_error) => Err(format!("{error}; orphaned worktree probe failed: {probe_error}")),
                        };
                    }
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
                        core_vcs::CheckoutRemoval::PreservedCheckout { reason: core_vcs::CheckoutPreservationReason::DifferentBranch, .. }
                    ) {
                    // A merged PR may have deleted its head ref after a squash.
                    // Archive first so a checkout that changed since settlement
                    // evidence was observed still remains recoverable.
                    vcs.force_remove_materialised_checkout(branch, target_path).await?
                } else {
                    result
                };
                match result {
                    core_vcs::CheckoutRemoval::Removed => Ok(CheckoutRemovalOutcome::Removed),
                    core_vcs::CheckoutRemoval::ArchivedAndRemoved { archive_path, preserved_branch } => {
                        tracing::warn!(checkout = %target_path, archive = %archive_path, "checkout archive saved before forced removal");
                        let preserved_branch = preserved_branch
                            .map(|(branch, reason)| branch_preservation_reason(reason).map(|reason| (branch, reason)))
                            .transpose()?;
                        Ok(CheckoutRemovalOutcome::ArchivedAndRemoved { archive_path, preserved_branch })
                    }
                    core_vcs::CheckoutRemoval::PreservedBranch { branch, reason } => {
                        let reason = branch_preservation_reason(reason)?;
                        Ok(CheckoutRemovalOutcome::PreservedBranch { branch, reason })
                    }
                    core_vcs::CheckoutRemoval::PreservedCheckout {
                        path,
                        reason: core_vcs::CheckoutPreservationReason::OrphanedCheckout,
                    } => Ok(CheckoutRemovalOutcome::PreservedOrphanedCheckout { path }),
                    core_vcs::CheckoutRemoval::PreservedCheckout { path, reason } => Err(format!("checkout {path} preserved: {reason:?}")),
                }
            }
        }?;
        let target_path = match removal {
            CheckoutRemoval::LocalClone { target_path }
            | CheckoutRemoval::FreshClone { target_path }
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

pub async fn sweep_host_empty_convoy_directories(
    convoys: &dyn flotilla_resources::convoy_records::ConvoyRecords,
    environments: &dyn flotilla_resources::environment_records::EnvironmentRecords,
    host: &str,
) -> Result<(), String> {
    // Repository roots are shared by every home's convoys: include each launch this store holds,
    // and fail closed on any listing error before touching the filesystem. A launch is held
    // before its checkouts are requested, so provisioning starts after it is listed here.
    // A late Git materialisation after the launch is gone recreates missing parents;
    // rmdir can race only while empty and cannot remove materialised files.
    // A launch that cannot be read is still held: its roots are kept too.
    let live = convoys.every()?.names().map(flotilla_resources::convoy::engine::launch_slug).collect::<BTreeSet<_>>();
    // The host's own environments are homed here, and never released.
    let mut roots = BTreeSet::new();
    for environment in environments.live()? {
        if let Some(direct) = environment.spec.host_direct {
            if direct.host_ref == host {
                roots.insert(direct.repo_default_dir);
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
        vcs: Option<Arc<dyn Vcs>>,
        change_requests: Option<ReplicaReadResolver<ChangeRequest>>,
    ) -> Self {
        Self { runner, vcs, change_requests }
    }
    pub fn vcs(&self, checkout: &str) -> Result<Arc<dyn Vcs>, String> {
        controller_vcs(&self.vcs, &self.runner, checkout)
    }
}

fn branch_preservation_reason(reason: core_vcs::CheckoutPreservationReason) -> Result<BranchPreservationReason, String> {
    match reason {
        core_vcs::CheckoutPreservationReason::CommitsPastBase => Ok(BranchPreservationReason::CommitsPastBase),
        core_vcs::CheckoutPreservationReason::CheckedOutElsewhere => Ok(BranchPreservationReason::CheckedOutElsewhere),
        core_vcs::CheckoutPreservationReason::NotCreatedForConvoy => Ok(BranchPreservationReason::NotCreatedForConvoy),
        other => Err(format!("unexpected branch preservation reason: {other:?}")),
    }
}

#[cfg(test)]
mod tests;
