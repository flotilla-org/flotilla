//! The ManifestRoot port over the old store, with `[primitives]` off (#3824 layer 2c): the
//! ManifestRoot resources' spec and status, written as the manifest reconciler, project ops and the
//! CLI wrote them. The engine's port replaces this one with the switch on; both go with the switch.

use std::collections::BTreeMap;

use flotilla_resources::{
    manifest_roots::{pass_status, source_status, ManifestRoots, PassReport, RootDeclaration, RootView},
    DocumentKey, DocumentPhase, DocumentState, InputMeta, ManifestRoot, ManifestRootSpec, ManifestRootStatus, ReadResourceObject,
    Resolution, ResolutionAction, ResolutionOutcome, ResourceError, ResourceProvenance, MANAGED_BY_LABEL,
};

use crate::{ResourceBackend, StoreQuery};

/// The label that marks a project's operational-entry root.
const OPS_MANAGER: &str = "ops-charter";

/// The optimistic-concurrency retries of one root write.
const RETRIES: usize = 3;

/// The ManifestRoots of the old store.
#[derive(Clone)]
pub struct OldStoreManifestRoots {
    backend: ResourceBackend,
}

impl OldStoreManifestRoots {
    pub fn new(backend: ResourceBackend) -> Self {
        Self { backend }
    }

    fn view(read: ReadResourceObject<ManifestRoot>) -> RootView {
        RootView {
            name: read.object.metadata.name,
            local: matches!(read.provenance, ResourceProvenance::Local),
            spec: read.object.spec,
            status: read.object.status.unwrap_or_default(),
        }
    }

    async fn local(&self, namespace: &str, name: &str) -> Result<flotilla_resources::ResourceObject<ManifestRoot>, String> {
        self.backend.using::<ManifestRoot>(namespace).get(name).await.map_err(|error| match error {
            ResourceError::NotFound { .. } => format!("ManifestRoot {name} is not declared here"),
            error => error.to_string(),
        })
    }

