// ADR 0047: retained only to decode and purge previous-generation rows.
// Remove this kind and cleanup one fleet roll after removal step 3 (#2915).
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::StatusPatch};

define_resource!(Presentation, "presentations", PresentationSpec, PresentationStatus, PresentationStatusPatch);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct PresentationSpec {
    pub convoy_ref: String,
    pub presentation_policy_ref: String,
    pub name: String,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub process_selector: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PresentationPhase {
    #[default]
    Pending,
    Active,
    TornDown,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentationStatus {
    pub phase: PresentationPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_workspace_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_presentation_manager: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_spec_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentationStatusPatch {
    /// Duplicate-safe while already active; from another phase this records a new realization attempt.
    MarkActive {
        presentation_manager: String,
        workspace_ref: String,
        spec_hash: String,
        ready_at: DateTime<Utc>,
    },
    MarkTornDown {
        message: Option<String>,
    },
    MarkFailed {
        message: String,
    },
}

impl StatusPatch<PresentationStatus> for PresentationStatusPatch {
    fn apply(&self, status: &mut PresentationStatus) {
        match self {
            Self::MarkActive { presentation_manager, workspace_ref, spec_hash, ready_at } => {
                let was_active = status.phase == PresentationPhase::Active;
                status.phase = PresentationPhase::Active;
                status.observed_presentation_manager = Some(presentation_manager.clone());
                status.observed_workspace_ref = Some(workspace_ref.clone());
                status.observed_spec_hash = Some(spec_hash.clone());
                if !was_active {
                    status.ready_at = Some(*ready_at);
                } else {
                    status.ready_at.get_or_insert(*ready_at);
                }
                status.message = None;
            }
            Self::MarkTornDown { message } => {
                status.phase = PresentationPhase::TornDown;
                status.observed_presentation_manager = None;
                status.observed_workspace_ref = None;
                status.observed_spec_hash = None;
                status.message = message.clone();
            }
            Self::MarkFailed { message } => {
                status.phase = PresentationPhase::Failed;
                status.message = Some(message.clone());
            }
        }
    }
}

/// ADR 0047 one-generation cleanup. Remove one fleet roll after #2915 step 3.
/// Presentation creation has stopped; release its retired finalizer even when
/// deletion already began, then request deletion without provider teardown.
/// Only the retired finalizer is released; unrelated finalizers may keep rows
/// pending deletion. Call after stored-record quarantine and before controllers.
pub async fn purge_retired_presentations(backend: &crate::ResourceBackend, namespace: &str) -> Result<(), crate::ResourceError> {
    let resolver = backend.clone().using::<Presentation>(namespace);
    let objects = resolver.list().await?.items;
    let original_count = objects.len();
    for object in objects {
        retire_presentation(&resolver, object).await?;
    }
    let remaining = resolver.list().await?.items.len();
    tracing::info!(namespace, purged = original_count.saturating_sub(remaining), remaining, "retired Presentation cleanup complete");
    Ok(())
}

// A stale startup snapshot gets one fresh read and retry. Persistent conflicts
// remain visible to the caller; no row is silently skipped.
async fn retire_presentation(
    resolver: &crate::TypedResolver<Presentation>,
    mut object: crate::ResourceObject<Presentation>,
) -> Result<(), crate::ResourceError> {
    for attempt in 0..2 {
        if !object.metadata.finalizers.iter().any(|name| name == "flotilla.work/presentation-teardown") {
            break;
        }
        let meta = crate::InputMeta::from(&object.metadata).without_finalizer("flotilla.work/presentation-teardown");
        match resolver.update(&meta, &object.metadata.resource_version, &object.spec).await {
            Ok(_) => break,
            Err(crate::ResourceError::NotFound { .. }) => return Ok(()),
            Err(crate::ResourceError::Conflict { .. }) if attempt == 0 => {
                object = match resolver.get(&object.metadata.name).await {
                    Ok(current) => current,
                    Err(crate::ResourceError::NotFound { .. }) => return Ok(()),
                    Err(error) => return Err(error),
                };
            }
            Err(error) => return Err(error),
        }
    }
    match resolver.delete(&object.metadata.name).await {
        Ok(()) | Err(crate::ResourceError::NotFound { .. }) => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::*;
    use crate::{InputMeta, ResourceBackend, ResourceError, SqliteBackend};
    use hegel::generators as gs;

    // Startup retirement retries one stale snapshot with the current metadata.
    // Generate every old phase and an unrelated finalizer on both real stores.
    #[hegel::test]
    fn stale_retirement_snapshot_retries_with_current_metadata(tc: hegel::TestCase) {
        let phase = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        let unrelated = tc.draw(gs::booleans());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            for backend in
                [ResourceBackend::InMemory(Default::default()), ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite"))]
            {
                let resolver = backend.using::<Presentation>("flotilla");
                let stale = resolver
                    .create(
                        &InputMeta::builder().name("old".into()).build().with_added_finalizer("flotilla.work/presentation-teardown"),
                        &PresentationSpec::builder()
                            .convoy_ref("old".into())
                            .presentation_policy_ref("default".into())
                            .name("old".into())
                            .build(),
                    )
                    .await
                    .expect("old row");
                let mut meta = InputMeta::from(&stale.metadata);
                if unrelated {
                    meta = meta.with_added_finalizer("other-controller");
                }
                meta.annotations.insert("concurrent-update".into(), "preserve".into());
                let current = resolver.update(&meta, &stale.metadata.resource_version, &stale.spec).await.expect("newer version");
                let status = PresentationStatus {
                    phase: [PresentationPhase::Pending, PresentationPhase::Active, PresentationPhase::Failed, PresentationPhase::TornDown]
                        [phase],
                    ..Default::default()
                };
                resolver.update_status("old", &current.metadata.resource_version, &status).await.expect("new status");
                retire_presentation(&resolver, stale).await.expect("retry stale snapshot");
                if unrelated {
                    let retained = resolver.get("old").await.expect("unrelated finalizer");
                    assert_eq!(retained.metadata.finalizers, vec!["other-controller".to_string()]);
                    assert_eq!(retained.metadata.annotations.get("concurrent-update").map(String::as_str), Some("preserve"));
                    assert_eq!(retained.status, Some(status));
                    assert!(retained.metadata.deletion_timestamp.is_some());
                } else {
                    assert!(matches!(resolver.get("old").await, Err(ResourceError::NotFound { .. })));
                }
            }
        });
    }
}
