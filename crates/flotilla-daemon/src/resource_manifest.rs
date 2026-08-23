//! Additive, ownership-aware application of resource manifest directories.
//!
//! This reconciler intentionally does not prune objects whose source documents
//! disappear. Deletion and adoption are separate, explicit lifecycle acts.

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use flotilla_resources::{
    apply_manifest_resource_document, get_resource_kind, resource_document_spec_hash, ResourceBackend, ResourceError, MANAGED_BY_LABEL,
    MANIFEST_RESOLUTION_ANNOTATION,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use tracing::{info, warn};

pub const MANIFEST_MANAGED_BY_VALUE: &str = "manifest";
pub const MANIFEST_SOURCE_ANNOTATION: &str = "flotilla.work/manifest-source";
pub const LAST_APPLIED_HASH_ANNOTATION: &str = "flotilla.work/last-applied-hash";
pub const MANIFEST_REFUSAL_ANNOTATION: &str = "flotilla.work/manifest-refusal";
pub const MANIFEST_LIVE_HASH_ANNOTATION: &str = "flotilla.work/manifest-live-hash";
pub const MANIFEST_DESIRED_HASH_ANNOTATION: &str = "flotilla.work/manifest-desired-hash";
pub const MANIFEST_BASELINE_HASH_ANNOTATION: &str = "flotilla.work/manifest-baseline-hash";
pub const MANIFEST_SUSPEND_ANNOTATION: &str = "flotilla.work/manifest-suspend";

const REFUSAL_ANNOTATIONS: [&str; 4] =
    [MANIFEST_REFUSAL_ANNOTATION, MANIFEST_LIVE_HASH_ANNOTATION, MANIFEST_DESIRED_HASH_ANNOTATION, MANIFEST_BASELINE_HASH_ANNOTATION];

type LoadedManifestFile = (PathBuf, Result<Vec<Value>, String>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ObjectIdentity {
    kind: String,
    namespace: String,
    name: String,
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
    warned_unmanaged: HashSet<ObjectIdentity>,
    warned_drift: HashSet<(ObjectIdentity, String, String)>,
}

impl ResourceManifestReconciler {
    pub fn new(backend: ResourceBackend, default_namespace: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            backend,
            default_namespace: default_namespace.into(),
            root: root.into(),
            warned_unmanaged: HashSet::new(),
            warned_drift: HashSet::new(),
        }
    }

    pub async fn run(mut self, interval: Duration) -> Result<(), ResourceError> {
        let mut last_root_error = None;
        loop {
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

    pub async fn reconcile_once(&mut self) -> Result<ManifestPassReport, String> {
        let root = self.root.clone();
        let files = tokio::task::spawn_blocking(move || load_manifest_files(&root))
            .await
            .map_err(|error| format!("manifest file loading task failed: {error}"))??;
        let mut report = ManifestPassReport::default();
        for (path, documents) in files {
            let relative = path.strip_prefix(&self.root).unwrap_or(&path).to_path_buf();
            let documents = match documents {
                Ok(documents) => documents,
                Err(reason) => {
                    report.errors.push(ManifestDocumentError { path: relative, reason });
                    continue;
                }
            };
            for (index, document) in documents.into_iter().enumerate() {
                if let Err(reason) = self.reconcile_document(&relative, document, &mut report).await {
                    let path = if index == 0 { relative.clone() } else { PathBuf::from(format!("{}#{}", relative.display(), index + 1)) };
                    report.errors.push(ManifestDocumentError { path, reason });
                }
            }
        }
        Ok(report)
    }

    async fn reconcile_document(&mut self, source: &Path, mut document: Value, report: &mut ManifestPassReport) -> Result<(), String> {
        let identity = document_identity(&document, &self.default_namespace)?;
        let desired_hash = resource_document_spec_hash(&document).map_err(|error| format!("{identity}: {error}"))?;
        stamp_manifest_metadata(&mut document, source, &desired_hash)?;

        let existing = match get_resource_kind(&self.backend, &identity.namespace, &identity.kind, &identity.name).await {
            Ok(existing) => Some(existing.value),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(format!("{identity}: {error}")),
        };
        let Some(existing) = existing else {
            self.apply_manifest_document(document).await.map_err(|error| format!("{identity}: {error}"))?;
            report.created += 1;
            return Ok(());
        };

        let annotations = string_map(&existing, "annotations")?;
        if annotations.get(MANIFEST_SUSPEND_ANNOTATION).is_some_and(|value| value == "true") {
            report.unchanged += 1;
            return Ok(());
        }

        let live_hash = resource_document_spec_hash(&existing).map_err(|error| format!("{identity}: {error}"))?;
        let last_applied = annotations.get(LAST_APPLIED_HASH_ANNOTATION).cloned().unwrap_or_else(|| "<missing>".to_string());
        let resolution = annotations.get(MANIFEST_RESOLUTION_ANNOTATION).map(String::as_str);

        // Equality is stronger evidence than a missing or stale baseline: this
        // write changes metadata only, so it is always safe to repair ownership
        // and record the spec that is already present.
        if live_hash == desired_hash {
            let source_value = source.to_string_lossy();
            let labels = string_map(&existing, "labels")?;
            let settled = last_applied == live_hash
                && labels.get(MANAGED_BY_LABEL).map(String::as_str) == Some(MANIFEST_MANAGED_BY_VALUE)
                && annotations.get(MANIFEST_SOURCE_ANNOTATION).map(String::as_str) == Some(source_value.as_ref())
                && resolution.is_none()
                && REFUSAL_ANNOTATIONS.iter().all(|key| !annotations.contains_key(*key));
            if settled {
                self.clear_warnings(&identity);
                report.unchanged += 1;
                return Ok(());
            }
            preserve_external_metadata(&mut document, &existing)?;
            clear_manifest_state(&mut document)?;
            self.apply_manifest_document(document).await.map_err(|error| format!("{identity}: {error}"))?;
            self.clear_warnings(&identity);
            report.unchanged += 1;
            return Ok(());
        }

        if resolution == Some("adopt") {
            self.adopt_live_spec(source, &identity, &existing).await?;
            let mut adopted = existing;
            set_annotation(&mut adopted, LAST_APPLIED_HASH_ANNOTATION, &live_hash)?;
            stamp_manifest_ownership(&mut adopted, source)?;
            clear_manifest_state(&mut adopted)?;
            self.apply_manifest_document(adopted).await.map_err(|error| format!("{identity}: {error}"))?;
            self.clear_warnings(&identity);
            report.updated += 1;
            return Ok(());
        }

        let labels = string_map(&existing, "labels")?;
        if labels.get(MANAGED_BY_LABEL).map(String::as_str) != Some(MANIFEST_MANAGED_BY_VALUE) {
            if self.warned_unmanaged.insert(identity.clone()) {
                warn!(object = %identity, source = %source.display(), "manifest names an unmanaged object; refusing adoption");
            }
            self.record_refusal(&existing, "unmanaged", &live_hash, &last_applied, &desired_hash).await?;
            report.unmanaged += 1;
            return Ok(());
        }
        self.warned_unmanaged.remove(&identity);

        if live_hash != last_applied && resolution != Some("sync") {
            if self.warned_drift.insert((identity.clone(), live_hash.clone(), last_applied.clone())) {
                warn!(
                    object = %identity,
                    live_digest = %live_hash,
                    last_applied_digest = %last_applied,
                    desired_digest = %desired_hash,
                    "manifest-managed object has live drift; refusing overwrite"
                );
            }
            self.record_refusal(&existing, "drift", &live_hash, &last_applied, &desired_hash).await?;
            report.drifted += 1;
            return Ok(());
        }

        let source_value = source.to_string_lossy();
        if resolution.is_none()
            && desired_hash == last_applied
            && annotations.get(MANIFEST_SOURCE_ANNOTATION).map(String::as_str) == Some(source_value.as_ref())
        {
            report.unchanged += 1;
            return Ok(());
        }

        preserve_external_metadata(&mut document, &existing)?;
        clear_manifest_state(&mut document)?;
        self.apply_manifest_document(document).await.map_err(|error| format!("{identity}: {error}"))?;
        self.clear_warnings(&identity);
        report.updated += 1;
        Ok(())
    }

    fn clear_warnings(&mut self, identity: &ObjectIdentity) {
        self.warned_unmanaged.remove(identity);
        self.warned_drift.retain(|(warned, _, _)| warned != identity);
    }

    async fn record_refusal(
        &self,
        existing: &Value,
        reason: &str,
        live_hash: &str,
        baseline_hash: &str,
        desired_hash: &str,
    ) -> Result<(), String> {
        let annotations = string_map(existing, "annotations")?;
        if annotations.get(MANIFEST_REFUSAL_ANNOTATION).map(String::as_str) == Some(reason)
            && annotations.get(MANIFEST_LIVE_HASH_ANNOTATION).map(String::as_str) == Some(live_hash)
            && annotations.get(MANIFEST_BASELINE_HASH_ANNOTATION).map(String::as_str) == Some(baseline_hash)
            && annotations.get(MANIFEST_DESIRED_HASH_ANNOTATION).map(String::as_str) == Some(desired_hash)
        {
            return Ok(());
        }
        let mut refused = existing.clone();
        set_annotation(&mut refused, MANIFEST_REFUSAL_ANNOTATION, reason)?;
        set_annotation(&mut refused, MANIFEST_LIVE_HASH_ANNOTATION, live_hash)?;
        set_annotation(&mut refused, MANIFEST_BASELINE_HASH_ANNOTATION, baseline_hash)?;
        set_annotation(&mut refused, MANIFEST_DESIRED_HASH_ANNOTATION, desired_hash)?;
        apply_manifest_resource_document(&self.backend, &self.default_namespace, refused).await.map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn adopt_live_spec(&self, source: &Path, identity: &ObjectIdentity, existing: &Value) -> Result<(), String> {
        let path = self.root.join(source);
        let mut documents = parse_documents(&path).map_err(|error| format!("{identity}: {error}"))?;
        let document = documents
            .iter_mut()
            .find(|document| document_identity(document, &self.default_namespace).as_ref() == Ok(identity))
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
        temporary
            .write_all(rendered.as_bytes())
            .and_then(|()| temporary.flush())
            .map_err(|error| format!("{identity}: write temporary manifest for {}: {error}", source.display()))?;
        temporary.persist(&path).map_err(|error| format!("{identity}: replace manifest {}: {}", source.display(), error.error))?;
        Ok(())
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
        insert_metadata_value(metadata, "annotations", LAST_APPLIED_HASH_ANNOTATION, &persisted_hash)?;
        apply_manifest_resource_document(&self.backend, &self.default_namespace, persisted).await.map_err(|error| error.to_string())?;
        Ok(())
    }
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
    if path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| extension.eq_ignore_ascii_case("json")) {
        return serde_json::from_str(&content).map(|document| vec![document]).map_err(|error| format!("parse JSON: {error}"));
    }

    let mut documents = Vec::new();
    for document in serde_yml::Deserializer::from_str(&content) {
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

fn stamp_manifest_metadata(document: &mut Value, source: &Path, hash: &str) -> Result<(), String> {
    let metadata =
        document.get_mut("metadata").and_then(Value::as_object_mut).ok_or_else(|| "missing or non-object metadata".to_string())?;
    insert_metadata_value(metadata, "labels", MANAGED_BY_LABEL, MANIFEST_MANAGED_BY_VALUE)?;
    insert_metadata_value(metadata, "annotations", MANIFEST_SOURCE_ANNOTATION, &source.to_string_lossy())?;
    insert_metadata_value(metadata, "annotations", LAST_APPLIED_HASH_ANNOTATION, hash)
}

fn stamp_manifest_ownership(document: &mut Value, source: &Path) -> Result<(), String> {
    let metadata =
        document.get_mut("metadata").and_then(Value::as_object_mut).ok_or_else(|| "missing or non-object metadata".to_string())?;
    insert_metadata_value(metadata, "labels", MANAGED_BY_LABEL, MANIFEST_MANAGED_BY_VALUE)?;
    insert_metadata_value(metadata, "annotations", MANIFEST_SOURCE_ANNOTATION, &source.to_string_lossy())
}

fn set_annotation(document: &mut Value, key: &str, value: &str) -> Result<(), String> {
    let metadata =
        document.get_mut("metadata").and_then(Value::as_object_mut).ok_or_else(|| "missing or non-object metadata".to_string())?;
    insert_metadata_value(metadata, "annotations", key, value)
}

fn clear_manifest_state(document: &mut Value) -> Result<(), String> {
    let annotations = document
        .get_mut("metadata")
        .and_then(|metadata| metadata.get_mut("annotations"))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "missing or non-object metadata.annotations".to_string())?;
    for key in REFUSAL_ANNOTATIONS.into_iter().chain([MANIFEST_RESOLUTION_ANNOTATION]) {
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
    use std::time::Duration;

    use flotilla_resources::{
        InMemoryBackend, InputMeta, PlacementPolicy, PlacementPolicySpec, ResourceBackend, WatchStart, WorkflowTemplate,
    };
    use futures::StreamExt;

    use super::*;

    const NAMESPACE: &str = "flotilla";

    fn write(path: &Path, content: &str) {
        std::fs::write(path, content).expect("write manifest");
    }

    fn manifest(name: &str, pool: &str) -> String {
        format!("apiVersion: flotilla.work/v1\nkind: PlacementPolicy\nmetadata:\n  name: {name}\nspec:\n  pool: {pool}\n")
    }

    fn manifest_with_priority(name: &str, pool: &str, priority: i32) -> String {
        format!(
            "apiVersion: flotilla.work/v1\nkind: PlacementPolicy\nmetadata:\n  name: {name}\nspec:\n  pool: {pool}\n  priority: {priority}\n"
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

        let report = reconciler.reconcile_once().await.expect("manifest pass");

        assert_eq!(report.created, 3);
        for (name, source) in [("alpha", "nested/policies.yaml"), ("gamma", "nested/policies.yaml"), ("beta", "beta.json")] {
            let object = backend.using::<PlacementPolicy>(NAMESPACE).get(name).await.expect("manifest object");
            assert_eq!(object.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str), Some(MANIFEST_MANAGED_BY_VALUE));
            assert_eq!(object.metadata.annotations.get(MANIFEST_SOURCE_ANNOTATION).map(String::as_str), Some(source));
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
    async fn steady_state_and_restart_perform_no_writes_or_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("policy.yaml"), &manifest("steady", "one"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut first = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());
        first.reconcile_once().await.expect("initial pass");
        let before = backend.using::<PlacementPolicy>(NAMESPACE).get("steady").await.expect("policy");

        let mut events = backend.using::<PlacementPolicy>(NAMESPACE).watch(WatchStart::Now).await.expect("watch");
        let mut restarted = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());
        let report = restarted.reconcile_once().await.expect("restart pass");
        let after = backend.using::<PlacementPolicy>(NAMESPACE).get("steady").await.expect("policy");

        assert_eq!(report.unchanged, 1);
        assert_eq!(before.metadata.resource_version, after.metadata.resource_version);
        assert!(tokio::time::timeout(Duration::from_millis(20), events.next()).await.is_err());
    }

    #[tokio::test]
    async fn loop_recovers_when_manifest_directory_appears_later() {
        let parent = tempfile::tempdir().expect("tempdir");
        let root = parent.path().join("not-created-yet");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
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
        reconciler.reconcile_once().await.expect("initial pass");
        let resolver = backend.using::<PlacementPolicy>(NAMESPACE);
        let applied = resolver.get("moving").await.expect("policy");
        let mut live_meta = InputMeta::from(&applied.metadata);
        live_meta.labels.insert("another-controller/observation".to_string(), "kept".to_string());
        resolver.update(&live_meta, &applied.metadata.resource_version, &applied.spec).await.expect("add external metadata");

        write(&path, &manifest_with_priority("moving", "two", 100));
        let report = reconciler.reconcile_once().await.expect("fast-forward pass");
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
        let settled = reconciler.reconcile_once().await.expect("settled pass");
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
        reconciler.reconcile_once().await.expect("initial pass");

        let steady = reconciler.reconcile_once().await.expect("steady pass");
        write(&path, &workflow("echo-two"));
        let updated = reconciler.reconcile_once().await.expect("fast-forward pass");
        let object = backend.using::<WorkflowTemplate>(NAMESPACE).get("defaults").await.expect("workflow");

        assert_eq!(steady.unchanged, 1);
        assert_eq!(steady.drifted, 0);
        assert_eq!(updated.updated, 1);
        assert_eq!(serde_json::to_value(&object.spec).expect("workflow spec")["vessels"][0]["crew"][0]["command"], "echo-two");
    }

    #[tokio::test]
    async fn live_drift_is_left_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("policy.yaml");
        write(&path, &manifest("drifted", "one"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());
        reconciler.reconcile_once().await.expect("initial pass");
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
        let report = reconciler.reconcile_once().await.expect("drift pass");
        let object = resolver.get("drifted").await.expect("policy");

        assert_eq!(report.drifted, 1);
        assert_eq!(object.spec.pool, "live-edit");
        assert_eq!(object.metadata.annotations.get(MANIFEST_REFUSAL_ANNOTATION).map(String::as_str), Some("drift"));
        assert!(object.metadata.annotations.contains_key(MANIFEST_LIVE_HASH_ANNOTATION));
        assert!(object.metadata.annotations.contains_key(MANIFEST_BASELINE_HASH_ANNOTATION));
        assert!(object.metadata.annotations.contains_key(MANIFEST_DESIRED_HASH_ANNOTATION));
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

        let report = reconciler.reconcile_once().await.expect("repair pass");
        let object = resolver.get("convergent").await.expect("policy");
        let digest = resource_document_spec_hash(&serde_json::to_value(object.to_k8s_object()).expect("document")).expect("digest");

        assert_eq!(report.unchanged, 1);
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

        assert_eq!(reconciler.reconcile_once().await.expect("creation pass").created, 1);
        let next = reconciler.reconcile_once().await.expect("verification pass");
        let object = backend.using::<PlacementPolicy>(NAMESPACE).get("fresh-baseline").await.expect("policy");
        let digest = resource_document_spec_hash(&serde_json::to_value(object.to_k8s_object()).expect("document")).expect("digest");

        assert_eq!(next.unchanged, 1);
        assert_eq!(next.drifted, 0);
        assert_eq!(object.metadata.annotations.get(LAST_APPLIED_HASH_ANNOTATION), Some(&digest));
    }

    #[tokio::test]
    async fn sync_resolution_makes_manifest_win_at_reconciler_seam() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("policy.yaml");
        write(&path, &manifest("sync-me", "manifest"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let resolver = backend.using::<PlacementPolicy>(NAMESPACE);
        let mut reconciler = ResourceManifestReconciler::new(backend, NAMESPACE, dir.path());
        reconciler.reconcile_once().await.expect("creation");
        let applied = resolver.get("sync-me").await.expect("policy");
        resolver
            .update(
                &InputMeta::from(&applied.metadata),
                &applied.metadata.resource_version,
                &PlacementPolicySpec::builder().pool("live".to_string()).build(),
            )
            .await
            .expect("live edit");
        reconciler.reconcile_once().await.expect("refusal");
        let refused = resolver.get("sync-me").await.expect("refused policy");
        let mut meta = InputMeta::from(&refused.metadata);
        meta.annotations.insert(MANIFEST_RESOLUTION_ANNOTATION.to_string(), "sync".to_string());
        resolver.update(&meta, &refused.metadata.resource_version, &refused.spec).await.expect("request sync");

        let report = reconciler.reconcile_once().await.expect("sync pass");
        let synced = resolver.get("sync-me").await.expect("synced policy");
        assert!(report.errors.is_empty(), "sync errors: {:?}", report.errors);
        assert_eq!(report.updated, 1, "report={report:?}, synced={synced:?}");
        assert_eq!(synced.spec.pool, "manifest");
        assert!(!synced.metadata.annotations.contains_key(MANIFEST_REFUSAL_ANNOTATION));
        assert!(!synced.metadata.annotations.contains_key(MANIFEST_RESOLUTION_ANNOTATION));
    }

    #[tokio::test]
    async fn adopt_resolution_writes_live_spec_to_manifest_at_reconciler_seam() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("policy.yaml");
        write(&path, &manifest("adopt-me", "manifest"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let resolver = backend.using::<PlacementPolicy>(NAMESPACE);
        let mut reconciler = ResourceManifestReconciler::new(backend, NAMESPACE, dir.path());
        reconciler.reconcile_once().await.expect("creation");
        let applied = resolver.get("adopt-me").await.expect("policy");
        resolver
            .update(
                &InputMeta::from(&applied.metadata),
                &applied.metadata.resource_version,
                &PlacementPolicySpec::builder().pool("live".to_string()).build(),
            )
            .await
            .expect("live edit");
        reconciler.reconcile_once().await.expect("refusal");
        let refused = resolver.get("adopt-me").await.expect("refused policy");
        let mut meta = InputMeta::from(&refused.metadata);
        meta.annotations.insert(MANIFEST_RESOLUTION_ANNOTATION.to_string(), "adopt".to_string());
        resolver.update(&meta, &refused.metadata.resource_version, &refused.spec).await.expect("request adopt");

        let report = reconciler.reconcile_once().await.expect("adopt pass");
        let rendered = std::fs::read_to_string(path).expect("updated manifest");
        assert_eq!(report.updated, 1);
        assert!(rendered.contains("pool: live"));
        assert_eq!(reconciler.reconcile_once().await.expect("settled pass").unchanged, 1);
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

        let report = reconciler.reconcile_once().await.expect("manifest pass");
        let object = backend.using::<PlacementPolicy>(NAMESPACE).get("unmanaged").await.expect("policy");

        assert_eq!(report.unmanaged, 1);
        assert_eq!(object.spec.pool, "live");
        assert!(!object.metadata.labels.contains_key(MANAGED_BY_LABEL));
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

        reconciler.reconcile_once().await.expect("initial unmanaged pass");
        assert!(reconciler.warned_unmanaged.contains(&identity));

        let object = resolver.get("unmanaged").await.expect("policy");
        let mut meta = InputMeta::from(&object.metadata);
        meta.labels.insert(MANAGED_BY_LABEL.to_string(), MANIFEST_MANAGED_BY_VALUE.to_string());
        resolver.update(&meta, &object.metadata.resource_version, &object.spec).await.expect("restore ownership");
        reconciler.reconcile_once().await.expect("managed pass");
        assert!(!reconciler.warned_unmanaged.contains(&identity));

        let object = resolver.get("unmanaged").await.expect("policy");
        let mut meta = InputMeta::from(&object.metadata);
        meta.labels.remove(MANAGED_BY_LABEL);
        resolver.update(&meta, &object.metadata.resource_version, &object.spec).await.expect("remove ownership");
        let report = reconciler.reconcile_once().await.expect("unmanaged again");

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

        let report = reconciler.reconcile_once().await.expect("manifest pass");

        assert_eq!(report.created, 1);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].path, PathBuf::from("broken.yaml"));
        assert!(report.errors[0].reason.contains("parse YAML"));
        backend.using::<PlacementPolicy>(NAMESPACE).get("valid").await.expect("valid policy");
    }

    #[tokio::test]
    async fn removing_a_manifest_does_not_prune_its_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("policy.yaml");
        write(&path, &manifest("retained", "one"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut reconciler = ResourceManifestReconciler::new(backend.clone(), NAMESPACE, dir.path());
        reconciler.reconcile_once().await.expect("initial pass");
        std::fs::remove_file(path).expect("remove manifest");

        reconciler.reconcile_once().await.expect("additive pass");

        backend.using::<PlacementPolicy>(NAMESPACE).get("retained").await.expect("object must not be pruned");
    }
}
