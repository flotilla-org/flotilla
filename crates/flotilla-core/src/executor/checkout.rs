pub use flotilla_protocol::CheckoutIntent;
use flotilla_protocol::{provider_data::Checkout, qualified_path::QualifiedPath, CheckoutSelector, HostName};

use crate::{
    path_context::{canonical_or_original, ExecutionEnvironmentPath},
    provider_data::ProviderData,
    vcs::Vcs,
};

pub(super) struct CheckoutService<'a> {
    vcs: &'a dyn Vcs,
}

/// Returns whether a checkout key belongs to the executor's local provider snapshot.
///
/// Host-id-qualified keys are treated as local-only because executor resolution
/// operates on the repo's unmerged local provider data (or equivalent
/// environment-local data). Peer overlay checkouts must not reach this path;
/// once peer routing grows a stronger owner model this predicate should narrow
/// accordingly instead of treating all `HostId`-qualified paths as executable
/// locally.
pub(crate) fn checkout_is_local_owned(host_path: &QualifiedPath, local_host: &HostName) -> bool {
    host_path.host_name() == Some(local_host) || host_path.host_id().is_some()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheckoutResolutionScope {
    Any,
    Local,
    Host(HostName),
    RemoteAny,
}

fn checkout_is_local(checkout_path: &QualifiedPath, checkout: &Checkout, local_host: &HostName) -> bool {
    match checkout.host_name.as_ref().or_else(|| checkout_path.host_name()) {
        Some(host_name) => host_name == local_host,
        None => checkout_is_local_owned(checkout_path, local_host),
    }
}

pub(crate) fn checkout_matches_scope(
    checkout_path: &QualifiedPath,
    checkout: &Checkout,
    local_host: &HostName,
    scope: &CheckoutResolutionScope,
) -> bool {
    let effective_host_name = checkout.host_name.as_ref().or_else(|| checkout_path.host_name());
    match scope {
        CheckoutResolutionScope::Any => true,
        CheckoutResolutionScope::Local => checkout_is_local(checkout_path, checkout, local_host),
        CheckoutResolutionScope::RemoteAny => !checkout_is_local(checkout_path, checkout, local_host),
        CheckoutResolutionScope::Host(target_host) => effective_host_name == Some(target_host),
    }
}

impl<'a> CheckoutService<'a> {
    pub(super) fn new(vcs: &'a dyn Vcs) -> Self {
        Self { vcs }
    }

    pub(super) async fn validate_target(
        &self,
        _repo_root: &ExecutionEnvironmentPath,
        branch: &str,
        intent: CheckoutIntent,
    ) -> Result<(), String> {
        self.vcs.validate_target(branch, intent).await
    }

    pub(super) async fn create_checkout(
        &self,
        _repo_root: &ExecutionEnvironmentPath,
        branch: &str,
        create_branch: bool,
    ) -> Result<ExecutionEnvironmentPath, String> {
        let (path, _checkout) = self.vcs.create_checkout(branch, create_branch).await?;
        Ok(path)
    }

    pub(super) async fn remove_checkout(&self, _repo_root: &ExecutionEnvironmentPath, branch: &str) -> Result<(), String> {
        self.vcs.remove_checkout(branch).await?;

        Ok(())
    }
}

pub(super) fn resolve_checkout_branch(
    selector: &CheckoutSelector,
    providers_data: &ProviderData,
    local_host: &HostName,
    scope: &CheckoutResolutionScope,
) -> Result<String, String> {
    match selector {
        CheckoutSelector::Path(path) => {
            let physical_path = canonical_or_original(path);
            providers_data
                .checkouts
                .iter()
                .find(|(host_path, checkout)| {
                    if !checkout_matches_scope(host_path, checkout, local_host, scope) {
                        return false;
                    }
                    if checkout_is_local(host_path, checkout, local_host) {
                        canonical_or_original(&host_path.path) == physical_path
                    } else {
                        host_path.path == *path
                    }
                })
                .map(|(_, checkout)| checkout.branch.clone())
                .ok_or_else(|| format!("checkout not found: {}", path.display()))
        }
        CheckoutSelector::Query(query) => {
            let matches: Vec<String> = providers_data
                .checkouts
                .iter()
                .filter(|(host_path, checkout)| {
                    checkout_matches_scope(host_path, checkout, local_host, scope)
                        && (checkout.branch == *query
                            || checkout.branch.contains(query)
                            || host_path.path.to_string_lossy().contains(query))
                })
                .map(|(_, checkout)| checkout.branch.clone())
                .collect();
            match matches.len() {
                0 => Err(format!("checkout not found: {query}")),
                1 => Ok(matches[0].clone()),
                _ => Err(format!("checkout selector is ambiguous: {query}")),
            }
        }
    }
}
