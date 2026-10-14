//! Shared command identity and resource-mutation helpers.

use flotilla_protocol::commands::RepositoryIdentityChange;
use flotilla_protocol::ResourceJsonResponse;
use flotilla_resources::{DocumentKey, Resolution, ResolutionAction};
use flotilla_resources::{InputMeta, ManifestRoot, ResourceError};
use flotilla_store::{get_resource_kind, ResourceBackend};
use std::path::{Path, PathBuf};
pub fn fallback_repo_identity(path: &Path) -> flotilla_protocol::RepoIdentity {
    flotilla_protocol::RepoIdentity { authority: "local".into(), path: path.to_string_lossy().into_owned() }
}

pub fn empty_repo_identity() -> flotilla_protocol::RepoIdentity {
    flotilla_protocol::RepoIdentity { authority: String::new(), path: String::new() }
}

#[derive(Debug)]
pub struct AddRepoOutcome {
    pub tracked_path: PathBuf,
    pub resolved_from: Option<PathBuf>,
    pub identity_change: Option<RepositoryIdentityChange>,
}

pub(crate) async fn request_manifest_resolution(
    backend: &ResourceBackend,
    namespace: &str,
    kind: &str,
    name: &str,
    action: flotilla_protocol::ManifestResolution,
    requested_by: &str,
) -> Result<ResourceJsonResponse, String> {
    let object = get_resource_kind(backend, namespace, kind, name).await.map_err(|error| error.to_string())?;
    let annotations = object
        .value
        .get("metadata")
        .and_then(|metadata| metadata.get("annotations"))
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| format!("{kind}/{name} has no manifest provenance"))?;
    let root = annotations
        .get("flotilla.work/manifest-reconciler-root")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{kind}/{name} has no manifest root"))?;
    let path = annotations
        .get("flotilla.work/manifest-path")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{kind}/{name} has no manifest path"))?;
    let key = DocumentKey { path: path.to_string(), kind: object.kind, namespace: namespace.to_string(), name: name.to_string() };
    let roots = backend.using::<ManifestRoot>(namespace);
    // One-roll compatibility: older provenance named the host rather than the
    // materialized ManifestRoot. Resolve that host together with its source.
    // Host config declares one source today; remove this fallback after one roll.
    let root_name = match roots.get(root).await {
        Ok(_) => root.to_string(),
        Err(ResourceError::NotFound { .. }) => {
            let source = annotations
                .get("flotilla.work/manifest-source")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("{kind}/{name} has no manifest source"))?;
            let matching = roots
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .filter(|candidate| candidate.spec.host == root && candidate.spec.source == source)
                .map(|candidate| candidate.metadata.name)
                .collect::<Vec<_>>();
            match matching.as_slice() {
                [name] => name.clone(),
                [] => return Err(format!("ManifestRoot for {kind}/{name} is no longer declared")),
                _ => return Err(format!("ManifestRoot for {kind}/{name} is ambiguous")),
            }
        }
        Err(error) => return Err(error.to_string()),
    };
    let action = match action {
        flotilla_protocol::ManifestResolution::Sync => ResolutionAction::Sync,
        flotilla_protocol::ManifestResolution::Adopt => ResolutionAction::Adopt,
    };
    for _ in 0..3 {
        let existing = roots.get(&root_name).await.map_err(|error| error.to_string())?;
        let mut spec = existing.spec;
        spec.resolutions.insert(
            key.clone(),
            Resolution {
                action,
                token: uuid::Uuid::new_v4().to_string(),
                requested_by: if requested_by.is_empty() { "unknown".to_string() } else { requested_by.to_string() },
            },
        );
        match roots.update(&InputMeta::from(&existing.metadata), &existing.metadata.resource_version, &spec).await {
            Ok(updated) => {
                return Ok(ResourceJsonResponse {
                    kind: "ManifestRoot".to_string(),
                    plural: "manifestroots".to_string(),
                    namespace: namespace.to_string(),
                    value: serde_json::to_value(updated).map_err(|error| error.to_string())?,
                    replica_origin: None,
                })
            }
            Err(ResourceError::Conflict { .. }) => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("ManifestRoot spec conflict retry budget exhausted".to_string())
}

/// Retry only replay-safe mutations and only optimistic conflicts.
pub(crate) async fn retry_resource_apply<T, F, Fut>(kind: &str, mut apply: F) -> Result<T, ResourceError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ResourceError>>,
{
    let attempts = if matches!(kind, "Artifact" | "Message") { 16 } else { 1 };
    for attempt in 0..attempts {
        let result = apply().await;
        if attempt + 1 == attempts || !matches!(result, Err(ResourceError::Conflict { .. })) {
            return result;
        }
    }
    unreachable!("retry budget always makes an attempt")
}

use flotilla_protocol::RepoIdentity;
use flotilla_resources::RepositorySpec;
/// Forge identity or Repository key identifies a row independently of any
/// execution location, including repositories that have no forge.
pub fn repository_event_identity(spec: &RepositorySpec, _path: Option<&Path>) -> RepoIdentity {
    if let Some(forge) = spec.forge() {
        let authority = forge
            .service_url
            .strip_prefix("https://")
            .or_else(|| forge.service_url.strip_prefix("http://"))
            .unwrap_or(&forge.service_url)
            .trim_end_matches('/')
            .to_string();
        RepoIdentity { authority, path: forge.repository.clone() }
    } else {
        RepoIdentity { authority: "repository".into(), path: spec.key().to_string() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Replay-safe commands retry conflicts within their existing finite budget.
    #[tokio::test]
    async fn message_apply_retries_only_conflicts_with_a_finite_budget() {
        for kind in ["Message", "Artifact"] {
            let mut attempts = 0;
            let result = retry_resource_apply(kind, || {
                attempts += 1;
                std::future::ready(if attempts < 3 { Err(ResourceError::conflict("message", "concurrent status")) } else { Ok(attempts) })
            })
            .await;
            assert_eq!(result.unwrap(), 3);
            let mut attempts = 0;
            let result: Result<(), _> = retry_resource_apply(kind, || {
                attempts += 1;
                std::future::ready(Err(ResourceError::conflict("message", "persistent conflict")))
            })
            .await;
            assert!(matches!(result, Err(ResourceError::Conflict { .. })));
            assert_eq!(attempts, 16);
            let mut attempts = 0;
            let result: Result<(), _> = retry_resource_apply(kind, || {
                attempts += 1;
                std::future::ready(Err(ResourceError::invalid("invalid intent")))
            })
            .await;
            assert!(matches!(result, Err(ResourceError::Invalid { .. })));
            assert_eq!(attempts, 1);
        }
        let mut attempts = 0;
        let result: Result<(), _> = retry_resource_apply("Convoy", || {
            attempts += 1;
            std::future::ready(Err(ResourceError::conflict("convoy", "no replay contract")))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts, 1);
    }
}
