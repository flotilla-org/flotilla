//! Additive, ownership-aware application of resource manifest directories.
//!
//! This reconciler intentionally does not prune objects whose source documents
//! disappear. Deletion and adoption are separate, explicit lifecycle acts.

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use flotilla_core::{charter_store::read_charter_source, vcs::Vcs};
use flotilla_resources::{
    apply_manifest_resource_document, get_resource_kind, resource_document_spec_hash, validate_resource_document, CharterSource,
    ControllerRetry, DocumentKey, DocumentPhase, DocumentState, EventRecorder, EventRegarding, InputMeta, LeafMaker, ManifestRoot,
    ManifestRootSpec, ManifestRootStatus, ObjectEvent, ResolutionAction, ResolutionOutcome, ResourceBackend, ResourceError, RetryCeiling,
    StallEvidenceSource, StallRung, StalledCondition, MANAGED_BY_LABEL,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

pub const MANIFEST_MANAGED_BY_VALUE: &str = "manifest";
pub const MANIFEST_SOURCE_ANNOTATION: &str = "flotilla.work/manifest-source";
pub const MANIFEST_PATH_ANNOTATION: &str = "flotilla.work/manifest-path";
pub const MANIFEST_REVISION_ANNOTATION: &str = "flotilla.work/manifest-revision";
pub const MANIFEST_RECONCILER_ROOT_ANNOTATION: &str = "flotilla.work/manifest-reconciler-root";
pub const LAST_APPLIED_HASH_ANNOTATION: &str = "flotilla.work/last-applied-hash";
pub const MANIFEST_BASELINE_HASH_ANNOTATION: &str = "flotilla.work/manifest-baseline-hash";
// ADR 0047: one-roll read/cleanup compatibility for metadata written before
// ManifestRoot. Remove this list after the next fleet roll.
const LEGACY_STATE_ANNOTATIONS: [&str; 5] = [
    "flotilla.work/manifest-refusal",
    "flotilla.work/manifest-live-hash",
    "flotilla.work/manifest-desired-hash",
    "flotilla.work/manifest-suspend",
    "flotilla.work/manifest-resolution",
];

pub(crate) type LoadedManifestFile = (PathBuf, Result<Vec<Value>, String>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ObjectIdentity {
    kind: String,
    namespace: String,
    name: String,
}

struct ManifestDocumentContext<'a> {
    path: &'a Path,
    revision: &'a str,
    key: &'a DocumentKey,
    identity: &'a ObjectIdentity,
}

impl fmt::Display for ObjectIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}/{}", self.kind, self.namespace, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestDocumentError {
    pub path: PathBuf,
    pub reason: String,
}

impl fmt::Display for ManifestDocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path.display(), self.reason)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ManifestPassReport {
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub drifted: usize,
    pub unmanaged: usize,
    pub errors: Vec<ManifestDocumentError>,
}

pub struct ResourceManifestReconciler {
    backend: ResourceBackend,
    default_namespace: String,
    root: PathBuf,
    source: String,
    reconciler_root: String,
    binding: Option<CharterSource>,
    fixed_revision: Option<String>,
    vcs: Option<Arc<dyn Vcs>>,
    warned_unmanaged: HashSet<ObjectIdentity>,
    warned_drift: HashSet<(ObjectIdentity, String, Option<String>)>,
    events: EventRecorder,
}

pub(crate) fn manifest_root_name(host: &str, path: &Path, source: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(host.as_bytes());
    digest.update([0]);
    digest.update(source.as_bytes());
    digest.update([0]);
    digest.update(path.as_os_str().as_encoded_bytes());
    let digest = digest.finalize();
    format!("manifest-{}", digest[..8].iter().map(|byte| format!("{byte:02x}")).collect::<String>())
}

pub(crate) async fn materialize_manifest_root(
    backend: &ResourceBackend,
    namespace: &str,
    path: &Path,
    source: &str,
    host: &str,
) -> Result<flotilla_resources::ResourceObject<ManifestRoot>, String> {
    let roots = backend.using::<ManifestRoot>(namespace);
    let name = manifest_root_name(host, path, source);
    match roots.get(&name).await {
        Ok(root) => {
            if root.spec.host != host {
                return Err(format!("ManifestRoot {name} belongs to another host"));
            }
            if root.spec.path != path.to_string_lossy() || root.spec.source != source {
                return Err(format!("ManifestRoot {name} has a conflicting declaration"));
            }
            Ok(root)
        }
        Err(ResourceError::NotFound { .. }) => {
            let spec = ManifestRootSpec {
                binding: None,
                host: host.to_string(),
                path: path.to_string_lossy().into_owned(),
                source: source.to_string(),
                suspended: Default::default(),
                resolutions: Default::default(),
            };
            roots
                .create(&InputMeta::builder().name(name).build(), &spec)
                .await
                .map_err(|error| format!("materialize ManifestRoot: {error}"))
        }
        Err(error) => Err(format!("read ManifestRoot: {error}")),
    }
}

