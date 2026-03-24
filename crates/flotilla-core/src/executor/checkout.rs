use std::path::Path;

use flotilla_protocol::{CheckoutSelector, HostName, HostPath};
use tracing::warn;

use crate::{
    path_context::ExecutionEnvironmentPath,
    provider_data::ProviderData,
    providers::{registry::ProviderRegistry, run, CommandRunner},
    terminal_manager::TerminalManager,
};

#[derive(Clone, Copy)]
pub enum CheckoutIntent {
    ExistingBranch,
    FreshBranch,
}

pub(super) struct CheckoutService<'a> {
    registry: &'a ProviderRegistry,
    runner: &'a dyn CommandRunner,
}

impl<'a> CheckoutService<'a> {
    pub(super) fn new(registry: &'a ProviderRegistry, runner: &'a dyn CommandRunner) -> Self {
        Self { registry, runner }
    }

    pub(super) async fn validate_target(&self, repo_root: &Path, branch: &str, intent: CheckoutIntent) -> Result<(), String> {
        validate_checkout_target(repo_root, branch, intent, self.runner).await
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
        deleted_checkout_paths: &[HostPath],
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
            .find(|(host_path, _)| host_path.host == *local_host && host_path.path == *path)
            .map(|(_, checkout)| checkout.branch.clone())
            .ok_or_else(|| format!("checkout not found: {}", path.display())),
        CheckoutSelector::Query(query) => {
            let matches: Vec<String> = providers_data
                .checkouts
                .iter()
                .filter(|(host_path, checkout)| {
                    host_path.host == *local_host
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

pub(super) async fn validate_checkout_target(
    repo_root: &Path,
    branch: &str,
    intent: CheckoutIntent,
    runner: &dyn CommandRunner,
) -> Result<(), String> {
    let local_exists = run!(runner, "git", &["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")], repo_root).is_ok();
    let remote_exists =
        run!(runner, "git", &["show-ref", "--verify", "--quiet", &format!("refs/remotes/origin/{branch}")], repo_root).is_ok();
    match intent {
        CheckoutIntent::ExistingBranch if local_exists || remote_exists => Ok(()),
        CheckoutIntent::ExistingBranch => Err(format!("branch not found: {branch}")),
        CheckoutIntent::FreshBranch if local_exists || remote_exists => Err(format!("branch already exists: {branch}")),
        CheckoutIntent::FreshBranch => Ok(()),
    }
}

pub(super) async fn write_branch_issue_links(repo_root: &Path, branch: &str, issue_ids: &[(String, String)], runner: &dyn CommandRunner) {
    use std::collections::HashMap;

    let mut by_provider: HashMap<&str, Vec<&str>> = HashMap::new();
    for (provider, id) in issue_ids {
        by_provider.entry(provider.as_str()).or_default().push(id.as_str());
    }
    for (provider, ids) in by_provider {
        let key = format!("branch.{branch}.flotilla.issues.{provider}");
        let value = ids.join(",");
        if let Err(err) = run!(runner, "git", &["config", &key, &value], repo_root) {
            warn!(err = %err, "failed to write issue link");
        }
    }
}