    /// Writes the status `next` makes of root `name`'s current status, retrying a concurrent write.
    async fn update_status(
        &self,
        namespace: &str,
        name: &str,
        next: impl Fn(&flotilla_resources::ResourceObject<ManifestRoot>) -> ManifestRootStatus,
    ) -> Result<(), String> {
        let roots = self.backend.using::<ManifestRoot>(namespace);
        for _ in 0..RETRIES {
            let current = self.local(namespace, name).await?;
            let status = next(&current);
            if current.status.as_ref() == Some(&status) {
                return Ok(());
            }
            match roots.update_status(name, &current.metadata.resource_version, &status).await {
                Ok(_) => return Ok(()),
                Err(ResourceError::Conflict { .. }) => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
        Err("ManifestRoot status conflict retry budget exhausted".to_string())
    }

    /// Writes the spec `edit` makes of root `name`'s current spec, retrying a concurrent write.
    async fn update_spec(&self, namespace: &str, name: &str, edit: impl Fn(&mut ManifestRootSpec)) -> Result<(), String> {
        let roots = self.backend.using::<ManifestRoot>(namespace);
        for _ in 0..RETRIES {
            let current = self.local(namespace, name).await?;
            let mut spec = current.spec.clone();
            edit(&mut spec);
            if spec == current.spec {
                return Ok(());
            }
            match roots.update(&InputMeta::from(&current.metadata), &current.metadata.resource_version, &spec).await {
                Ok(_) => return Ok(()),
                Err(ResourceError::Conflict { .. }) => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
        Err("ManifestRoot spec conflict retry budget exhausted".to_string())
    }
}

#[async_trait::async_trait]
impl ManifestRoots for OldStoreManifestRoots {
    async fn root(&self, namespace: &str, name: &str) -> Result<Option<RootView>, String> {
        match self.backend.including_replicas::<ManifestRoot>(namespace).get(name).await {
            Ok(read) => Ok(Some(Self::view(read))),
            Err(ResourceError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn roots(&self, namespace: &str) -> Result<Vec<RootView>, String> {
        let roots = self
            .backend
            .query_declared::<ManifestRoot>(namespace, &StoreQuery { include_replicas: true, ..Default::default() })
            .await
            .map_err(|error| error.to_string())?;
        Ok(roots.items.into_iter().map(Self::view).collect())
    }

    async fn declare(&self, namespace: &str, name: &str, declaration: RootDeclaration) -> Result<RootView, String> {
        if name.contains('.') {
            return Err(format!("ManifestRoot {name}: a declared name must not contain '.'"));
        }
        let roots = self.backend.using::<ManifestRoot>(namespace);
        let current = match roots.get(name).await {
            Ok(root) => Some(root),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(format!("read ManifestRoot: {error}")),
        };
        match current {
            Some(root) => {
                // A project's ops binding follows its member's: its path and source move with it.
                if !declaration.ops {
                    if root.spec.host != declaration.host {
                        return Err(format!("ManifestRoot {name} belongs to another host"));
                    }
                    if root.spec.path != declaration.path || root.spec.source != declaration.source {
                        return Err(format!("ManifestRoot {name} has a conflicting declaration"));
                    }
                }
                self.update_spec(namespace, name, |spec| {
                    spec.binding.clone_from(&declaration.binding);
                    spec.host.clone_from(&declaration.host);
                    spec.path.clone_from(&declaration.path);
                    spec.source.clone_from(&declaration.source);
                })
                .await?;
            }
            None => {
                let spec = ManifestRootSpec {
                    binding: declaration.binding.clone(),
                    host: declaration.host.clone(),
                    path: declaration.path.clone(),
                    source: declaration.source.clone(),
                    suspended: Default::default(),
                    resolutions: Default::default(),
                };
                let labels = if declaration.ops {
                    BTreeMap::from([(MANAGED_BY_LABEL.to_string(), OPS_MANAGER.to_string())])
                } else {
                    BTreeMap::new()
                };
                roots
                    .create(&InputMeta::builder().name(name.to_string()).labels(labels).build(), &spec)
                    .await
                    .map_err(|error| format!("materialize ManifestRoot: {error}"))?;
            }
        }
        let root = self.local(namespace, name).await?;
        Ok(RootView { name: name.to_owned(), local: true, spec: root.spec, status: root.status.unwrap_or_default() })
    }

    async fn retire(&self, namespace: &str, name: &str) -> Result<(), String> {
        match self.backend.using::<ManifestRoot>(namespace).delete(name).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn record_pass(&self, namespace: &str, name: &str, pass: PassReport) -> Result<(), String> {
        self.update_status(namespace, name, |current| {
            let mut status = pass_status(current.status.as_ref(), &pass, name);
            if let Some(current_status) = &current.status {
                for (key, state) in &mut status.documents {
                    if let Some(current_state) = current_status.documents.get(key) {
                        // A concurrent pass may have claimed the current token after this pass
                        // read status. Preserve that claim unless this pass has its own terminal
                        // result for the token.
                        let current_token = current.spec.resolutions.get(key).map(|resolution| resolution.token.as_str());
                        if current_token.is_some()
                            && current_state.resolved_token.as_deref() == current_token
                            && (state.resolved_token != current_state.resolved_token
                                || state.resolution_outcome == Some(ResolutionOutcome::Started)
                                || state.resolution_outcome.is_none())
                        {
                            state.resolved_token.clone_from(&current_state.resolved_token);
                            state.resolution_outcome.clone_from(&current_state.resolution_outcome);
                        }
                    }
                }
            }
            status
        })
        .await
    }

    async fn record_source(&self, namespace: &str, name: &str, revision: Option<String>, error: Option<&str>) -> Result<(), String> {
        self.update_status(namespace, name, |current| {
            source_status(current.status.clone().unwrap_or_default(), revision.clone(), error, name)
        })
        .await
    }

    async fn claim_resolution(&self, namespace: &str, name: &str, key: &DocumentKey, token: &str) -> Result<bool, String> {
        let roots = self.backend.using::<ManifestRoot>(namespace);
        for _ in 0..RETRIES {
            let root = self.local(namespace, name).await?;
            if root.spec.resolutions.get(key).map(|resolution| resolution.token.as_str()) != Some(token) {
                return Ok(false);
            }
            let mut status = root.status.unwrap_or_default();
            if status.documents.get(key).and_then(|state| state.resolved_token.as_deref()) == Some(token) {
                return Ok(false);
            }
            let mut state = status.documents.get(key).cloned().unwrap_or_else(|| DocumentState {
                phase: DocumentPhase::Refused,
                reason: None,
                live_hash: None,
                desired_hash: None,
                baseline_hash: None,
                observed_at: chrono::Utc::now(),
                resolved_token: None,
                resolution_outcome: None,
            });
            state.resolved_token = Some(token.to_string());
            state.resolution_outcome = Some(ResolutionOutcome::Started);
            status.documents.insert(key.clone(), state);
            match roots.update_status(name, &root.metadata.resource_version, &status).await {
                Ok(_) => return Ok(true),
                Err(ResourceError::Conflict { .. }) => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
        Err("ManifestRoot resolution claim conflict retry budget exhausted".to_string())
    }

    async fn request_resolution(
        &self,
        namespace: &str,
        name: &str,
        key: &DocumentKey,
        action: ResolutionAction,
        requested_by: &str,
    ) -> Result<String, String> {
        let token = uuid::Uuid::new_v4().to_string();
        let requested_by = if requested_by.is_empty() { "unknown".to_string() } else { requested_by.to_string() };
        self.update_spec(namespace, name, |spec| {
            spec.resolutions.insert(key.clone(), Resolution { action, token: token.clone(), requested_by: requested_by.clone() });
        })
        .await?;
        Ok(token)
    }

    async fn set_suspended(&self, namespace: &str, name: &str, key: &DocumentKey, suspended: bool) -> Result<(), String> {
        self.update_spec(namespace, name, |spec| {
            if suspended {
                spec.suspended.insert(key.clone());
            } else {
                spec.suspended.remove(key);
            }
        })
        .await
    }
}

/// The ManifestRoots the daemon uses: the old store's, until composition installs the engine's
/// behind `[primitives]`. Installed once, before the manifest reconciler starts.
pub struct SwitchedManifestRoots {
    old: OldStoreManifestRoots,
    engine: std::sync::OnceLock<std::sync::Arc<dyn ManifestRoots>>,
}

impl SwitchedManifestRoots {
    pub fn new(backend: ResourceBackend) -> Self {
        Self { old: OldStoreManifestRoots::new(backend), engine: std::sync::OnceLock::new() }
    }

    /// Installs the engine's roots; refused if some are installed already.
    pub fn install(&self, engine: std::sync::Arc<dyn ManifestRoots>) -> Result<(), String> {
        self.engine.set(engine).map_err(|_| "manifest roots are already installed".to_owned())
    }

    fn current(&self) -> &dyn ManifestRoots {
        match self.engine.get() {
            Some(engine) => engine.as_ref(),
            None => &self.old,
        }
    }
}

#[async_trait::async_trait]
impl ManifestRoots for SwitchedManifestRoots {
    async fn root(&self, namespace: &str, name: &str) -> Result<Option<RootView>, String> {
        self.current().root(namespace, name).await
    }
    async fn roots(&self, namespace: &str) -> Result<Vec<RootView>, String> {
        self.current().roots(namespace).await
    }
    async fn declare(&self, namespace: &str, name: &str, declaration: RootDeclaration) -> Result<RootView, String> {
        self.current().declare(namespace, name, declaration).await
    }
    async fn retire(&self, namespace: &str, name: &str) -> Result<(), String> {
        self.current().retire(namespace, name).await
    }
    async fn record_pass(&self, namespace: &str, name: &str, pass: PassReport) -> Result<(), String> {
        self.current().record_pass(namespace, name, pass).await
    }
    async fn record_source(&self, namespace: &str, name: &str, revision: Option<String>, error: Option<&str>) -> Result<(), String> {
        self.current().record_source(namespace, name, revision, error).await
    }
    async fn claim_resolution(&self, namespace: &str, name: &str, key: &DocumentKey, token: &str) -> Result<bool, String> {
        self.current().claim_resolution(namespace, name, key, token).await
    }
    async fn request_resolution(
        &self,
        namespace: &str,
        name: &str,
        key: &DocumentKey,
        action: ResolutionAction,
        requested_by: &str,
    ) -> Result<String, String> {
        self.current().request_resolution(namespace, name, key, action, requested_by).await
    }
    async fn set_suspended(&self, namespace: &str, name: &str, key: &DocumentKey, suspended: bool) -> Result<(), String> {
        self.current().set_suspended(namespace, name, key, suspended).await
    }
}