pub(crate) async fn materialize_bound_manifest_root(
    backend: &ResourceBackend,
    namespace: &str,
    config: &flotilla_core::config::ResourceManifestsConfig,
) -> Result<(), String> {
    if let Some(binding) = &config.binding {
        binding.validate()?;
    }
    let root = materialize_manifest_root(backend, namespace, &config.dir, &config.source, &config.reconciler_root).await?;
    if root.spec.binding != config.binding {
        let mut spec = root.spec.clone();
        spec.binding.clone_from(&config.binding);
        backend
            .using::<ManifestRoot>(namespace)
            .update(&InputMeta::from(&root.metadata), &root.metadata.resource_version, &spec)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

impl ResourceManifestReconciler {
    pub(crate) fn new(backend: ResourceBackend, default_namespace: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            events: EventRecorder::new(backend.clone()),
            backend,
            default_namespace: default_namespace.into(),
            root: root.into(),
            source: "local".to_string(),
            reconciler_root: "local".to_string(),
            binding: None,
            fixed_revision: Some("unversioned".to_string()),
            vcs: None,
            warned_unmanaged: HashSet::new(),
            warned_drift: HashSet::new(),
        }
    }

    fn root_name(&self) -> String {
        manifest_root_name(&self.reconciler_root, &self.root, &self.source)
    }

    pub fn with_declared_source(mut self, source: impl Into<String>, reconciler_root: impl Into<String>) -> Self {
        self.source = source.into();
        self.reconciler_root = reconciler_root.into();
        self.fixed_revision = None;
        self
    }

    pub fn with_binding(mut self, binding: Option<CharterSource>) -> Self {
        self.binding = binding;
        self
    }

    pub fn with_vcs(mut self, vcs: Arc<dyn Vcs>) -> Self {
        self.vcs = Some(vcs);
        self
    }

    #[cfg(test)]
    fn with_revision(mut self, revision: impl Into<String>) -> Self {
        self.fixed_revision = Some(revision.into());
        self
    }

    pub async fn run(mut self, interval: Duration) -> Result<(), ResourceError> {
        let mut last_root_error = None;
        loop {
            // A deleted declaration is restored by the daemon materializer on
            // the next supervised start, not by the status-only reconciler.
            self.backend.using::<ManifestRoot>(&self.default_namespace).get(&self.root_name()).await?;
            let report = match self.reconcile_once().await {
                Ok(report) => {
                    if let Some(previous) = last_root_error.take() {
                        info!(root = %self.root.display(), %previous, "manifest directory recovered");
                    }
                    report
                }
                Err(error) => {
                    if last_root_error.as_deref() != Some(error.as_str()) {
                        warn!(root = %self.root.display(), %error, "manifest directory unavailable; reconciliation will retry");
                        last_root_error = Some(error);
                    }
                    tokio::time::sleep(interval).await;
                    continue;
                }
            };
            for error in &report.errors {
                warn!(path = %error.path.display(), reason = %error.reason, "manifest document rejected");
            }
            if report.created > 0 || report.updated > 0 {
                info!(
                    root = %self.root.display(),
                    created = report.created,
                    updated = report.updated,
                    unchanged = report.unchanged,
                    drifted = report.drifted,
                    unmanaged = report.unmanaged,
                    errors = report.errors.len(),
                    "manifest reconciliation pass applied desired state"
                );
            }
            tokio::time::sleep(interval).await;
        }
    }

    #[cfg(test)]
    async fn reconcile_once_for_test(&mut self) -> Result<ManifestPassReport, String> {
        materialize_bound_manifest_root(
            &self.backend,
            &self.default_namespace,
            &flotilla_core::config::ResourceManifestsConfig {
                binding: self.binding.clone(),
                dir: self.root.clone(),
                source: self.source.clone(),
                reconciler_root: self.reconciler_root.clone(),
            },
        )
        .await?;
        self.reconcile_once().await
    }

    async fn publish_status(&self, status: ManifestRootStatus) -> Result<(), String> {
        let roots = self.backend.using::<ManifestRoot>(&self.default_namespace);
        let root_name = self.root_name();
        for _ in 0..3 {
            let current = roots.get(&root_name).await.map_err(|error| error.to_string())?;
            if current.status.as_ref() == Some(&status) {
                return Ok(());
            }
            let mut status = status.clone();
            if let Some(current_status) = &current.status {
                for (key, state) in &mut status.documents {
                    if let Some(current_state) = current_status.documents.get(key) {
                        // A concurrent pass may have claimed the current token
                        // after this pass read status. Preserve that claim unless
                        // this pass has its own terminal result for the token.
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
            match roots.update_status(&root_name, &current.metadata.resource_version, &status).await {
                Ok(_) => return Ok(()),
                Err(ResourceError::Conflict { .. }) => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
        Err("ManifestRoot status conflict retry budget exhausted".to_string())
    }

    async fn claim_resolution(&self, key: &DocumentKey, token: &str, previous: Option<&DocumentState>) -> Result<bool, String> {
        let roots = self.backend.using::<ManifestRoot>(&self.default_namespace);
        let root_name = self.root_name();
        for _ in 0..3 {
            let root = roots.get(&root_name).await.map_err(|error| error.to_string())?;
            if root.spec.resolutions.get(key).map(|resolution| resolution.token.as_str()) != Some(token) {
                return Ok(false);
            }
            let mut status = root.status.unwrap_or_default();
            if status.documents.get(key).and_then(|state| state.resolved_token.as_deref()) == Some(token) {
                return Ok(false);
            }
            let mut state = previous.cloned().unwrap_or_else(|| document_state(DocumentPhase::Refused, None, None, None, None, None));
            state.resolved_token = Some(token.to_string());
            state.resolution_outcome = Some(ResolutionOutcome::Started);
            status.documents.insert(key.clone(), state);
            match roots.update_status(&root_name, &root.metadata.resource_version, &status).await {
                Ok(_) => return Ok(true),
                Err(ResourceError::Conflict { .. }) => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
        Err("ManifestRoot resolution claim conflict retry budget exhausted".to_string())
    }

    pub async fn reconcile_once(&mut self) -> Result<ManifestPassReport, String> {
        let lock = flotilla_core::charter_store::authoring_lock(&self.default_namespace);
        let _guard = lock.lock().await;
        let root_resource = self
            .backend
            .using::<ManifestRoot>(&self.default_namespace)
            .get(&self.root_name())
            .await
            .map_err(|error| format!("read ManifestRoot: {error}"))?;
        let inputs = self.load_inputs().await;
        let (revision, mut files) = match inputs {
            Ok(inputs) => inputs,
            Err(error) => {
                self.publish_source_failure(&root_resource, &error).await?;
                return Err(error);
            }
        };
        let reader =
            crate::charter_delegation::BoundCharterReader { cache: &self.root.with_extension("charter-cache"), vcs: self.vcs.as_deref() };
        let registered = match flotilla_core::charter_store::source_read(crate::charter_delegation::expand_registered_charters(
            &files,
            &revision,
            &self.default_namespace,
            &self.backend,
            &reader,
        ))
        .await
        {
            Ok(Some(expanded)) => {
                files = expanded;
                true
            }
            Ok(None) => false,
            Err(error) => {
                self.publish_source_failure(&root_resource, &error).await?;
                return Err(error);
            }
        };
        if registered {
            for (path, parsed) in &files {
                for document in parsed.as_ref().map_err(Clone::clone)? {
                    if document.get("kind").and_then(Value::as_str) != Some("Project") {
                        continue;
                    }
                    let identity = document_identity(document, &self.default_namespace)?;
                    match get_resource_kind(&self.backend, &identity.namespace, &identity.kind, &identity.name).await {
                        Ok(existing) => {
                            let annotations = string_map(&existing.value, "annotations")?;
                            if let Some(other) =
                                annotations.get(MANIFEST_RECONCILER_ROOT_ANNOTATION).filter(|other| **other != self.root_name())
                            {
                                let error = format!(
                                    "Project `{identity}` claimed by two sources: ManifestRoot/{other} and ManifestRoot/{} ({})",
                                    self.root_name(),
                                    path.display()
                                );
                                self.publish_source_failure(&root_resource, &error).await?;
                                return Err(error);
                            }
                        }
                        Err(ResourceError::NotFound { .. }) => {}
                        Err(error) => return Err(error.to_string()),
                    }
                }
            }
        }
        // A bound revision is accepted as a whole. Validate all documents before
        // any resource writes, including typed specs and duplicate identities.
        if self.binding.is_some() || registered {
            let mut identities = HashSet::new();
            let validation = files.iter().try_for_each(|(path, parsed)| {
                for document in parsed.as_ref().map_err(|error| format!("{}: {error}", path.display()))? {
                    validate_resource_document(document).map_err(|error| format!("{}: {error}", path.display()))?;
                    let identity = document_identity(document, &self.default_namespace)?;
                    if !identities.insert(identity.clone()) {
                        return Err(format!("duplicate charter document {identity}"));
                    }
                }
                Ok::<_, String>(())
            });
            if let Err(error) = validation {
                self.publish_source_failure(&root_resource, &error).await?;
                return Err(error);
            }
        }
        let mut report = ManifestPassReport::default();
        let mut status = root_resource.status.clone().unwrap_or_default();
        let previous = status.documents.clone();
        let mut documents = BTreeMap::new();
        for (path, parsed) in files {
            let relative = path.strip_prefix(&self.root).unwrap_or(&path).to_path_buf();
            let parsed = match parsed {
                Ok(parsed) => parsed,
                Err(reason) => {
                    let key = DocumentKey {
                        path: relative.to_string_lossy().into_owned(),
                        kind: String::new(),
                        namespace: String::new(),
                        name: String::new(),
                    };
                    let state = document_state(DocumentPhase::Refused, Some(reason.clone()), None, None, None, previous.get(&key));
                    documents.insert(key, state);
                    report.errors.push(ManifestDocumentError { path: relative, reason });
                    continue;
                }
            };
            for (index, document) in parsed.into_iter().enumerate() {
                let identity = document_identity(&document, &self.default_namespace);
                let key = match &identity {
                    Ok(identity) => DocumentKey {
                        path: relative.to_string_lossy().into_owned(),
                        kind: identity.kind.clone(),
                        namespace: identity.namespace.clone(),
                        name: identity.name.clone(),
                    },
                    Err(_) => DocumentKey {
                        path: relative.to_string_lossy().into_owned(),
                        kind: String::new(),
                        namespace: String::new(),
                        name: format!("#{}", index + 1),
                    },
                };
                let document_revision = document
                    .pointer("/metadata/annotations/flotilla.work~1charter-revision")
                    .and_then(Value::as_str)
                    .unwrap_or(&revision)
                    .to_string();
                let result = match identity {
                    Ok(identity) => {
                        self.reconcile_document(
                            ManifestDocumentContext { path: &relative, revision: &document_revision, key: &key, identity: &identity },
                            document,
                            &root_resource.spec,
                            previous.get(&key),
                            &mut report,
                        )
                        .await
                    }
                    Err(reason) => Err(reason),
                };
                match result {
                    Ok(state) => {
                        documents.insert(key, state);
                    }
                    Err(reason) => {
                        let state = document_state(DocumentPhase::Refused, Some(reason.clone()), None, None, None, previous.get(&key));
                        documents.insert(key, state);
                        let path =
                            if index == 0 { relative.clone() } else { PathBuf::from(format!("{}#{}", relative.display(), index + 1)) };
                        report.errors.push(ManifestDocumentError { path, reason });
                    }
                }
            }
        }
        status.documents = documents;
        status.source_error = None;
        if report.errors.is_empty() && status.documents.values().all(|state| state.phase == DocumentPhase::Applied) {
            status.applied_revision = Some(revision);
        }
        let refusals = status
            .documents
            .iter()
            .filter(|(_, state)| state.phase == DocumentPhase::Refused)
            .map(|(key, state)| format!("{}: {}", key.path, state.reason.as_deref().unwrap_or("manifest document refused")))
            .collect::<Vec<_>>();
        status.stalled = (!refusals.is_empty()).then(|| {
            let evidence = refusals.join("; ");
            let now = chrono::Utc::now();
            let retry = ControllerRetry::terminal(None, now, evidence.clone());
            StalledCondition {
                leaves: Vec::new(),
                maker: Some(LeafMaker::Controller {
                    resource_kind: "ManifestRoot".into(),
                    name: Some(self.root_name()),
                    retry,
                    ceiling: RetryCeiling::default(),
                }),
                evidence,
                source: StallEvidenceSource::LeafEngine,
                cause: None,
                began_at: now,
                rung: StallRung::Operator,
                supervisor: None,
                supervision_message: None,
                supervision_index: None,
                supervision_exhausted: false,
                reason: None,
                proposed_disposition: None,
                nudge_history: Vec::new(),
            }
        });
        if let (Some(previous_stall), Some(stall)) =
            (root_resource.status.as_ref().and_then(|status| status.stalled.as_ref()), status.stalled.as_mut())
        {
            if previous_stall.evidence == stall.evidence {
                *stall = previous_stall.clone();
            }
        }
        self.publish_status(status).await?;
        Ok(report)
    }

    async fn load_inputs(&self) -> Result<(String, Vec<LoadedManifestFile>), String> {
        if let Some(binding) = &self.binding {
            let snapshot = read_charter_source(binding, &self.root, self.vcs.as_deref()).await?;
            let files = snapshot
                .files
                .into_iter()
                .filter(|(path, _)| {
                    Path::new(path)
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| matches!(extension, "json" | "yaml" | "yml"))
                })
                .map(|(path, contents)| {
                    let path = self.root.join(path);
                    let parsed = parse_document_contents(&path, &contents);
                    (path, parsed)
                })
                .collect();
            return Ok((snapshot.revision, files));
        }
        // ADR 0047: previous-generation clean-checkout configuration remains
        // readable for one roll. Remove this fallback after that fleet roll has
        // migrated all declared manifest sources to explicit bindings.
        let revision = match &self.fixed_revision {
            Some(revision) => revision.clone(),
            None => self.vcs.as_ref().ok_or("manifest VCS provider unavailable")?.clean_revision().await?,
        };
        let root = self.root.clone();
        let files = tokio::task::spawn_blocking(move || load_manifest_files(&root))
            .await
            .map_err(|error| format!("manifest file loading task failed: {error}"))??;
        Ok((revision, files))
    }

    async fn publish_source_failure(&self, root: &flotilla_resources::ResourceObject<ManifestRoot>, error: &str) -> Result<(), String> {
        let status =
            flotilla_core::charter_store::source_status(root.status.clone().unwrap_or_default(), None, Some(error), &self.root_name());
        self.publish_status(status).await
    }

    async fn reconcile_document(
        &mut self,
        context: ManifestDocumentContext<'_>,
        mut document: Value,
        spec: &ManifestRootSpec,
        previous: Option<&DocumentState>,
        report: &mut ManifestPassReport,
    ) -> Result<DocumentState, String> {
        let ManifestDocumentContext { path, revision, key, identity } = context;
        let desired_hash = resource_document_spec_hash(&document).map_err(|error| format!("{identity}: {error}"))?;
        stamp_manifest_metadata(&mut document, &self.source, path, revision, &self.root_name(), &desired_hash)?;
        let existing = match get_resource_kind(&self.backend, &identity.namespace, &identity.kind, &identity.name).await {
            Ok(existing) => Some(existing.value),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(format!("{identity}: {error}")),
        };
        let resolution = spec
            .resolutions
            .get(key)
            .filter(|resolution| previous.and_then(|state| state.resolved_token.as_deref()) != Some(resolution.token.as_str()));
        if spec.suspended.contains(key) {
            // Suspension is persistent operator intent. A resolution remains
            // pending until the operator removes this key from suspended.
            let live_hash =
                existing.as_ref().map(|object| resource_document_spec_hash(object).map_err(|error| error.to_string())).transpose()?;
            let baseline = existing.as_ref().map(|object| string_map(object, "annotations")).transpose()?.and_then(|annotations| {
                annotations.get(MANIFEST_BASELINE_HASH_ANNOTATION).or_else(|| annotations.get(LAST_APPLIED_HASH_ANNOTATION)).cloned()
            });
            report.unchanged += 1;
            return Ok(document_state(DocumentPhase::Suspended, None, live_hash, Some(desired_hash), baseline, previous));
        }
        let Some(existing) = existing else {
            self.apply_manifest_document(document).await.map_err(|error| format!("{identity}: {error}"))?;
            report.created += 1;
            let actual_hash = self.stored_spec_hash(identity).await?;
            return Ok(document_state(
                DocumentPhase::Applied,
                None,
                Some(actual_hash.clone()),
                Some(desired_hash),
                Some(actual_hash),
                previous,
            ));
        };
        let annotations = string_map(&existing, "annotations")?;
        let labels = string_map(&existing, "labels")?;
        if identity.kind == "Project"
            && document.pointer("/spec/charter").is_some_and(|pointer| !pointer.is_null())
            && existing.pointer("/spec/charter").is_none_or(Value::is_null)
            && !annotations.contains_key(MANIFEST_RECONCILER_ROOT_ANNOTATION)
            && (annotations.contains_key(flotilla_core::project_declaration::BOOTSTRAP_REPOSITORY_ANNOTATION)
                || labels.get(MANAGED_BY_LABEL).map(String::as_str) == Some("whole-repository-project"))
        {
            // The explicit registration transfers a legacy-generated Project.
            // Unrelated unmanaged objects retain the ordinary adoption refusal.
            preserve_external_metadata(&mut document, &existing)?;
            clear_manifest_state(&mut document)?;
            self.apply_manifest_document(document).await?;
            report.updated += 1;
            let hash = self.stored_spec_hash(identity).await?;
            return Ok(document_state(DocumentPhase::Applied, None, Some(hash.clone()), Some(desired_hash), Some(hash), previous));
        }
        let live_hash = resource_document_spec_hash(&existing).map_err(|error| format!("{identity}: {error}"))?;
        let baseline =
            annotations.get(MANIFEST_BASELINE_HASH_ANNOTATION).or_else(|| annotations.get(LAST_APPLIED_HASH_ANNOTATION)).cloned();
        if let Some(resolution) = resolution.filter(|resolution| {
            resolution.action != ResolutionAction::Sync
                || labels.get(MANAGED_BY_LABEL).map(String::as_str) == Some(MANIFEST_MANAGED_BY_VALUE)
        }) {
            if self.claim_resolution(key, &resolution.token, previous).await? {
                let action_result: Result<ResolutionOutcome, String> = async {
                    match resolution.action {
                        ResolutionAction::Sync => {
                            preserve_external_metadata(&mut document, &existing)?;
                            clear_manifest_state(&mut document)?;
                            self.apply_manifest_document(document).await.map_err(|error| format!("{identity}: {error}"))?;
                            Ok(ResolutionOutcome::Synced)
                        }
                        ResolutionAction::Adopt => {
                            // Re-read before changing the source file and verify the
                            // live spec again afterward; adoption crosses an await.
                            let refreshed = get_resource_kind(&self.backend, &identity.namespace, &identity.kind, &identity.name)
                                .await
                                .map_err(|error| format!("{identity}: refresh live spec for adoption: {error}"))?
                                .value;
                            let fresh_hash = resource_document_spec_hash(&refreshed).map_err(|error| error.to_string())?;
                            let root = self
                                .backend
                                .using::<ManifestRoot>(&self.default_namespace)
                                .get(&self.root_name())
                                .await
                                .map_err(|error| error.to_string())?;
                            if root.spec.resolutions.get(key).map(|request| request.token.as_str()) != Some(resolution.token.as_str()) {
                                return Err(format!("{identity}: adopt resolution changed while reconciling"));
                            }
                            self.adopt_live_spec(path, identity, &refreshed).await?;
                            // The source may already be replaced if verification below fails.
                            // The claimed token then records Failed; an operator must inspect
                            // the file and issue a new token to retry.
                            let current = get_resource_kind(&self.backend, &identity.namespace, &identity.kind, &identity.name)
                                .await
                                .map_err(|error| format!("{identity}: verify live spec after adoption: {error}"))?
                                .value;
                            let current_hash = resource_document_spec_hash(&current).map_err(|error| error.to_string())?;
                            if current_hash != fresh_hash {
                                return Err(format!("{identity}: live spec changed while adopting"));
                            }
                            let mut adopted = refreshed;
                            stamp_manifest_metadata(&mut adopted, &self.source, path, revision, &self.root_name(), &fresh_hash)?;
                            clear_manifest_state(&mut adopted)?;
                            self.apply_manifest_document(adopted).await.map_err(|error| format!("{identity}: {error}"))?;
                            Ok(ResolutionOutcome::Adopted)
                        }
                    }
                }
                .await;
                let (phase, reason, outcome, actual_hash) = match action_result {
                    Ok(outcome) => {
                        report.updated += 1;
                        let applied = get_resource_kind(&self.backend, &identity.namespace, &identity.kind, &identity.name)
                            .await
                            .map_err(|error| error.to_string())?;
                        let actual_hash = resource_document_spec_hash(&applied.value).map_err(|error| error.to_string())?;
                        (DocumentPhase::Applied, None, outcome, Some(actual_hash))
                    }
                    Err(error) => {
                        report.errors.push(ManifestDocumentError { path: path.to_path_buf(), reason: error.clone() });
                        (DocumentPhase::Refused, Some(error.clone()), ResolutionOutcome::Failed(error), Some(live_hash))
                    }
                };
                let effective_desired = if resolution.action == ResolutionAction::Adopt && phase == DocumentPhase::Applied {
                    actual_hash.clone()
                } else {
                    Some(desired_hash)
                };
                let mut state = document_state(phase, reason, actual_hash.clone(), effective_desired, actual_hash, previous);
                state.resolved_token = Some(resolution.token.clone());
                state.resolution_outcome = Some(outcome);
                state.observed_at = chrono::Utc::now();
                self.clear_warnings(identity);
                return Ok(state);
            }
        }
        if live_hash == desired_hash {
            let settled = baseline.as_deref() == Some(live_hash.as_str())
                && labels.get(MANAGED_BY_LABEL).map(String::as_str) == Some(MANIFEST_MANAGED_BY_VALUE)
                && annotations.get(MANIFEST_SOURCE_ANNOTATION).map(String::as_str) == Some(self.source.as_str())
                && annotations.get(MANIFEST_PATH_ANNOTATION).map(String::as_str) == Some(path.to_string_lossy().as_ref())
                && annotations.contains_key(MANIFEST_REVISION_ANNOTATION)
                && [
                    crate::charter_delegation::CHARTER_SOURCE,
                    crate::charter_delegation::CHARTER_REVISION,
                    crate::charter_delegation::CHARTER_SCOPE,
                ]
                .iter()
                .all(|key| {
                    document["metadata"]["annotations"].get(*key).and_then(Value::as_str) == annotations.get(*key).map(String::as_str)
                })
                && ((self.binding.is_none()
                    && document["metadata"]["annotations"].get(crate::charter_delegation::CHARTER_REVISION).is_none())
                    || annotations.get(MANIFEST_REVISION_ANNOTATION).map(String::as_str) == Some(revision))
                && annotations.get(MANIFEST_RECONCILER_ROOT_ANNOTATION).map(String::as_str) == Some(self.root_name().as_str())
                && LEGACY_STATE_ANNOTATIONS.iter().all(|key| !annotations.contains_key(*key));
            if settled {
                report.unchanged += 1;
            } else {
                preserve_external_metadata(&mut document, &existing)?;
                clear_manifest_state(&mut document)?;
                self.apply_manifest_document(document).await.map_err(|error| format!("{identity}: {error}"))?;
                report.updated += 1;
            }
            self.clear_warnings(identity);
            return Ok(document_state(
                DocumentPhase::Applied,
                None,
                Some(live_hash.clone()),
                Some(desired_hash),
                Some(live_hash),
                previous,
            ));
        }
        if labels.get(MANAGED_BY_LABEL).map(String::as_str) != Some(MANIFEST_MANAGED_BY_VALUE) {
            self.record_refusal_event(
                identity,
                "ManifestAdoptionRefused",
                format!("{}: manifest names an unmanaged object", path.display()),
            )
            .await;
            if self.warned_unmanaged.insert(identity.clone()) {
                warn!(object = %identity, source = %self.source, path = %path.display(), "manifest names an unmanaged object; refusing adoption");
            }
            report.unmanaged += 1;
            return Ok(document_state(
                DocumentPhase::Refused,
                Some("unmanaged object".into()),
                Some(live_hash),
                Some(desired_hash),
                baseline,
                previous,
            ));
        }
        self.warned_unmanaged.remove(identity);
        if baseline.as_deref() != Some(live_hash.as_str()) {
            self.record_refusal_event(
                identity,
                "ManifestOverwriteRefused",
                format!("{}: manifest-managed object has live drift", path.display()),
            )
            .await;
            if self.warned_drift.insert((identity.clone(), live_hash.clone(), baseline.clone())) {
                warn!(object = %identity, live_digest = %live_hash, "manifest-managed object has live drift; refusing overwrite");
            }
            report.drifted += 1;
            return Ok(document_state(
                DocumentPhase::Drifted,
                Some("live spec drift".into()),
                Some(live_hash),
                Some(desired_hash),
                baseline,
                previous,
            ));
        }
        if desired_hash == live_hash {
            report.unchanged += 1;
        } else {
            preserve_external_metadata(&mut document, &existing)?;
            clear_manifest_state(&mut document)?;
            self.apply_manifest_document(document).await.map_err(|error| format!("{identity}: {error}"))?;
            report.updated += 1;
        }
        self.clear_warnings(identity);
        let actual_hash = self.stored_spec_hash(identity).await?;
        Ok(document_state(DocumentPhase::Applied, None, Some(actual_hash.clone()), Some(desired_hash), Some(actual_hash), previous))
    }

    fn clear_warnings(&mut self, identity: &ObjectIdentity) {
        self.warned_unmanaged.remove(identity);
        self.warned_drift.retain(|(warned, _, _)| warned != identity);
    }

    async fn record_refusal_event(&self, identity: &ObjectIdentity, reason: &str, message: String) {
        let event = ObjectEvent {
            regarding: EventRegarding {
                api_version: "flotilla.work/v1".to_string(),
                kind: identity.kind.clone(),
                namespace: identity.namespace.clone(),
                name: identity.name.clone(),
            },
            reason: reason.to_string(),
            message,
            related_labels: BTreeMap::new(),
        };
        if let Err(error) = self.events.record(event, chrono::Utc::now()).await {
            warn!(object = %identity, %error, "failed to record manifest refusal event");
        }
    }

    async fn adopt_live_spec(&self, source: &Path, identity: &ObjectIdentity, existing: &Value) -> Result<(), String> {
        if existing.pointer("/metadata/annotations/flotilla.work~1charter-scope").is_some() {
            return Err(
                "adoption cannot write a registered charter; edit the declared inline inputs or commit to its repository branch".into()
            );
        }
        if matches!(self.binding, Some(CharterSource::Repository { .. })) {
            return Err("adoption cannot write a repository-bound charter; commit the desired change to its branch".into());
        }
        let root = match &self.binding {
            Some(CharterSource::LocalDirectory { directory }) => Path::new(directory),
            _ => &self.root,
        };
        let path = root.join(source);
        let source = source.to_path_buf();
        let identity = identity.clone();
        let task_identity = identity.clone();
        let existing = existing.clone();
        let default_namespace = self.default_namespace.clone();
        tokio::task::spawn_blocking(move || Self::adopt_live_spec_blocking(&path, &source, &task_identity, &existing, &default_namespace))
            .await
            .map_err(|error| format!("{identity}: manifest adoption task failed: {error}"))?
    }

    fn adopt_live_spec_blocking(
        path: &Path,
        source: &Path,
        identity: &ObjectIdentity,
        existing: &Value,
        default_namespace: &str,
    ) -> Result<(), String> {
        let mut documents = parse_documents(path).map_err(|error| format!("{identity}: {error}"))?;
        let document = documents
            .iter_mut()
            .find(|document| document_identity(document, default_namespace).as_ref() == Ok(identity))
            .ok_or_else(|| format!("{identity}: manifest source {} no longer contains the object", source.display()))?;
        document["spec"] = existing["spec"].clone();
        let rendered =
            if path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| extension.eq_ignore_ascii_case("json")) {
                serde_json::to_string_pretty(&documents[0]).map_err(|error| format!("serialize JSON: {error}"))? + "\n"
            } else {
                documents
                    .iter()
                    .map(|document| serde_yml::to_string(document).map_err(|error| format!("serialize YAML: {error}")))
                    .collect::<Result<Vec<_>, _>>()?
                    .join("---\n")
            };
        let parent = path.parent().ok_or_else(|| format!("{identity}: manifest source has no parent"))?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|error| format!("{identity}: create temporary manifest beside {}: {error}", source.display()))?;
        let permissions = std::fs::metadata(path)
            .map_err(|error| format!("{identity}: inspect manifest {} permissions: {error}", source.display()))?
            .permissions();
        temporary
            .as_file()
            .set_permissions(permissions)
            .map_err(|error| format!("{identity}: preserve manifest {} permissions: {error}", source.display()))?;
        temporary
            .write_all(rendered.as_bytes())
            .and_then(|()| temporary.flush())
            .map_err(|error| format!("{identity}: write temporary manifest for {}: {error}", source.display()))?;
        temporary.persist(path).map_err(|error| format!("{identity}: replace manifest {}: {}", source.display(), error.error))?;
        Ok(())
    }

    async fn stored_spec_hash(&self, identity: &ObjectIdentity) -> Result<String, String> {
        let object = get_resource_kind(&self.backend, &identity.namespace, &identity.kind, &identity.name)
            .await
            .map_err(|error| error.to_string())?;
        resource_document_spec_hash(&object.value).map_err(|error| error.to_string())
    }

    async fn apply_manifest_document(&self, document: Value) -> Result<(), String> {
        let applied =
            apply_manifest_resource_document(&self.backend, &self.default_namespace, document).await.map_err(|error| error.to_string())?;
        let persisted_hash = resource_document_spec_hash(&applied.value).map_err(|error| error.to_string())?;
        let recorded_hash = string_map(&applied.value, "annotations")?.get(LAST_APPLIED_HASH_ANNOTATION).cloned();
        if recorded_hash.as_deref() == Some(persisted_hash.as_str()) {
            return Ok(());
        }

        // Ownership enforcement can preserve protected fields, so the stored
        // spec may differ from the requested manifest. Record the hash of what
        // was actually persisted or the next pass will misclassify that
        // preservation as external drift.
        let mut persisted = applied.value;
        let metadata =
            persisted.get_mut("metadata").and_then(Value::as_object_mut).ok_or_else(|| "stored object has invalid metadata".to_string())?;
        insert_metadata_value(metadata, "annotations", MANIFEST_BASELINE_HASH_ANNOTATION, &persisted_hash)?;
        insert_metadata_value(metadata, "annotations", LAST_APPLIED_HASH_ANNOTATION, &persisted_hash)?;
        apply_manifest_resource_document(&self.backend, &self.default_namespace, persisted).await.map_err(|error| error.to_string())?;
        Ok(())
    }
}

fn document_state(
    phase: DocumentPhase,
    reason: Option<String>,
    live_hash: Option<String>,
    desired_hash: Option<String>,
    baseline_hash: Option<String>,
    previous: Option<&DocumentState>,
) -> DocumentState {
    let mut state = DocumentState {
        phase,
        reason,
        live_hash,
        desired_hash,
        baseline_hash,
        observed_at: chrono::Utc::now(),
        resolved_token: previous.and_then(|state| state.resolved_token.clone()),
        resolution_outcome: previous.and_then(|state| state.resolution_outcome.clone()),
    };
    if let Some(previous) = previous {
        if state.phase == previous.phase
            && state.reason == previous.reason
            && state.live_hash == previous.live_hash
            && state.desired_hash == previous.desired_hash
            && state.baseline_hash == previous.baseline_hash
        {
            state.observed_at = previous.observed_at;
        }
    }
    state
}

fn manifest_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    fn collect(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
        let entries = std::fs::read_dir(dir).map_err(|error| format!("read manifest directory {}: {error}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("read manifest directory entry in {}: {error}", dir.display()))?;
            let file_type = entry.file_type().map_err(|error| format!("inspect manifest path {}: {error}", entry.path().display()))?;
            if file_type.is_dir() {
                collect(&entry.path(), files)?;
            } else if file_type.is_file()
                && entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "json" | "yaml" | "yml"))
            {
                files.push(entry.path());
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    collect(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn load_manifest_files(root: &Path) -> Result<Vec<LoadedManifestFile>, String> {
    Ok(manifest_files(root)?
        .into_iter()
        .map(|path| {
            let documents = parse_documents(&path);
            (path, documents)
        })
        .collect())
}

fn parse_documents(path: &Path) -> Result<Vec<Value>, String> {
    let content = std::fs::read_to_string(path).map_err(|error| format!("read file: {error}"))?;
    parse_document_contents(path, &content)
}

pub(crate) fn parse_document_contents(path: &Path, content: &str) -> Result<Vec<Value>, String> {
    if path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| extension.eq_ignore_ascii_case("json")) {
        return serde_json::from_str(content).map(|document| vec![document]).map_err(|error| format!("parse JSON: {error}"));
    }

    let mut documents = Vec::new();
    for document in serde_yml::Deserializer::from_str(content) {
        let value = Value::deserialize(document).map_err(|error| format!("parse YAML: {error}"))?;
        if !value.is_null() {
            documents.push(value);
        }
    }
    if documents.is_empty() {
        return Err("parse YAML: file contains no resource documents".to_string());
    }
    Ok(documents)
}

fn document_identity(document: &Value, default_namespace: &str) -> Result<ObjectIdentity, String> {
    document.get("apiVersion").and_then(Value::as_str).ok_or_else(|| "missing or non-string apiVersion".to_string())?;
    let kind = document.get("kind").and_then(Value::as_str).ok_or_else(|| "missing or non-string kind".to_string())?.to_string();
    let metadata = document.get("metadata").and_then(Value::as_object).ok_or_else(|| "missing or non-object metadata".to_string())?;
    let name = metadata.get("name").and_then(Value::as_str).ok_or_else(|| "missing or non-string metadata.name".to_string())?.to_string();
    let namespace = metadata.get("namespace").and_then(Value::as_str).unwrap_or(default_namespace).to_string();
    Ok(ObjectIdentity { kind, namespace, name })
}

fn stamp_manifest_metadata(
    document: &mut Value,
    source: &str,
    path: &Path,
    revision: &str,
    reconciler_root: &str,
    hash: &str,
) -> Result<(), String> {
    let metadata =
        document.get_mut("metadata").and_then(Value::as_object_mut).ok_or_else(|| "missing or non-object metadata".to_string())?;
    insert_metadata_value(metadata, "labels", MANAGED_BY_LABEL, MANIFEST_MANAGED_BY_VALUE)?;
    insert_metadata_value(metadata, "annotations", MANIFEST_SOURCE_ANNOTATION, source)?;
    insert_metadata_value(metadata, "annotations", MANIFEST_PATH_ANNOTATION, &path.to_string_lossy())?;
    insert_metadata_value(metadata, "annotations", MANIFEST_REVISION_ANNOTATION, revision)?;
    insert_metadata_value(metadata, "annotations", MANIFEST_RECONCILER_ROOT_ANNOTATION, reconciler_root)?;
    insert_metadata_value(metadata, "annotations", MANIFEST_BASELINE_HASH_ANNOTATION, hash)?;
    insert_metadata_value(metadata, "annotations", LAST_APPLIED_HASH_ANNOTATION, hash)
}

fn clear_manifest_state(document: &mut Value) -> Result<(), String> {
    let annotations = document
        .get_mut("metadata")
        .and_then(|metadata| metadata.get_mut("annotations"))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "missing or non-object metadata.annotations".to_string())?;
    for key in LEGACY_STATE_ANNOTATIONS {
        annotations.remove(key);
    }
    Ok(())
}

fn insert_metadata_value(metadata: &mut Map<String, Value>, field: &str, key: &str, value: &str) -> Result<(), String> {
    let values = metadata.entry(field).or_insert_with(|| Value::Object(Map::new()));
    let values = values.as_object_mut().ok_or_else(|| format!("metadata.{field} must be an object"))?;
    values.insert(key.to_string(), Value::String(value.to_string()));
    Ok(())
}

fn preserve_external_metadata(document: &mut Value, existing: &Value) -> Result<(), String> {
    let desired =
        document.get_mut("metadata").and_then(Value::as_object_mut).ok_or_else(|| "missing or non-object metadata".to_string())?;
    let stored = existing
        .get("metadata")
        .and_then(Value::as_object)
        .ok_or_else(|| "stored object has missing or non-object metadata".to_string())?;
    for field in ["labels", "annotations"] {
        let desired_values = desired.entry(field).or_insert_with(|| Value::Object(Map::new()));
        let desired_values = desired_values.as_object_mut().ok_or_else(|| format!("metadata.{field} must be an object"))?;
        if let Some(stored_values) = stored.get(field).and_then(Value::as_object) {
            for (key, value) in stored_values {
                // Charter provenance is reconciler-owned, so removing a pointer
                // can clear it instead of preserving stale delegated authority.
                if field == "annotations"
                    && [
                        crate::charter_delegation::CHARTER_SOURCE,
                        crate::charter_delegation::CHARTER_REVISION,
                        crate::charter_delegation::CHARTER_SCOPE,
                    ]
                    .contains(&key.as_str())
                {
                    continue;
                }
                desired_values.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
    }
    Ok(())
}

fn string_map(document: &Value, field: &str) -> Result<BTreeMap<String, String>, String> {
    let value = document.get("metadata").and_then(|metadata| metadata.get(field)).cloned().unwrap_or_else(|| Value::Object(Map::new()));
    serde_json::from_value(value).map_err(|error| format!("stored metadata.{field} is invalid: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{process::Command, time::Duration};

    use chrono::Utc;
    use flotilla_core::config::ConfigStore;
    use flotilla_core::in_process::InProcessDaemon;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use flotilla_core::providers::discovery::factories::git::GitVcsFactory;
    use flotilla_core::providers::discovery::EnvironmentAssertion;
    use flotilla_core::providers::discovery::EnvironmentBag;
    use flotilla_core::providers::discovery::Factory;
    use flotilla_core::providers::ProcessCommandRunner;
    use flotilla_discovery_testkit::fake_discovery_with_provider_set;
    use flotilla_discovery_testkit::FakeDiscoveryProviders;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use flotilla_protocol::{HostName, NodeId};
    use flotilla_resources::{
        patch_resource_annotation, InMemoryBackend, InputMeta, PlacementPolicy, PlacementPolicySpec, Project, Resolution, ResourceBackend,
        WatchStart, WorkflowTemplate, MANIFEST_WRITER_SOURCE,
    };
    use futures::StreamExt;

    use super::*;

    const NAMESPACE: &str = "flotilla";

    // #2720: branch-head changes apply without updating a working tree. Invalid
    // heads and fetch failures retain every last-applied record and its revision.
    #[tokio::test]
    async fn bound_branch_heads_preserve_last_good_revision() {
        let repo = committed_manifest_repo();
        git(repo.path(), &["branch", "-M", "main"]);
        std::fs::create_dir(repo.path().join("charters")).expect("charter path");
        git(repo.path(), &["mv", "policy.yaml", "charters/policy.yaml"]);
        write(&repo.path().join("unrelated.yaml"), "invalid: [");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "charter subtree"]);
        let cache = tempfile::tempdir().expect("charter cache");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let source = CharterSource::Repository {
            repo: repo.path().to_string_lossy().into_owned(),
            branch: "main".into(),
            path: "./charters".into(),
        };
        let mut reconciler = versioned_reconciler(repo.path(), backend.clone()).await;
        reconciler.root = cache.path().to_path_buf();
        reconciler = reconciler.with_binding(Some(source));
        reconciler.reconcile_once_for_test().await.expect("apply first head");
        let first = test_root(&backend).await.status.expect("source status").applied_revision.expect("first revision");
        git(repo.path(), &["switch", "-c", "edit"]);
        write(&repo.path().join("charters/policy.yaml"), &manifest("versioned", "merged"));
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "change"]);
        git(repo.path(), &["switch", "main"]);
        git(repo.path(), &["merge", "--ff-only", "edit"]);
        // Leave the source checkout on a different, dirty branch. Only main's
        // committed objects can affect the bound store.
        git(repo.path(), &["switch", "edit"]);
        write(&repo.path().join("charters/policy.yaml"), &manifest("versioned", "dirty"));
        reconciler.reconcile_once().await.expect("apply merged head");
        let merged = test_root(&backend).await.status.expect("status").applied_revision.expect("merged revision");
        assert_ne!(first, merged);
        let live = backend.using::<PlacementPolicy>(NAMESPACE).get("versioned").await.expect("policy");
        assert_eq!(live.spec.pool, "merged");
        assert_eq!(live.metadata.annotations[MANIFEST_REVISION_ANNOTATION], merged);
        git(repo.path(), &["restore", "charters/policy.yaml"]);
        git(repo.path(), &["switch", "main"]);
        write(&repo.path().join("charters/policy.yaml"), &manifest("versioned", "must-not-apply"));
        write(&repo.path().join("charters/z-bad.yaml"), "invalid: [");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "bad head"]);
        assert!(reconciler.reconcile_once().await.is_err());
        let status = test_root(&backend).await.status.expect("refusal status");
        assert_eq!(status.applied_revision.as_deref(), Some(merged.as_str()));
        assert!(status.source_error.as_deref().expect("parse reason").contains("z-bad.yaml"));
        assert!(status.stalled.is_some());
        assert_eq!(backend.using::<PlacementPolicy>(NAMESPACE).get("versioned").await.expect("live policy").spec, live.spec);
        git(repo.path(), &["rm", "charters/z-bad.yaml"]);
        git(repo.path(), &["commit", "-m", "repair"]);
        reconciler.reconcile_once().await.expect("recover");
        let repaired = test_root(&backend).await.status.expect("recovered status");
        assert!(repaired.source_error.is_none());
        assert!(repaired.stalled.is_none());
        assert_ne!(repaired.applied_revision.as_deref(), Some(merged.as_str()));
        // Invalid blob bytes must not be silently replaced by string decoding.
        let mut blob = manifest("versioned", "invalid").into_bytes();
        let offset = blob.windows(7).position(|bytes| bytes == b"invalid").expect("pool value");
        blob[offset] = 0xff;
        std::fs::write(repo.path().join("charters/policy.yaml"), blob).expect("non-UTF8 charter");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "non-UTF8 charter"]);
        assert!(reconciler.reconcile_once().await.is_err());
        let invalid = test_root(&backend).await.status.expect("non-UTF8 status");
        assert_eq!(invalid.applied_revision, repaired.applied_revision);
        assert!(invalid.source_error.expect("non-UTF8 reason").contains("policy.yaml"));
        write(&repo.path().join("charters/policy.yaml"), &manifest("versioned", "must-not-apply"));
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "restore text inputs"]);
        // A Git symlink refusal must reach source status without advancing it.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("policy.yaml", repo.path().join("charters/link.yaml")).expect("symlink");
            git(repo.path(), &["add", "."]);
            git(repo.path(), &["commit", "-m", "symlink charter"]);
            assert!(reconciler.reconcile_once().await.is_err());
            let refused = test_root(&backend).await.status.expect("symlink status");
            assert_eq!(refused.applied_revision, repaired.applied_revision);
            assert!(refused.source_error.expect("symlink reason").contains("link.yaml"));
        }
        reconciler.binding = Some(CharterSource::Repository {
            repo: repo.path().to_string_lossy().into_owned(),
            branch: "missing".into(),
            path: "./charters".into(),
        });
        assert!(reconciler.reconcile_once().await.is_err());
        let failed = test_root(&backend).await.status.expect("fetch status");
        assert_eq!(failed.applied_revision, repaired.applied_revision);
        assert!(failed.source_error.expect("fetch reason").contains("missing"));
    }

    // #2720: laptop stores apply the exact files hashed into a local revision,
    // retain the previous revision on parse refusal, and recover after repair.
    #[tokio::test]
    async fn bound_local_directory_applies_and_recovers() {
        let dir = tempfile::tempdir().expect("local source");
        let cache = tempfile::tempdir().expect("unused cache");
        write(&dir.path().join("policy.yaml"), &manifest("laptop", "local"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, cache.path())
            .with_binding(Some(CharterSource::LocalDirectory { directory: dir.path().to_string_lossy().into_owned() }));
        reconciler.reconcile_once_for_test().await.expect("apply directory");
        let first = test_root(&backend).await.status.expect("status").applied_revision.expect("revision");
        assert!(first.starts_with("local:"));
        let live = backend.using::<PlacementPolicy>(NAMESPACE).get("laptop").await.expect("policy");
        assert_eq!(live.metadata.annotations[MANIFEST_REVISION_ANNOTATION], first);
        write(&dir.path().join("policy.yaml"), "broken: [");
        assert!(reconciler.reconcile_once().await.is_err());
        assert_eq!(test_root(&backend).await.status.expect("status").applied_revision.as_deref(), Some(first.as_str()));
        write(&dir.path().join("policy.yaml"), &manifest("laptop", "repaired"));
        reconciler.reconcile_once().await.expect("repair");
        let status = test_root(&backend).await.status.expect("recovery");
        assert!(status.source_error.is_none());
        assert_ne!(status.applied_revision.as_deref(), Some(first.as_str()));
    }

    // #2720: source-level failures expose their reason in fleet health even
    // before any charter document or host heartbeat has been applied.
    #[tokio::test]
    async fn bound_source_failure_reason_reaches_fleet_health() {
        let source = tempfile::tempdir().expect("source");
        let config = tempfile::tempdir().expect("config");
        write(&config.path().join("daemon.toml"), "machine_id = \"charter-health\"\n");
        let daemon = InProcessDaemon::new(
            Vec::new(),
            Arc::new(ConfigStore::with_base(config.path())),
            fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
            HostName::new("laptop"),
        )
        .await;
        let backend = daemon.resource_backend();
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, source.path())
            .with_declared_source("laptop-charter", daemon.local_host_id().expect("host").to_string())
            .with_binding(Some(CharterSource::LocalDirectory { directory: source.path().to_string_lossy().into_owned() }));
        write(&source.path().join("policy.yaml"), "bad: [");
        assert!(reconciler.reconcile_once_for_test().await.is_err());
        let health = daemon.fleet_health_internal().await.expect("fleet health");
        let host = health.hosts.iter().find(|host| host.host == HostName::new("laptop")).expect("local host");
        assert!(host.surface_states.needs_you > 0);
        assert!(host.degraded_conditions.iter().any(|reason| reason.contains("policy.yaml")), "{host:?}");
        write(&source.path().join("policy.yaml"), &manifest("healthy", "default"));
        reconciler.reconcile_once().await.expect("source recovers");
        let health = daemon.fleet_health_internal().await.expect("fleet health");
        let host = health.hosts.iter().find(|host| host.host == HostName::new("laptop")).expect("local host");
        assert!(!host.degraded_conditions.iter().any(|reason| reason.contains("ManifestRoot/")));
    }

    async fn test_root(backend: &ResourceBackend) -> flotilla_resources::ResourceObject<ManifestRoot> {
        backend.using::<ManifestRoot>(NAMESPACE).list().await.expect("list roots").items.into_iter().next().expect("root")
    }

    fn key(name: &str) -> DocumentKey {
        DocumentKey { path: "policy.yaml".into(), kind: "PlacementPolicy".into(), namespace: NAMESPACE.into(), name: name.into() }
    }

    async fn set_resolution(backend: &ResourceBackend, key: DocumentKey, action: ResolutionAction, token: &str) {
        let roots = backend.using::<ManifestRoot>(NAMESPACE);
        let root = test_root(backend).await;
        let mut spec = root.spec.clone();
        spec.resolutions.insert(key, Resolution { action, token: token.into(), requested_by: "test".into() });
        roots.update(&InputMeta::from(&root.metadata), &root.metadata.resource_version, &spec).await.expect("set resolution");
    }

    async fn set_suspended(backend: &ResourceBackend, key: DocumentKey, suspended: bool) {
        let roots = backend.using::<ManifestRoot>(NAMESPACE);
        let root = test_root(backend).await;
        let mut spec = root.spec.clone();
        if suspended {
            spec.suspended.insert(key);
        } else {
            spec.suspended.remove(&key);
        }
        roots.update(&InputMeta::from(&root.metadata), &root.metadata.resource_version, &spec).await.expect("set suspension");
    }

    fn write(path: &Path, content: &str) {
        std::fs::write(path, content).expect("write manifest");
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git").args(args).current_dir(dir).output().expect("run git fixture command");
        assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).expect("git fixture output is UTF-8").trim().to_string()
    }

    fn committed_manifest_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("git tempdir");
        git(dir.path(), &["init"]);
        git(dir.path(), &["config", "user.name", "Manifest Test"]);
        git(dir.path(), &["config", "user.email", "manifest-test@example.com"]);
        write(&dir.path().join("policy.yaml"), &manifest("versioned", "current"));
        git(dir.path(), &["add", "policy.yaml"]);
        git(dir.path(), &["commit", "-m", "manifest"]);
        dir
    }

    async fn versioned_reconciler(dir: &Path, backend: ResourceBackend) -> ResourceManifestReconciler {
        let runner = Arc::new(ProcessCommandRunner);
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("git", "/usr/bin/git"));
        let vcs = GitVcsFactory
            .probe(&bag, &ConfigStore::with_base(dir), &ExecutionEnvironmentPath::new(dir), runner)
            .await
            .expect("discover Git VCS");
        ResourceManifestReconciler::new(backend, NAMESPACE, dir).with_declared_source("project-map", "kiwi").with_vcs(vcs)
    }

    fn manifest(name: &str, pool: &str) -> String {
        format!("apiVersion: flotilla.work/v1\nkind: PlacementPolicy\nmetadata:\n  name: {name}\nspec:\n  pool: {pool}\n")
    }

    fn manifest_with_priority(name: &str, pool: &str, priority: i32) -> String {
        format!(
            "apiVersion: flotilla.work/v1\nkind: PlacementPolicy\nmetadata:\n  name: {name}\nspec:\n  pool: {pool}\n  priority: {priority}\n"
        )
    }

    fn project_manifest(workflow: &str, detailed_repository: bool) -> String {
        let repository = if detailed_repository {
            "{\"repo\":\"repo-key\",\"alias\":\"andamento\",\"roles\":[\"code\",\"ops\",\"knowledge\"]}"
        } else {
            "{\"repo\":\"repo-key\"}"
        };
        format!(
            "{{\"apiVersion\":\"flotilla.work/v1\",\"kind\":\"Project\",\"metadata\":{{\"name\":\"andamento\"}},\"spec\":{{\"display_name\":\"andamento\",\"default_workflow_ref\":\"{workflow}\",\"repositories\":[{repository}]}}}}"
        )
    }

    #[tokio::test]
    async fn fresh_directory_applies_all_documents_with_ownership_and_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).expect("nested manifest directory");
        write(&nested.join("policies.yaml"), &format!("{}---\n{}", manifest("alpha", "one"), manifest("gamma", "three")));
        write(
            &dir.path().join("beta.json"),
            r#"{"apiVersion":"flotilla.work/v1","kind":"PlacementPolicy","metadata":{"name":"beta"},"spec":{"pool":"two","priority":100}}"#,
        );
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());

        let report = reconciler.reconcile_once_for_test().await.expect("manifest pass");

        assert_eq!(report.created, 3);
        let expected_root = manifest_root_name("local", dir.path(), "local");
        for (name, source) in [("alpha", "nested/policies.yaml"), ("gamma", "nested/policies.yaml"), ("beta", "beta.json")] {
            let object = backend.using::<PlacementPolicy>(NAMESPACE).get(name).await.expect("manifest object");
            assert_eq!(object.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some(MANIFEST_MANAGED_BY_VALUE));
            assert_eq!(object.metadata.annotations.get(MANIFEST_SOURCE_ANNOTATION).map(String::as_str), Some("local"));
            assert_eq!(object.metadata.annotations.get(MANIFEST_PATH_ANNOTATION).map(String::as_str), Some(source));
            assert_eq!(object.metadata.annotations.get(MANIFEST_REVISION_ANNOTATION).map(String::as_str), Some("unversioned"));
            assert_eq!(
                object.metadata.annotations.get(MANIFEST_RECONCILER_ROOT_ANNOTATION).map(String::as_str),
                Some(expected_root.as_str())
            );
            assert_eq!(
                object.metadata.annotations.get(LAST_APPLIED_HASH_ANNOTATION),
                Some(
                    &resource_document_spec_hash(&serde_json::to_value(object.to_k8s_object()).expect("resource document"))
                        .expect("spec digest")
                )
            );
        }
        assert_eq!(backend.using::<PlacementPolicy>(NAMESPACE).get("beta").await.expect("beta policy").spec.priority, 100);
    }

    #[tokio::test]
    async fn divergent_trees_only_write_when_the_declared_root_runs() {
        let kiwi_dir = tempfile::tempdir().expect("kiwi tempdir");
        let feta_dir = tempfile::tempdir().expect("feta tempdir");
        write(&kiwi_dir.path().join("policy.yaml"), &manifest("shared", "current"));
        write(&feta_dir.path().join("policy.yaml"), &manifest("shared", "stale"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());

        let mut kiwi = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, kiwi_dir.path())
            .with_declared_source("project-map", "kiwi")
            .with_revision("current-revision");
        kiwi.reconcile_once_for_test().await.expect("declared root pass");

        assert!(super::super::runtime::manifest_reconciler_enabled("kiwi", "kiwi"));
        assert!(!super::super::runtime::manifest_reconciler_enabled("kiwi", "feta"));
        // The false eligibility result means runtime never constructs or runs
        // a reconciler against feta_dir, regardless of its stale contents.
        let object = backend.using::<PlacementPolicy>(NAMESPACE).get("shared").await.expect("replicated manifest object");
        assert_eq!(object.spec.pool, "current");
        let expected_root = manifest_root_name("kiwi", kiwi_dir.path(), "project-map");
        assert_eq!(object.metadata.annotations.get(MANIFEST_RECONCILER_ROOT_ANNOTATION).map(String::as_str), Some(expected_root.as_str()));
        assert_eq!(object.metadata.annotations.get(MANIFEST_REVISION_ANNOTATION).map(String::as_str), Some("current-revision"));
    }

    #[tokio::test]
    async fn clean_manifest_tree_resolves_head_revision() {
        let dir = committed_manifest_repo();
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut reconciler = versioned_reconciler(dir.path(), backend.clone()).await;

        reconciler.reconcile_once_for_test().await.expect("clean revision");
        let applied = backend.using::<PlacementPolicy>(NAMESPACE).get("versioned").await.expect("applied manifest");
        let revision = git(dir.path(), &["rev-parse", "HEAD"]);
        assert_eq!(applied.metadata.annotations.get(MANIFEST_REVISION_ANNOTATION).map(String::as_str), Some(revision.as_str()));
    }

    #[tokio::test]
    async fn dirty_manifest_tree_is_rejected_as_unversioned_input() {
        let dir = committed_manifest_repo();
        write(&dir.path().join("untracked.yaml"), &manifest("draft", "uncommitted"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut reconciler = versioned_reconciler(dir.path(), backend).await;

        let error = reconciler.reconcile_once_for_test().await.expect_err("dirty tree must be rejected");

        assert!(error.contains("changes not represented by a revision"), "{error}");
    }

    #[tokio::test]
    async fn ignored_manifest_is_rejected_as_unversioned_input() {
        let dir = committed_manifest_repo();
        write(&dir.path().join(".gitignore"), "*.local.yaml\n");
        git(dir.path(), &["add", ".gitignore"]);
        git(dir.path(), &["commit", "-m", "ignore local manifests"]);
        write(&dir.path().join("draft.local.yaml"), &manifest("draft", "ignored"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut reconciler = versioned_reconciler(dir.path(), backend).await;

        let error = reconciler.reconcile_once_for_test().await.expect_err("ignored manifest must be rejected");

        assert!(error.contains("changes not represented by a revision"), "{error}");
    }

    #[tokio::test]
    async fn manifest_tree_outside_git_is_rejected() {
        let dir = tempfile::tempdir_in("/tmp").expect("non-git tempdir");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut reconciler = versioned_reconciler(dir.path(), backend).await;

        let error = reconciler.reconcile_once_for_test().await.expect_err("non-repository must be rejected");

        assert!(error.contains("checkout status failed"), "{error}");
    }

    #[tokio::test]
    async fn steady_state_and_restart_perform_no_writes_or_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("policy.yaml"), &manifest("steady", "one"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut first = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());
        first.reconcile_once_for_test().await.expect("initial pass");
        let before = backend.using::<PlacementPolicy>(NAMESPACE).get("steady").await.expect("policy");

        let mut events = backend.using::<PlacementPolicy>(NAMESPACE).watch(WatchStart::Now).await.expect("watch");
        let mut restarted = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());
        let report = restarted.reconcile_once_for_test().await.expect("restart pass");
        let after = backend.using::<PlacementPolicy>(NAMESPACE).get("steady").await.expect("policy");

        assert_eq!(report.unchanged, 1);
        assert_eq!(before.metadata.resource_version, after.metadata.resource_version);
        assert!(tokio::time::timeout(Duration::from_millis(20), events.next()).await.is_err());
    }

    #[tokio::test]
    async fn unrelated_source_revision_does_not_rewrite_unchanged_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("policy.yaml"), &manifest("steady-revision", "one"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path())
            .with_declared_source("project-map", "kiwi")
            .with_revision("revision-one");
        reconciler.reconcile_once_for_test().await.expect("initial pass");
        let before = backend.using::<PlacementPolicy>(NAMESPACE).get("steady-revision").await.expect("policy");

        reconciler.fixed_revision = Some("revision-two".to_string());
        let report = reconciler.reconcile_once_for_test().await.expect("unrelated revision pass");
        let after = backend.using::<PlacementPolicy>(NAMESPACE).get("steady-revision").await.expect("policy");

        assert_eq!(report.unchanged, 1);
        assert_eq!(report.updated, 0);
        assert_eq!(after.metadata.resource_version, before.metadata.resource_version);
        assert_eq!(after.metadata.annotations.get(MANIFEST_REVISION_ANNOTATION).map(String::as_str), Some("revision-one"));
    }

    #[tokio::test]
    async fn loop_recovers_when_manifest_directory_appears_later() {
        let parent = tempfile::tempdir().expect("tempdir");
        let root = parent.path().join("not-created-yet");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        materialize_manifest_root(&backend, NAMESPACE, &root, "local", "local").await.expect("materialize root");
        let task = tokio::spawn(ResourceManifestReconciler::new(backend.clone(), NAMESPACE, root.clone()).run(Duration::from_millis(5)));
        tokio::time::sleep(Duration::from_millis(10)).await;

        std::fs::create_dir(&root).expect("create manifest directory");
        write(&root.join("policy.yaml"), &manifest("late", "one"));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if backend.using::<PlacementPolicy>(NAMESPACE).get("late").await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("manifest loop should recover");

        task.abort();
    }

    #[tokio::test]
    async fn manifest_updates_all_declared_fields_and_then_settles() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("policy.yaml");
        write(&path, &manifest("moving", "one"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());
        reconciler.reconcile_once_for_test().await.expect("initial pass");
        let resolver = backend.using::<PlacementPolicy>(NAMESPACE);
        let applied = resolver.get("moving").await.expect("policy");
        let mut live_meta = InputMeta::from(&applied.metadata);
        live_meta.labels.insert("another-controller/observation".to_string(), "kept".to_string());
        resolver.update(&live_meta, &applied.metadata.resource_version, &applied.spec).await.expect("add external metadata");

        write(&path, &manifest_with_priority("moving", "two", 100));
        let report = reconciler.reconcile_once_for_test().await.expect("fast-forward pass");
        let object = resolver.get("moving").await.expect("policy");

        assert_eq!(report.updated, 1);
        assert_eq!(object.spec.pool, "two");
        assert_eq!(object.spec.priority, 100);
        assert_eq!(object.metadata.labels.get("another-controller/observation").map(String::as_str), Some("kept"));
        assert_eq!(
            object.metadata.annotations.get(LAST_APPLIED_HASH_ANNOTATION),
            Some(
                &resource_document_spec_hash(&serde_json::to_value(object.to_k8s_object()).expect("resource document"))
                    .expect("persisted spec digest")
            ),
            "last-applied must describe the ownership-filtered spec"
        );

        let settled_version = object.metadata.resource_version;
        let settled = reconciler.reconcile_once_for_test().await.expect("settled pass");
        let object = resolver.get("moving").await.expect("settled policy");

        assert_eq!(settled.unchanged, 1);
        assert_eq!(settled.updated, 0);
        assert_eq!(settled.drifted, 0);
        assert_eq!(object.metadata.resource_version, settled_version, "settled manifests must not keep writing");
        let diagnostics = backend.diagnostics().await.expect("diagnostics").expect("embedded diagnostics");
        assert!(diagnostics.field_ownership_violations.is_empty(), "manifest projection must not synthesize ownership violations");
    }

    #[tokio::test]
    async fn omitted_serde_defaults_remain_steady_and_can_fast_forward() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workflow.yaml");
        let workflow = |command: &str| {
            format!(
                "apiVersion: flotilla.work/v1\nkind: WorkflowTemplate\nmetadata:\n  name: defaults\nspec:\n  vessels:\n    - name: work\n      crew:\n        - role: coder\n          command: {command}\n"
            )
        };
        write(&path, &workflow("echo-one"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());
        reconciler.reconcile_once_for_test().await.expect("initial pass");

        let steady = reconciler.reconcile_once_for_test().await.expect("steady pass");
        write(&path, &workflow("echo-two"));
        let updated = reconciler.reconcile_once_for_test().await.expect("fast-forward pass");
        let object = backend.using::<WorkflowTemplate>(NAMESPACE).get("defaults").await.expect("workflow");

        assert_eq!(steady.unchanged, 1);
        assert_eq!(steady.drifted, 0);
        assert_eq!(updated.updated, 1);
        assert_eq!(serde_json::to_value(&object.spec).expect("workflow spec")["vessels"][0]["crew"][0]["command"], "echo-two");
    }

    #[tokio::test]
    async fn in_process_daemon_exposes_refusal_and_stall_without_managed_state_annotations() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("policy.yaml"), &manifest("colliding", "desired"));
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        write(&config_dir.join("daemon.toml"), "machine_id = \"manifest-test\"\n");
        let daemon = InProcessDaemon::new(
            Vec::new(),
            Arc::new(ConfigStore::with_base(config_dir)),
            fake_discovery_with_provider_set(FakeDiscoveryProviders::new()),
            HostName::local(),
        )
        .await;
        let backend = daemon.resource_backend();
        let policies = backend.using::<PlacementPolicy>(NAMESPACE);
        let unmanaged = policies
            .create(
                &InputMeta::builder().name("colliding".to_string()).build(),
                &PlacementPolicySpec::builder().pool("live".to_string()).build(),
            )
            .await
            .expect("create unmanaged object");
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());
        reconciler.reconcile_once_for_test().await.expect("reconcile");
        let root = test_root(&backend).await;
        let status = root.status.expect("status");
        assert_eq!(status.documents[&key("colliding")].phase, DocumentPhase::Refused);
        assert!(status.stalled.expect("stall").evidence.contains("policy.yaml"));
        let after = policies.get("colliding").await.expect("managed object");
        assert_eq!(after.metadata.resource_version, unmanaged.metadata.resource_version);
        assert!(!after.metadata.annotations.contains_key("flotilla.work/manifest-refusal"));
    }

    #[tokio::test]
    async fn live_drift_is_left_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("policy.yaml");
        write(&path, &manifest("drifted", "one"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());
        reconciler.reconcile_once_for_test().await.expect("initial pass");
        let resolver = backend.using::<PlacementPolicy>(NAMESPACE);
        let applied = resolver.get("drifted").await.expect("policy");
        resolver
            .update(
                &InputMeta::from(&applied.metadata),
                &applied.metadata.resource_version,
                &PlacementPolicySpec::builder().pool("live-edit".to_string()).build(),
            )
            .await
            .expect("edit live spec");

        write(&path, &manifest("drifted", "manifest-edit"));
        let report = reconciler.reconcile_once_for_test().await.expect("drift pass");
        let object = resolver.get("drifted").await.expect("policy");

        assert_eq!(report.drifted, 1);
        assert_eq!(object.spec.pool, "live-edit");
        assert!(!object.metadata.annotations.contains_key("flotilla.work/manifest-refusal"));
        let root = test_root(&backend).await;
        let status = root.status.expect("status");
        assert_eq!(status.documents[&key("drifted")].phase, DocumentPhase::Drifted);
        assert!(status.documents[&key("drifted")].live_hash.is_some());
    }

    #[tokio::test]
    async fn drift_updates_root_status_without_mutating_managed_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("policy.yaml");
        write(&path, &manifest("atomic-refusal", "one"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let resolver = backend.using::<PlacementPolicy>(NAMESPACE);
        let mut reconciler = ResourceManifestReconciler::new(backend, NAMESPACE, dir.path());
        reconciler.reconcile_once_for_test().await.expect("initial pass");
        let applied = resolver.get("atomic-refusal").await.expect("policy");
        let edited = resolver
            .update(
                &InputMeta::from(&applied.metadata),
                &applied.metadata.resource_version,
                &PlacementPolicySpec::builder().pool("live-edit".to_string()).build(),
            )
            .await
            .expect("edit live spec");
        write(&path, &manifest("atomic-refusal", "manifest-edit"));
        let mut events = resolver.watch(WatchStart::Now).await.expect("watch managed object");
        reconciler.reconcile_once_for_test().await.expect("drift pass");
        let root = test_root(&reconciler.backend).await;
        assert_eq!(root.status.expect("status").documents[&key("atomic-refusal")].phase, DocumentPhase::Drifted);
        assert_eq!(resolver.get("atomic-refusal").await.expect("policy").metadata.resource_version, edited.metadata.resource_version);
        assert!(tokio::time::timeout(Duration::from_millis(20), events.next()).await.is_err(), "refusal changed managed object");
    }

    #[tokio::test]
    async fn convergent_live_spec_repairs_any_baseline_and_ownership() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("policy.yaml"), &manifest("convergent", "same"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let resolver = backend.using::<PlacementPolicy>(NAMESPACE);
        resolver
            .create(
                &InputMeta::builder().name("convergent".to_string()).build(),
                &PlacementPolicySpec::builder().pool("same".to_string()).build(),
            )
            .await
            .expect("unmanaged but convergent policy");
        let mut reconciler = ResourceManifestReconciler::new(backend, NAMESPACE, dir.path());

        let report = reconciler.reconcile_once_for_test().await.expect("repair pass");
        let object = resolver.get("convergent").await.expect("policy");
        let digest = resource_document_spec_hash(&serde_json::to_value(object.to_k8s_object()).expect("document")).expect("digest");

        assert_eq!(report.updated, 1);
        assert_eq!(report.drifted, 0);
        assert_eq!(report.unmanaged, 0);
        assert_eq!(object.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some(MANIFEST_MANAGED_BY_VALUE));
        assert_eq!(object.metadata.annotations.get(LAST_APPLIED_HASH_ANNOTATION), Some(&digest));
    }

    #[tokio::test]
    async fn fresh_object_has_persisted_baseline_on_following_pass() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("policy.yaml"), &manifest("fresh-baseline", "one"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());

        assert_eq!(reconciler.reconcile_once_for_test().await.expect("creation pass").created, 1);
        let next = reconciler.reconcile_once_for_test().await.expect("verification pass");
        let object = backend.using::<PlacementPolicy>(NAMESPACE).get("fresh-baseline").await.expect("policy");
        let digest = resource_document_spec_hash(&serde_json::to_value(object.to_k8s_object()).expect("document")).expect("digest");

        assert_eq!(next.unchanged, 1);
        assert_eq!(next.drifted, 0);
        assert_eq!(object.metadata.annotations.get(LAST_APPLIED_HASH_ANNOTATION), Some(&digest));
    }

    #[tokio::test]
    async fn suspended_object_is_untouched_and_resumes_after_unsuspension() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("policy.yaml");
        write(&path, &manifest("suspended", "manifest"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let resolver = backend.using::<PlacementPolicy>(NAMESPACE);
        let mut reconciler = ResourceManifestReconciler::new(backend, NAMESPACE, dir.path());
        reconciler.reconcile_once_for_test().await.expect("creation");
        let applied = resolver.get("suspended").await.expect("policy");
        set_suspended(&reconciler.backend, key("suspended"), true).await;
        resolver
            .update(
                &InputMeta::from(&applied.metadata),
                &applied.metadata.resource_version,
                &PlacementPolicySpec::builder().pool("hotfix".to_string()).build(),
            )
            .await
            .expect("hotfix");
        write(&path, &manifest("suspended", "new-manifest"));
        set_resolution(&reconciler.backend, key("suspended"), ResolutionAction::Sync, "token-1").await;
        assert_eq!(reconciler.reconcile_once_for_test().await.expect("suspended pass").unchanged, 1);
        assert_eq!(resolver.get("suspended").await.expect("policy").spec.pool, "hotfix");
        let root = test_root(&reconciler.backend).await;
        let state = &root.status.expect("status").documents[&key("suspended")];
        assert_eq!(state.phase, DocumentPhase::Suspended);
        assert_eq!(state.resolved_token, None);
        set_suspended(&reconciler.backend, key("suspended"), false).await;
        assert_eq!(reconciler.reconcile_once_for_test().await.expect("sync pass").updated, 1);
        assert_eq!(resolver.get("suspended").await.expect("policy").spec.pool, "new-manifest");
    }

    #[tokio::test]
    async fn sync_resolution_makes_manifest_win_at_reconciler_seam() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("policy.yaml");
        write(&path, &manifest("sync-me", "manifest"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let resolver = backend.using::<PlacementPolicy>(NAMESPACE);
        let mut reconciler = ResourceManifestReconciler::new(backend, NAMESPACE, dir.path());
        reconciler.reconcile_once_for_test().await.expect("creation");
        let applied = resolver.get("sync-me").await.expect("policy");
        resolver
            .update(
                &InputMeta::from(&applied.metadata),
                &applied.metadata.resource_version,
                &PlacementPolicySpec::builder().pool("live".to_string()).build(),
            )
            .await
            .expect("live edit");
        reconciler.reconcile_once_for_test().await.expect("refusal");
        set_resolution(&reconciler.backend, key("sync-me"), ResolutionAction::Sync, "token-1").await;

        let report = reconciler.reconcile_once_for_test().await.expect("sync pass");
        let synced = resolver.get("sync-me").await.expect("synced policy");
        assert!(report.errors.is_empty(), "sync errors: {:?}", report.errors);
        assert_eq!(report.updated, 1, "report={report:?}, synced={synced:?}");
        assert_eq!(synced.spec.pool, "manifest");
        assert!(!synced.metadata.annotations.contains_key("flotilla.work/manifest-refusal"));
        assert_eq!(
            test_root(&reconciler.backend).await.status.expect("status").documents[&key("sync-me")].resolved_token.as_deref(),
            Some("token-1")
        );
        assert_eq!(reconciler.reconcile_once_for_test().await.expect("same token pass").unchanged, 1);
        let edited = resolver.get("sync-me").await.expect("policy");
        resolver
            .update(
                &InputMeta::from(&edited.metadata),
                &edited.metadata.resource_version,
                &PlacementPolicySpec::builder().pool("second-live-edit".to_string()).build(),
            )
            .await
            .expect("second live edit");
        assert_eq!(reconciler.reconcile_once_for_test().await.expect("same token cannot resolve again").drifted, 1);
        assert_eq!(resolver.get("sync-me").await.expect("policy").spec.pool, "second-live-edit");
        set_resolution(&reconciler.backend, key("sync-me"), ResolutionAction::Sync, "token-2").await;
        assert_eq!(reconciler.reconcile_once_for_test().await.expect("new token pass").updated, 1);
        assert_eq!(resolver.get("sync-me").await.expect("policy").spec.pool, "manifest");
    }

    #[tokio::test]
    async fn claimed_resolution_is_not_replayed_after_reconciler_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("policy.yaml"), &manifest("interrupted", "manifest"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let resolver = backend.using::<PlacementPolicy>(NAMESPACE);
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());
        reconciler.reconcile_once_for_test().await.expect("creation");
        let applied = resolver.get("interrupted").await.expect("policy");
        resolver
            .update(
                &InputMeta::from(&applied.metadata),
                &applied.metadata.resource_version,
                &PlacementPolicySpec::builder().pool("live".to_string()).build(),
            )
            .await
            .expect("live edit");
        reconciler.reconcile_once_for_test().await.expect("drift pass");
        set_resolution(&backend, key("interrupted"), ResolutionAction::Sync, "token-1").await;
        let root = test_root(&backend).await;
        let status = root.status.expect("status");
        let previous = &status.documents[&key("interrupted")];
        assert!(reconciler.claim_resolution(&key("interrupted"), "token-1", Some(previous)).await.expect("claim"));

        let mut restarted = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());
        assert_eq!(restarted.reconcile_once_for_test().await.expect("restart pass").drifted, 1);
        assert_eq!(resolver.get("interrupted").await.expect("policy").spec.pool, "live");
        let root = test_root(&backend).await;
        let status = root.status.expect("status");
        let state = &status.documents[&key("interrupted")];
        assert_eq!(state.resolved_token.as_deref(), Some("token-1"));
        assert_eq!(state.resolution_outcome, Some(ResolutionOutcome::Started));

        set_resolution(&backend, key("interrupted"), ResolutionAction::Sync, "token-2").await;
        assert_eq!(restarted.reconcile_once_for_test().await.expect("new token pass").updated, 1);
        assert_eq!(resolver.get("interrupted").await.expect("policy").spec.pool, "manifest");
    }

    #[tokio::test]
    async fn only_declared_manifest_reconciler_reauthors_a_divergent_project() {
        // #1736 owns selecting exactly one reconciler per manifest source. Model
        // that contract here by constructing only the declared root's loop.
        let current_dir = tempfile::tempdir().expect("current manifest dir");
        write(&current_dir.path().join("project.json"), &project_manifest("single-agent", true));

        let current = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("current-root"));
        let stale = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("stale-root"));
        let mut current_reconciler = ResourceManifestReconciler::new(current.clone(), NAMESPACE, current_dir.path());
        current_reconciler.reconcile_once_for_test().await.expect("current creation");
        let stale_document = serde_json::from_str(&project_manifest("single-agent", false)).expect("stale manifest document");
        apply_manifest_resource_document(&stale, NAMESPACE, stale_document).await.expect("seed pre-existing divergent peer state");

        current
            .replica_writer::<Project>(NodeId::new("stale-root"), NAMESPACE)
            .replace(&stale.using::<Project>(NAMESPACE).list().await.expect("stale projects"), Utc::now())
            .await
            .expect("replicate stale project to current root");
        set_resolution(
            &current,
            DocumentKey { path: "project.json".into(), kind: "Project".into(), namespace: NAMESPACE.into(), name: "andamento".into() },
            ResolutionAction::Sync,
            "token-1",
        )
        .await;
        current_reconciler.reconcile_once_for_test().await.expect("sync current manifest");

        stale
            .replica_writer::<Project>(NodeId::new("current-root"), NAMESPACE)
            .replace(&current.using::<Project>(NAMESPACE).list().await.expect("current projects"), Utc::now())
            .await
            .expect("replicate synced project to stale root");
        for iteration in 0..3 {
            patch_resource_annotation(
                &stale,
                NAMESPACE,
                "projects",
                "andamento",
                "test.flotilla.work/peer-observation",
                &iteration.to_string(),
            )
            .await
            .expect("metadata-only peer observation");
        }

        let project = stale.definitions::<Project>(NAMESPACE).get("andamento").await.expect("merged project");
        assert_eq!(project.spec.default_workflow_ref, "single-agent");
        assert_eq!(project.spec.repositories[0].alias.as_deref(), Some("andamento"));
        assert_eq!(project.spec.repositories[0].roles.len(), 3);
        let writer = project
            .metadata
            .merge
            .as_ref()
            .and_then(|merge| merge.fields.get("spec.default_workflow_ref"))
            .and_then(|field| field.writer.as_ref())
            .expect("workflow write attribution");
        assert_eq!(writer.source.as_deref(), Some(MANIFEST_WRITER_SOURCE));
        assert_eq!(writer.role, flotilla_resources::WriterRole::ReconcileLoop);
        assert_eq!(
            project
                .metadata
                .merge
                .as_ref()
                .and_then(|merge| merge.fields.get("spec.default_workflow_ref"))
                .expect("workflow merge metadata")
                .dot
                .author_root
                .as_str(),
            "current-root",
            "an undeclared peer's metadata writes must not re-author the declared reconciler's spec",
        );
    }

    #[tokio::test]
    async fn adopt_resolution_writes_live_spec_to_manifest_at_reconciler_seam() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("policy.yaml");
        write(&path, &manifest("adopt-me", "manifest"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let resolver = backend.using::<PlacementPolicy>(NAMESPACE);
        let mut reconciler = ResourceManifestReconciler::new(backend, NAMESPACE, dir.path());
        reconciler.reconcile_once_for_test().await.expect("creation");
        let applied = resolver.get("adopt-me").await.expect("policy");
        resolver
            .update(
                &InputMeta::from(&applied.metadata),
                &applied.metadata.resource_version,
                &PlacementPolicySpec::builder().pool("live".to_string()).build(),
            )
            .await
            .expect("live edit");
        reconciler.reconcile_once_for_test().await.expect("refusal");
        set_resolution(&reconciler.backend, key("adopt-me"), ResolutionAction::Adopt, "token-1").await;

        let report = reconciler.reconcile_once_for_test().await.expect("adopt pass");
        let rendered = std::fs::read_to_string(path).expect("updated manifest");
        assert_eq!(report.updated, 1);
        assert!(rendered.contains("pool: live"));
        assert_eq!(reconciler.reconcile_once_for_test().await.expect("settled pass").unchanged, 1);
    }

    #[tokio::test]
    async fn unmanaged_object_is_never_adopted() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("policy.yaml"), &manifest("unmanaged", "manifest"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        backend
            .using::<PlacementPolicy>(NAMESPACE)
            .create(
                &InputMeta::builder().name("unmanaged".to_string()).build(),
                &PlacementPolicySpec::builder().pool("live".to_string()).build(),
            )
            .await
            .expect("unmanaged policy");
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());

        let report = reconciler.reconcile_once_for_test().await.expect("manifest pass");
        let object = backend.using::<PlacementPolicy>(NAMESPACE).get("unmanaged").await.expect("policy");

        assert_eq!(report.unmanaged, 1);
        assert_eq!(object.spec.pool, "live");
        assert!(!object.metadata.labels.contains_key(MANAGED_BY_LABEL));
        assert!(!object.metadata.annotations.contains_key("flotilla.work/manifest-refusal"));
        let status = test_root(&backend).await.status.expect("status");
        assert_eq!(status.documents[&key("unmanaged")].phase, DocumentPhase::Refused);
        assert!(status.stalled.expect("stall").evidence.contains("unmanaged"));

        set_resolution(&backend, key("unmanaged"), ResolutionAction::Sync, "token-1").await;
        let report = reconciler.reconcile_once_for_test().await.expect("unmanaged sync pass");
        let still_unmanaged = backend.using::<PlacementPolicy>(NAMESPACE).get("unmanaged").await.expect("policy");
        let root = test_root(&backend).await;
        let status = root.status.expect("status");
        let state = &status.documents[&key("unmanaged")];
        assert_eq!(report.unmanaged, 1);
        assert_eq!(still_unmanaged.spec.pool, "live");
        assert_eq!(still_unmanaged.metadata.resource_version, object.metadata.resource_version);
        assert_eq!(state.phase, DocumentPhase::Refused);
        assert_eq!(state.resolved_token, None);
    }

    #[tokio::test]
    async fn unmanaged_warning_is_rearmed_after_ownership_is_restored() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("policy.yaml"), &manifest("unmanaged", "manifest"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let resolver = backend.using::<PlacementPolicy>(NAMESPACE);
        resolver
            .create(
                &InputMeta::builder().name("unmanaged".to_string()).build(),
                &PlacementPolicySpec::builder().pool("live".to_string()).build(),
            )
            .await
            .expect("unmanaged policy");
        let mut reconciler = ResourceManifestReconciler::new(backend, NAMESPACE, dir.path());
        let identity =
            ObjectIdentity { kind: "PlacementPolicy".to_string(), namespace: NAMESPACE.to_string(), name: "unmanaged".to_string() };

        reconciler.reconcile_once_for_test().await.expect("initial unmanaged pass");
        assert!(reconciler.warned_unmanaged.contains(&identity));

        let object = resolver.get("unmanaged").await.expect("policy");
        let mut meta = InputMeta::from(&object.metadata);
        meta.labels.insert(MANAGED_BY_LABEL.to_string(), MANIFEST_MANAGED_BY_VALUE.to_string());
        resolver.update(&meta, &object.metadata.resource_version, &object.spec).await.expect("restore ownership");
        reconciler.reconcile_once_for_test().await.expect("managed pass");
        assert!(!reconciler.warned_unmanaged.contains(&identity));

        let object = resolver.get("unmanaged").await.expect("policy");
        let mut meta = InputMeta::from(&object.metadata);
        meta.labels.remove(MANAGED_BY_LABEL);
        resolver.update(&meta, &object.metadata.resource_version, &object.spec).await.expect("remove ownership");
        let report = reconciler.reconcile_once_for_test().await.expect("unmanaged again");

        assert_eq!(report.unmanaged, 1);
        assert!(reconciler.warned_unmanaged.contains(&identity));
    }

    #[tokio::test]
    async fn malformed_document_reports_its_path_and_does_not_block_remaining_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("broken.yaml"), "kind: [");
        write(&dir.path().join("valid.yaml"), &manifest("valid", "one"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());

        let report = reconciler.reconcile_once_for_test().await.expect("manifest pass");

        assert_eq!(report.created, 1);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].path, PathBuf::from("broken.yaml"));
        assert!(report.errors[0].reason.contains("parse YAML"));
        backend.using::<PlacementPolicy>(NAMESPACE).get("valid").await.expect("valid policy");
        let status = test_root(&backend).await.status.expect("status");
        let broken = DocumentKey { path: "broken.yaml".into(), kind: String::new(), namespace: String::new(), name: String::new() };
        assert_eq!(status.documents[&broken].phase, DocumentPhase::Refused);
        assert!(status.stalled.expect("stall").evidence.contains("broken.yaml"));
    }

    #[tokio::test]
    async fn removing_a_manifest_does_not_prune_its_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("policy.yaml");
        write(&path, &manifest("retained", "one"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());
        reconciler.reconcile_once_for_test().await.expect("initial pass");
        std::fs::remove_file(path).expect("remove manifest");

        reconciler.reconcile_once_for_test().await.expect("additive pass");

        backend.using::<PlacementPolicy>(NAMESPACE).get("retained").await.expect("object must not be pruned");
        let root = test_root(&backend).await;
        backend.using::<ManifestRoot>(NAMESPACE).delete(&root.metadata.name).await.expect("remove declared root");
        backend.using::<PlacementPolicy>(NAMESPACE).get("retained").await.expect("root deletion must not prune object");
    }

    #[tokio::test]
    async fn legacy_state_annotations_decode_and_are_stripped_on_apply() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("policy.yaml"), &manifest("legacy", "one"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let resolver = backend.using::<PlacementPolicy>(NAMESPACE);
        let mut reconciler = ResourceManifestReconciler::new(backend, NAMESPACE, dir.path());
        reconciler.reconcile_once_for_test().await.expect("initial pass");
        let object = resolver.get("legacy").await.expect("policy");
        let mut meta = InputMeta::from(&object.metadata);
        for key in LEGACY_STATE_ANNOTATIONS {
            meta.annotations.insert(key.into(), "stale".into());
        }
        meta.annotations.insert("flotilla.work/manifest-resolution".into(), "sync".into());
        let legacy = resolver.update(&meta, &object.metadata.resource_version, &object.spec).await.expect("write legacy metadata");
        flotilla_resources::decode_stored_resource_document(&serde_json::to_value(legacy.to_k8s_object()).expect("encode"))
            .expect("decode previous-generation annotations");
        assert_eq!(reconciler.reconcile_once_for_test().await.expect("cleanup pass").updated, 1);
        let cleaned = resolver.get("legacy").await.expect("policy");
        for key in LEGACY_STATE_ANNOTATIONS {
            assert!(!cleaned.metadata.annotations.contains_key(key), "stale state annotation {key}");
        }
        assert!(cleaned.metadata.annotations.contains_key(MANIFEST_BASELINE_HASH_ANNOTATION));
    }
}

