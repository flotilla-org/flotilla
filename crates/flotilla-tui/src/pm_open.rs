//! Direct v0 realization of the row-level "open in PM" Regard.
//!
//! The TUI supplies semantic identity and capability facts. The connector
//! owns presentation-manager effects and preserves focus-not-duplicate.

use std::{collections::HashMap, path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_core::{
    path_context::ExecutionEnvironmentPath,
    providers::{
        presentation::{zellij::ZellijPresentationManager, PresentationManager},
        types::WorkspaceAttachRequest,
        ProcessCommandRunner,
    },
};
use flotilla_manifest::{
    projection::{convoy_group_path, project_segment, vessel_factory_id, vessel_group_path},
    stamp::WorkspaceStamp,
};
use flotilla_protocol::{arg::shell_quote, HostName, RepoKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenInPmTarget {
    pub namespace: String,
    pub convoy: String,
    pub vessel: Option<String>,
    pub label: String,
    pub host: Option<HostName>,
    pub project_ref: Option<String>,
    pub repo_hint: Option<RepoKey>,
    pub workspace_ref: Option<String>,
    pub materialize_ref: Option<String>,
}

#[async_trait]
pub trait PmConnector: Send + Sync {
    async fn open(&self, target: &OpenInPmTarget, working_directory: &Path) -> Result<(), String>;
}

pub struct PresentationPmConnector {
    manager: Arc<dyn PresentationManager>,
    flotilla_bin: String,
    /// Direct v0 realization predates PM→daemon convergence (#835), so keep
    /// the stable workspace refs created by this connector. The mutex covers
    /// the complete focus-or-create transaction and prevents double-open
    /// races from creating duplicate tabs.
    realized: tokio::sync::Mutex<HashMap<String, String>>,
}

impl PresentationPmConnector {
    pub fn new(manager: Arc<dyn PresentationManager>, flotilla_bin: impl Into<String>) -> Self {
        Self { manager, flotilla_bin: flotilla_bin.into(), realized: tokio::sync::Mutex::new(HashMap::new()) }
    }

    fn stamp(target: &OpenInPmTarget) -> WorkspaceStamp {
        let project = project_segment(target.project_ref.as_deref(), target.repo_hint.as_ref().map(|repo| repo.0.as_str()));
        match &target.vessel {
            Some(vessel) => WorkspaceStamp {
                kind: "flotilla-vessel".to_owned(),
                factory_id: vessel_factory_id(&target.namespace, &target.convoy, vessel),
                scope: Some(vessel_group_path(project, &target.namespace, &target.convoy, vessel)),
            },
            None => WorkspaceStamp {
                kind: "flotilla-convoy".to_owned(),
                factory_id: format!("flotilla:convoys/{}/{}", target.namespace, target.convoy),
                scope: Some(convoy_group_path(project, &target.namespace, &target.convoy)),
            },
        }
    }

    /// Stable, PM-visible fallback identity for direct v0 realizations. This
    /// remains unique across namespaces and does not change with display
    /// labels or vessel-count presentation rules.
    fn workspace_name(target: &OpenInPmTarget) -> String {
        match &target.vessel {
            Some(vessel) => format!("{}/{}/{}", target.namespace, target.convoy, vessel),
            None => format!("{}/{}", target.namespace, target.convoy),
        }
    }

    fn workspace_ref_belongs_to_manager(&self, workspace_ref: &str) -> bool {
        let prefix = self.manager.binding_scope_prefix();
        prefix.is_empty() || workspace_ref.starts_with(&prefix)
    }
}

#[async_trait]
impl PmConnector for PresentationPmConnector {
    async fn open(&self, target: &OpenInPmTarget, working_directory: &Path) -> Result<(), String> {
        let stamp = Self::stamp(target);
        let workspace_name = Self::workspace_name(target);
        let mut realized = self.realized.lock().await;
        if let Some(workspace_ref) = target.workspace_ref.as_deref().filter(|reference| self.workspace_ref_belongs_to_manager(reference)) {
            if self.manager.select_workspace(workspace_ref).await.is_ok() {
                realized.insert(stamp.factory_id, workspace_ref.to_owned());
                return Ok(());
            }
        }

        if let Some(workspace_ref) = realized.get(&stamp.factory_id).cloned() {
            if self.manager.select_workspace(&workspace_ref).await.is_ok() {
                return Ok(());
            }
            realized.remove(&stamp.factory_id);
        }

        if let Some((workspace_ref, _)) =
            self.manager.list_workspaces().await?.into_iter().find(|(_, workspace)| workspace.name == workspace_name)
        {
            self.manager.select_workspace(&workspace_ref).await?;
            realized.insert(stamp.factory_id, workspace_ref);
            return Ok(());
        }

        let materialize_ref =
            target.materialize_ref.as_deref().ok_or_else(|| format!("{} has no running session to open in the PM", target.label))?;
        let command = format!("{} attach {}", shell_quote(&self.flotilla_bin), shell_quote(materialize_ref));
        let request = WorkspaceAttachRequest::builder()
            .name(workspace_name)
            .working_directory(ExecutionEnvironmentPath::new(working_directory))
            .attach_commands(vec![(target.vessel.clone().unwrap_or_else(|| "session".to_owned()), command)])
            .stamp(stamp.clone())
            .build();
        let (workspace_ref, _) = self.manager.create_workspace(&request).await?;
        realized.insert(stamp.factory_id, workspace_ref);
        Ok(())
    }
}

/// Detect the PM enclosing this TUI. In v0 only Zellij/andamento supports
/// direct realization; absence is represented honestly as `None`.
pub fn detect_connector() -> Option<Arc<dyn PmConnector>> {
    std::env::var_os("ZELLIJ")?;
    let session = std::env::var("ZELLIJ_SESSION_NAME").ok()?;
    let runner = Arc::new(ProcessCommandRunner);
    let manager: Arc<dyn PresentationManager> = Arc::new(ZellijPresentationManager::with_session_name(runner, session));
    let flotilla_bin = std::env::current_exe().ok()?.to_string_lossy().into_owned();
    Some(Arc::new(PresentationPmConnector::new(manager, flotilla_bin)))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use flotilla_core::providers::types::Workspace;

    use super::*;

    type CreatedWorkspace = (String, Vec<(String, String)>, Option<WorkspaceStamp>);

    #[derive(Default)]
    struct FakeManager {
        workspaces: Mutex<Vec<(String, Workspace)>>,
        selected: Mutex<Vec<String>>,
        created: Mutex<Vec<CreatedWorkspace>>,
    }

    #[async_trait]
    impl PresentationManager for FakeManager {
        async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
            let workspaces = self.workspaces.lock().expect("workspaces lock").clone();
            // Force concurrent opens to interleave at the list/create seam;
            // without the connector's transaction mutex both would observe
            // the same empty snapshot and create duplicate workspaces.
            tokio::task::yield_now().await;
            Ok(workspaces)
        }

        async fn create_workspace(&self, request: &WorkspaceAttachRequest) -> Result<(String, Workspace), String> {
            self.created.lock().expect("created lock").push((request.name.clone(), request.attach_commands.clone(), request.stamp.clone()));
            let workspace = Workspace { name: request.name.clone(), correlation_keys: vec![], attachable_set_id: None };
            self.workspaces.lock().expect("workspaces lock").push(("session:7".into(), workspace.clone()));
            Ok(("session:7".into(), workspace))
        }

        async fn select_workspace(&self, workspace_ref: &str) -> Result<(), String> {
            self.selected.lock().expect("selected lock").push(workspace_ref.to_owned());
            Ok(())
        }

        async fn delete_workspace(&self, _workspace_ref: &str) -> Result<(), String> {
            Ok(())
        }

        fn binding_scope_prefix(&self) -> String {
            "session:".into()
        }
    }

    fn target() -> OpenInPmTarget {
        OpenInPmTarget {
            namespace: "dev".into(),
            convoy: "tables".into(),
            vessel: Some("implement".into()),
            label: "tables".into(),
            host: Some(HostName::new("kiwi")),
            project_ref: Some("flotilla".into()),
            repo_hint: Some(RepoKey("flotilla-org/flotilla".into())),
            workspace_ref: None,
            materialize_ref: Some("terminal-implement".into()),
        }
    }

    #[tokio::test]
    async fn focuses_a_realized_semantic_identity_instead_of_duplicating_it() {
        let manager = Arc::new(FakeManager::default());
        let connector = PresentationPmConnector::new(manager.clone(), "flotilla");

        connector.open(&target(), Path::new("/repo")).await.expect("materialize workspace");
        let mut repeated = target();
        repeated.label = "a changed display label".into();
        repeated.materialize_ref = None;
        connector.open(&repeated, Path::new("/repo")).await.expect("focus realized workspace");

        assert_eq!(*manager.selected.lock().expect("selected lock"), vec!["session:7"]);
        assert_eq!(manager.created.lock().expect("created lock").len(), 1);
    }

    #[tokio::test]
    async fn concurrent_opens_create_one_workspace_for_the_semantic_identity() {
        let manager = Arc::new(FakeManager::default());
        let connector = PresentationPmConnector::new(manager.clone(), "flotilla");
        let first = target();
        let second = first.clone();

        let (left, right) = tokio::join!(connector.open(&first, Path::new("/repo")), connector.open(&second, Path::new("/repo")));

        left.expect("first open");
        right.expect("second open");
        assert_eq!(manager.created.lock().expect("created lock").len(), 1);
        assert_eq!(*manager.selected.lock().expect("selected lock"), vec!["session:7"]);
    }

    #[tokio::test]
    async fn materializes_a_stamped_workspace_from_the_session_capability() {
        let manager = Arc::new(FakeManager::default());
        let connector = PresentationPmConnector::new(manager.clone(), "/opt/flotilla");

        connector.open(&target(), Path::new("/repo")).await.expect("materialize workspace");

        let created = manager.created.lock().expect("created lock");
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].0, "dev/tables/implement");
        assert_eq!(created[0].1, vec![("implement".into(), "'/opt/flotilla' attach 'terminal-implement'".into())]);
        assert_eq!(created[0].2.as_ref().expect("workspace stamp").factory_id, "flotilla:convoys/dev/tables/implement");
    }

    #[tokio::test]
    async fn a_new_connector_rediscovers_the_stable_direct_workspace_name() {
        let manager = Arc::new(FakeManager::default());
        PresentationPmConnector::new(manager.clone(), "flotilla")
            .open(&target(), Path::new("/repo"))
            .await
            .expect("initial materialization");
        let mut rediscovered = target();
        rediscovered.materialize_ref = None;

        PresentationPmConnector::new(manager.clone(), "flotilla")
            .open(&rediscovered, Path::new("/repo"))
            .await
            .expect("rediscover workspace after connector restart");

        assert_eq!(manager.created.lock().expect("created lock").len(), 1);
        assert_eq!(*manager.selected.lock().expect("selected lock"), vec!["session:7"]);
    }
}
