pub use flotilla_protocol::CheckoutIntent;
use flotilla_protocol::{CheckoutSelector, HostName, QualifiedPath};
use tracing::warn;

use crate::{
    path_context::ExecutionEnvironmentPath, provider_data::ProviderData, providers::registry::ProviderRegistry,
    terminal_manager::TerminalManager,
};

pub(super) struct CheckoutService<'a> {
    registry: &'a ProviderRegistry,
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
) -> Result<String, String> {
    match selector {
        CheckoutSelector::Path(path) => providers_data
            .checkouts
            .iter()
            .find(|(qp, _)| qp.host_id().map(|h| h.as_str()) == Some(local_host.as_str()) && qp.path == *path)
            .map(|(_, checkout)| checkout.branch.clone())
            .ok_or_else(|| format!("checkout not found: {}", path.display())),
        CheckoutSelector::Query(query) => {
            let matches: Vec<String> = providers_data
                .checkouts
                .iter()
                .filter(|(qp, checkout)| {
                    qp.host_id().map(|h| h.as_str()) == Some(local_host.as_str())
                        && (checkout.branch == *query || checkout.branch.contains(query) || qp.path.to_string_lossy().contains(query))
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