#[cfg(test)]
mod registered_charter_tests {
    use flotilla_core::config::ConfigStore;
    use flotilla_core::in_process::InProcessDaemon;
    use flotilla_discovery_testkit::fake_discovery;
    use flotilla_protocol::HostName;
    use flotilla_resources::{
        CharterPointer, InMemoryBackend, PlacementPolicy, Project, ProjectRepositoryRole, ProjectRepositorySpec, ProjectSpec,
        RepositoryKey, ResourceProvenance,
    };

    use super::*;
    use crate::testkits::server::spawn_in_memory_request_topology;

    fn project(pointer: CharterPointer) -> Value {
        let spec = ProjectSpec::builder()
            .display_name("App".into())
            .default_workflow_ref("single-agent".into())
            .charter(pointer)
            .repositories(vec![ProjectRepositorySpec::builder()
                .repo(RepositoryKey("app-repo".into()))
                .roles(std::collections::BTreeSet::from([ProjectRepositoryRole::Code]))
                .build()])
            .build();
        serde_json::json!({"apiVersion": "flotilla.work/v1", "kind": "Project", "metadata": {"name": "app"}, "spec": spec})
    }

    // #2721: scope refusal happens before any resource writes and preserves the
    // last applied revision and records. Fixing the source recovers its attention.
    #[tokio::test]
    async fn scope_refusal_preserves_the_entire_last_applied_candidate() {
        let directory = tempfile::tempdir().expect("fleet directory");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let pointer = CharterPointer::Inline { documents: vec![], files: BTreeMap::new() };
        let good = project(pointer);
        std::fs::write(directory.path().join("project.json"), serde_json::to_string(&good).expect("encode")).expect("write");
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), "flotilla", directory.path())
            .with_binding(Some(CharterSource::LocalDirectory { directory: directory.path().to_string_lossy().into_owned() }));
        reconciler.reconcile_once_for_test().await.expect("initial apply");
        let initial_root = backend.using::<ManifestRoot>("flotilla").get(&reconciler.root_name()).await.expect("root");
        let initial_project = backend.using::<Project>("flotilla").get("app").await.expect("project");
        let mut bad = good.clone();
        bad["spec"]["display_name"] = serde_json::json!("Must not apply");
        bad["spec"]["charter"]["documents"] = serde_json::json!([{"apiVersion": "flotilla.work/v1", "kind": "PlacementPolicy", "metadata": {"name": "escape"}, "spec": {"pool": "other"}}]);
        std::fs::write(directory.path().join("project.json"), serde_json::to_string(&bad).expect("encode")).expect("write refused head");
        let error = reconciler.reconcile_once().await.expect_err("refuse whole candidate");
        assert!(error.contains("delegated scope `flotilla/app`"), "{error}");
        assert_eq!(backend.using::<Project>("flotilla").get("app").await.expect("project").spec, initial_project.spec);
        assert!(backend.using::<PlacementPolicy>("flotilla").list().await.expect("policies").items.is_empty());
        let refused =
            backend.using::<ManifestRoot>("flotilla").get(&reconciler.root_name()).await.expect("refused root").status.expect("status");
        assert_eq!(refused.applied_revision, initial_root.status.expect("initial status").applied_revision);
        assert!(refused.source_error.is_some());
        assert!(refused.stalled.is_some());
        std::fs::write(directory.path().join("project.json"), serde_json::to_string(&good).expect("encode")).expect("restore");
        reconciler.reconcile_once().await.expect("recover");
        let recovered = backend.using::<ManifestRoot>("flotilla").get(&reconciler.root_name()).await.expect("root").status.expect("status");
        assert!(recovered.source_error.is_none());
        assert!(recovered.stalled.is_none());
    }

    // #2721: even a legacy unbound fleet source stamps updated charter revisions
    // when resource specs are unchanged; pointer removal clears charter provenance.
    #[tokio::test]
    async fn unbound_source_refreshes_charter_provenance_and_can_return_to_legacy() {
        let directory = tempfile::tempdir().expect("fleet input");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let ensure = serde_json::json!({"apiVersion": "flotilla.work/v1", "kind": "ConvoyEnsure", "metadata": {"name": "app-governor"}, "spec": {"project_ref": "app", "role": "governor", "workflow_ref": "govern", "repositories": ["app-repo"]}});
        let mut registration = project(CharterPointer::Inline { documents: vec![ensure.clone()], files: BTreeMap::new() });
        let mut legacy_spec: ProjectSpec = serde_json::from_value(registration["spec"].clone()).expect("legacy spec");
        legacy_spec.charter = None;
        backend
            .definitions::<Project>("flotilla")
            .create(
                &InputMeta::builder()
                    .name("app".into())
                    .annotations(BTreeMap::from([(
                        flotilla_core::project_declaration::BOOTSTRAP_REPOSITORY_ANNOTATION.into(),
                        "app-repo".into(),
                    )]))
                    .build(),
                &legacy_spec,
            )
            .await
            .expect("legacy bootstrap Project");
        let path = directory.path().join("project.json");
        std::fs::write(&path, serde_json::to_string(&registration).expect("encode")).expect("registration");
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), "flotilla", directory.path()).with_revision("first");
        let report = reconciler.reconcile_once_for_test().await.expect("first apply");
        assert!(report.errors.is_empty(), "{report:?}");
        let registered = backend.using::<Project>("flotilla").get("app").await.expect("transferred registration");
        assert!(registered.spec.charter.is_some());
        assert_eq!(registered.metadata.labels[MANAGED_BY_LABEL], MANIFEST_MANAGED_BY_VALUE);
        assert_eq!(registered.metadata.annotations[flotilla_core::project_declaration::BOOTSTRAP_REPOSITORY_ANNOTATION], "app-repo");
        let ensures = backend.using::<flotilla_resources::ConvoyEnsure>("flotilla");
        assert_eq!(ensures.get("app-governor").await.expect("ensure").metadata.annotations[MANIFEST_REVISION_ANNOTATION], "first");
        reconciler.fixed_revision = Some("second".into());
        reconciler.reconcile_once().await.expect("refresh same specs");
        let refreshed = ensures.get("app-governor").await.expect("ensure");
        assert_eq!(refreshed.metadata.annotations[MANIFEST_REVISION_ANNOTATION], "second");
        assert_eq!(refreshed.metadata.annotations[crate::charter_delegation::CHARTER_REVISION], "second");
        registration["spec"].as_object_mut().expect("spec").remove("charter");
        std::fs::write(&path, serde_json::to_string(&registration).expect("encode")).expect("remove pointer");
        std::fs::write(directory.path().join("ensure.json"), serde_json::to_string(&ensure).expect("encode")).expect("legacy document");
        reconciler.fixed_revision = Some("third".into());
        reconciler.reconcile_once().await.expect("return to legacy");
        let legacy = ensures.get("app-governor").await.expect("preserved ensure");
        assert!(!legacy.metadata.annotations.contains_key(crate::charter_delegation::CHARTER_SCOPE));
        assert!(!legacy.metadata.annotations.contains_key(crate::charter_delegation::CHARTER_REVISION));
        assert_eq!(legacy.spec, refreshed.spec);
    }

    // #2721: a fleet-repo policy federates to the host it targets alongside a
    // legacy locally authored policy; updates federate without altering local inputs.
    #[tokio::test]
    async fn fleet_placement_policy_federates_alongside_host_local_policy() {
        let directory = tempfile::tempdir().expect("fleet input");
        let fleet_backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let host_backend = ResourceBackend::InMemory(InMemoryBackend::default());
        for (name, machine) in [("fleet-config", "fleet-root"), ("host-config", "host-root")] {
            let config = directory.path().join(name);
            std::fs::create_dir_all(&config).expect("config directory");
            std::fs::write(config.join("daemon.toml"), format!("machine_id = '{machine}'\n")).expect("machine identity");
        }
        let input = directory.path().join("input");
        std::fs::create_dir(&input).expect("input directory");
        let fleet = InProcessDaemon::new_with_resource_backend(
            vec![],
            Arc::new(ConfigStore::with_base(directory.path().join("fleet-config"))),
            fake_discovery(false),
            HostName::new("fleet-home"),
            fleet_backend.clone(),
        )
        .await;
        let host = InProcessDaemon::new_with_resource_backend(
            vec![],
            Arc::new(ConfigStore::with_base(directory.path().join("host-config"))),
            fake_discovery(false),
            HostName::new("target-host"),
            host_backend.clone(),
        )
        .await;
        let host_ref = host.local_host_summary().await.environment_id.host_id().expect("host ID").to_string();
        let spec = flotilla_resources::PlacementPolicySpec::builder()
            .pool("passthrough".into())
            .host_direct(flotilla_resources::HostDirectPlacementPolicySpec {
                host_ref,
                checkout: flotilla_resources::HostDirectPlacementPolicyCheckout::Worktree,
            })
            .priority(40)
            .build();
        host_backend
            .using::<PlacementPolicy>("flotilla")
            .create(&InputMeta::builder().name("legacy-local".into()).build(), &spec)
            .await
            .expect("legacy local policy");
        let path = input.join("placement.json");
        let mut document = serde_json::json!({"apiVersion": "flotilla.work/v1", "kind": "PlacementPolicy", "metadata": {"name": "fleet-target-host"}, "spec": spec});
        std::fs::write(&path, serde_json::to_string(&document).expect("encode")).expect("fleet policy");
        let mut reconciler = ResourceManifestReconciler::new(fleet_backend.clone(), "flotilla", &input)
            .with_binding(Some(CharterSource::LocalDirectory { directory: input.to_string_lossy().into_owned() }));
        reconciler.reconcile_once_for_test().await.expect("apply fleet policy");
        let topology = spawn_in_memory_request_topology(fleet, host).await.expect("connect daemon stores");
        for priority in [40, 75] {
            if priority == 75 {
                document["spec"]["priority"] = serde_json::json!(priority);
                std::fs::write(&path, serde_json::to_string(&document).expect("encode")).expect("update fleet policy");
                reconciler.reconcile_once().await.expect("apply policy update");
            }
            let replicated = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Ok(policy) = host_backend.including_replicas::<PlacementPolicy>("flotilla").get("fleet-target-host").await {
                        if policy.object.spec.priority == priority {
                            break policy;
                        }
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("policy federates to target host");
            assert!(matches!(replicated.provenance, ResourceProvenance::Replica { .. }));
            assert_eq!(
                replicated.object.spec.host_direct.as_ref().expect("host strategy").host_ref,
                spec.host_direct.as_ref().expect("host strategy").host_ref
            );
            assert_eq!(
                host_backend.using::<PlacementPolicy>("flotilla").get("legacy-local").await.expect("local policy preserved").spec,
                spec
            );
        }
        drop(topology);
    }
}
