pub use flotilla_protocol::CheckoutIntent;
use flotilla_protocol::{provider_data::Checkout, qualified_path::QualifiedPath, CheckoutSelector, HostName};
use tracing::warn;

use crate::{
    path_context::ExecutionEnvironmentPath, provider_data::ProviderData, providers::registry::ProviderRegistry,
    terminal_manager::TerminalManager,
};

pub(super) struct CheckoutService<'a> {
    registry: &'a ProviderRegistry,
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

pub(crate) fn checkout_matches_scope(
    checkout_path: &QualifiedPath,
    checkout: &Checkout,
    local_host: &HostName,
    scope: &CheckoutResolutionScope,
) -> bool {
    let effective_host_name = checkout.host_name.as_ref().or_else(|| checkout_path.host_name());
    match scope {
        CheckoutResolutionScope::Any => true,
        CheckoutResolutionScope::Local => match effective_host_name {
            Some(host_name) => host_name == local_host,
            None => checkout_is_local_owned(checkout_path, local_host),
        },
        CheckoutResolutionScope::RemoteAny => match effective_host_name {
            Some(host_name) => host_name != local_host,
            None => !checkout_is_local_owned(checkout_path, local_host),
        },
        CheckoutResolutionScope::Host(target_host) => effective_host_name == Some(target_host),
    }
}

impl<'a> CheckoutService<'a> {
    pub(super) fn new(registry: &'a ProviderRegistry) -> Self {
        Self { registry }
    }

    pub(super) async fn validate_target(
        &self,
        repo_root: &ExecutionEnvironmentPath,
        branch: &str,
        intent: CheckoutIntent,
    ) -> Result<(), String> {
        let checkout_manager =
            self.registry.checkout_managers.preferred().cloned().ok_or_else(|| "No checkout manager available".to_string())?;
        checkout_manager.validate_target(repo_root, branch, intent).await
    }

    pub(super) async fn create_checkout(
        &self,
        repo_root: &ExecutionEnvironmentPath,
        branch: &str,
        create_branch: bool,
    ) -> Result<ExecutionEnvironmentPath, String> {
        let checkout_manager =
            self.registry.checkout_managers.preferred().cloned().ok_or_else(|| "No checkout manager available".to_string())?;
        let (path, _checkout) = checkout_manager.create_checkout(repo_root, branch, create_branch).await?;
        Ok(path)
    }

    pub(super) async fn remove_checkout(
        &self,
        repo_root: &ExecutionEnvironmentPath,
        branch: &str,
        deleted_checkout_paths: &[QualifiedPath],
        terminal_manager: Option<&TerminalManager>,
    ) -> Result<(), String> {
        let checkout_manager =
            self.registry.checkout_managers.preferred().cloned().ok_or_else(|| "No checkout manager available".to_string())?;
        checkout_manager.remove_checkout(repo_root, branch).await?;

        // Cascade: remove attachable sets and kill terminal sessions for deleted checkouts
        if let Some(tm) = terminal_manager {
            if let Err(err) = tm.cascade_delete(deleted_checkout_paths).await {
                warn!(err = %err, "failed to cascade delete terminal sessions (best-effort)");
            }
        }

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
        CheckoutSelector::Path(path) => providers_data
            .checkouts
            .iter()
            .find(|(host_path, checkout)| checkout_matches_scope(host_path, checkout, local_host, scope) && host_path.path == *path)
            .map(|(_, checkout)| checkout.branch.clone())
            .ok_or_else(|| format!("checkout not found: {}", path.display())),
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
