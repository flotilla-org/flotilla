//! Clone materialisation, recovery, and per-target single-flight ownership.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex as StdMutex, Weak},
};

use async_trait::async_trait;
use flotilla_core::providers::{ChannelLabel, CommandRunner};
use flotilla_core::vcs::Vcs;
use flotilla_resources::{canonicalize_repo_url, ForgeSpec};
use tokio::sync::Mutex;

use crate::reconcilers::CloneRuntime;

#[derive(Default)]
pub struct CloneFlights {
    by_target: StdMutex<HashMap<String, Weak<Mutex<()>>>>,
}

impl CloneFlights {
    fn for_target(&self, target_path: &str) -> Arc<Mutex<()>> {
        let mut by_target = self.by_target.lock().expect("clone flights lock poisoned");
        by_target.retain(|_, flight| flight.strong_count() > 0);
        if let Some(flight) = by_target.get(target_path).and_then(Weak::upgrade) {
            return flight;
        }
        let flight = Arc::new(Mutex::new(()));
        by_target.insert(target_path.to_string(), Arc::downgrade(&flight));
        flight
    }
}

pub struct CloneControllerRuntime {
    runner: Arc<dyn CommandRunner>,
    vcs: Option<Arc<dyn Vcs>>,
    flights: Arc<CloneFlights>,
    forges: Vec<ForgeSpec>,
}

pub(crate) fn controller_vcs(
    discovered: &Option<Arc<dyn Vcs>>,
    _runner: &Arc<dyn CommandRunner>,
    _checkout: &str,
) -> Result<Arc<dyn Vcs>, String> {
    if let Some(vcs) = discovered {
        return Ok(Arc::clone(vcs));
    }
    #[cfg(test)]
    {
        use flotilla_core::{
            path_context::ExecutionEnvironmentPath,
            providers::vcs::git_worktree::GitWorktreeStrategy,
            vcs::{FlotillaVcs, GitCheckoutStrategy},
        };
        Ok(Arc::new(FlotillaVcs::new(
            ExecutionEnvironmentPath::new(_checkout),
            Arc::clone(_runner),
            GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(".".into(), Arc::clone(_runner)))),
        )))
    }
    #[cfg(not(test))]
    Err("discovered VCS provider unavailable".into())
}

#[async_trait]
impl CloneRuntime for CloneControllerRuntime {
    async fn clone_and_inspect(&self, repo_url: &str, target_path: &str) -> Result<Option<String>, String> {
        let vcs = controller_vcs(&self.vcs, &self.runner, target_path)?;
        let flight = self.flights.for_target(target_path);
        let _flight_guard = flight.lock().await;
        if let Some(inspection) = recover_existing_clone(vcs.as_ref(), &*self.runner, repo_url, target_path, &self.forges).await? {
            return Ok(inspection);
        }

        let staging_path = clone_staging_path(target_path);
        remove_checkout_path(&*self.runner, &staging_path).await?;
        let prepare = async {
            vcs.clone_repository(repo_url, Path::new(&staging_path)).await?;
            vcs.inspect_clone(Path::new(&staging_path)).await
        }
        .await;
        let inspection = match prepare {
            Ok(inspection) => inspection,
            Err(error) => return Err(cleanup_failed_checkout(&*self.runner, &staging_path, error).await),
        };
        if let Err(error) = self.runner.run("mv", &[&staging_path, target_path], Path::new("/"), &ChannelLabel::Default).await {
            let error = cleanup_failed_checkout(&*self.runner, &staging_path, format!("publish clone: {error}")).await;
            return match recover_existing_clone(vcs.as_ref(), &*self.runner, repo_url, target_path, &self.forges).await {
                Ok(Some(inspection)) => Ok(inspection),
                Ok(None) => Err(error),
                Err(adoption_error) => Err(format!("{error}; additionally failed to adopt clone at target: {adoption_error}")),
            };
        }
        Ok(inspection)
    }

    async fn inspect_existing(&self, target_path: &str) -> Result<Option<String>, String> {
        controller_vcs(&self.vcs, &self.runner, target_path)?.inspect_clone(Path::new(target_path)).await
    }
}

async fn recover_existing_clone(
    vcs: &dyn Vcs,
    runner: &dyn CommandRunner,
    repo_url: &str,
    target_path: &str,
    forges: &[ForgeSpec],
) -> Result<Option<Option<String>>, String> {
    if !runner.path_exists(Path::new(target_path)).await? {
        return Ok(None);
    }

    verify_clone_origin(vcs, repo_url, target_path, "clone target", forges).await?;
    vcs.inspect_clone(Path::new(target_path)).await.map(Some)
}

async fn verify_clone_origin(
    vcs: &dyn Vcs,
    repo_url: &str,
    target_path: &str,
    target_label: &str,
    forges: &[ForgeSpec],
) -> Result<(), String> {
    let origin = vcs
        .clone_origin(Path::new(target_path))
        .await
        .map_err(|error| format!("{target_label} {target_path} already exists but is not a reusable clone: {error}"))?;
    let origin = origin.trim();
    let same_origin = origin == repo_url
        || canonicalize_repo_url(origin)
            .ok()
            .zip(canonicalize_repo_url(repo_url).ok())
            .is_some_and(|(origin, expected)| origin == expected)
        || forges.iter().any(|forge| {
            forge
                .repository_path(origin)
                .ok()
                .flatten()
                .zip(forge.repository_path(repo_url).ok().flatten())
                .is_some_and(|(origin, expected)| origin == expected)
        });
    if !same_origin {
        return Err(format!("{target_label} {target_path} already exists with origin {origin}, expected {repo_url}"));
    }

    Ok(())
}

pub(crate) async fn remove_checkout_path(runner: &dyn CommandRunner, target_path: &str) -> Result<(), String> {
    runner.run("rm", &["-rf", target_path], Path::new("/"), &ChannelLabel::Default).await?;
    for predicate in ["-e", "-L"] {
        let remaining = runner.run_output("test", &[predicate, target_path], Path::new("/"), &ChannelLabel::Default).await?;
        if remaining.success() {
            return Err(format!("checkout cleanup reported success but path remains: {target_path}"));
        }
    }
    Ok(())
}

async fn cleanup_failed_checkout(runner: &dyn CommandRunner, target_path: &str, error: String) -> String {
    match remove_checkout_path(runner, target_path).await {
        Ok(()) => error,
        Err(cleanup_error) => format!("{error}; additionally failed to remove partial checkout: {cleanup_error}"),
    }
}

pub(crate) fn clone_staging_path(target_path: &str) -> String {
    format!("{target_path}.flotilla-clone-partial")
}
impl CloneControllerRuntime {
    pub fn new(runner: Arc<dyn CommandRunner>, vcs: Option<Arc<dyn Vcs>>, flights: Arc<CloneFlights>, forges: Vec<ForgeSpec>) -> Self {
        Self { runner, vcs, flights, forges }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
