//! Expand opt-in charter registrations and validate the complete candidate before
//! the manifest reconciler writes anything. Legacy inputs do not enter this path.
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use flotilla_charter::{
    charter_store::{
        charter_image_layer_source, CharterSnapshot, LayerDirectorySnapshot, CHARTER_IMAGE_LAYER_PATH, PROJECT_IMAGE_LAYER_PATH,
        PROJECT_IMAGE_MANAGER,
    },
    ops_entry::{materialized_workflow_name, parse_operational_entry, OperationalEntryDefinition, MATERIALIZED_PROJECT_ANNOTATION},
};
use flotilla_resources::{
    CharterPointer, CharterSource, ConvoyEnsureSpec, FleetDesignation, Project, ProjectRepositoryRole, ProjectSpec, ResourceError,
    FLEET_DESIGNATION_NAME,
};
use flotilla_store::ResourceBackend;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::resource_manifest::{parse_document_contents, LoadedManifestFile};

// Declared in flotilla-resources for the definitions import (#3824 layer 2b); this path goes with
// the annotation writers when the definitions move onto the engine (layer 2b-ii).
pub(crate) use flotilla_resources::definition_provenance::{
    CHARTER_REVISION_ANNOTATION as CHARTER_REVISION, CHARTER_SCOPE_ANNOTATION as CHARTER_SCOPE, CHARTER_SOURCE_ANNOTATION as CHARTER_SOURCE,
};
const MAX_REPOSITORY_FETCHES: usize = 32;

#[async_trait]
pub(crate) trait CharterReader: Send + Sync {
    async fn read(&self, source: &CharterSource) -> Result<CharterSnapshot, String>;
    /// The charter's project image layer directory (ADR 0062), if present.
    async fn read_layer(&self, _source: &CharterSource) -> Result<Option<LayerDirectorySnapshot>, String> {
        Ok(None)
    }
}

pub(crate) struct BoundCharterReader<'a> {
    pub cache: &'a Path,
    pub vcs: Option<&'a dyn flotilla_vcs::vcs::Vcs>,
}

impl BoundCharterReader<'_> {
    /// Retire unused caches inside each per-source `charter-<digest>`
    /// directory, then remove the directory once it is empty. Other entries
    /// are never touched.
    pub(crate) async fn retire_unused(&self) -> Result<(), String> {
        let Some(vcs) = self.vcs else {
            return Ok(());
        };
        let mut entries = match tokio::fs::read_dir(self.cache).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("read {}: {error}", self.cache.display())),
        };
        while let Some(entry) = entries.next_entry().await.map_err(|error| format!("read {}: {error}", self.cache.display()))? {
            let per_source = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix("charter-"))
                .is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')));
            if !per_source || !entry.file_type().await.map_err(|error| error.to_string())?.is_dir() {
                continue;
            }
            flotilla_charter::charter_store::retire_unused_caches(&entry.path(), vcs).await?;
            // Fails harmlessly while a cache remains.
            let _ = tokio::fs::remove_dir(entry.path()).await;
        }
        Ok(())
    }
}

#[async_trait]
impl CharterReader for BoundCharterReader<'_> {
    async fn read(&self, source: &CharterSource) -> Result<CharterSnapshot, String> {
        let identity = serde_json::to_vec(source).map_err(|error| error.to_string())?;
        let cache = self.cache.join(format!("charter-{:x}", Sha256::digest(identity)));
        flotilla_charter::charter_store::read_charter_source(source, &cache, self.vcs).await
    }

    async fn read_layer(&self, source: &CharterSource) -> Result<Option<LayerDirectorySnapshot>, String> {
        let identity = serde_json::to_vec(source).map_err(|error| error.to_string())?;
        let cache = self.cache.join(format!("charter-{:x}", Sha256::digest(identity)));
        flotilla_charter::charter_store::read_layer_directory(source, &cache, self.vcs).await
    }
}

#[derive(Clone, bon::Builder)]
struct Document {
    path: PathBuf,
    value: Value,
    scope: Option<(String, String)>,
    source: String,
    revision: String,
}

fn is_kind(document: &Value, expected: &str) -> bool {
    document.get("kind").and_then(Value::as_str).and_then(|kind| flotilla_store::canonical_resource_kind(kind).ok()) == Some(expected)
}

fn namespace(document: &Value, default: &str) -> String {
    document.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or(default).to_string()
}

fn inherit_namespace(document: &mut Value, namespace: &str) -> Result<(), String> {
    let metadata = document.get_mut("metadata").and_then(Value::as_object_mut).ok_or("charter document metadata must be an object")?;
    metadata.entry("namespace").or_insert_with(|| json!(namespace));
    Ok(())
}

fn name(document: &Value) -> Result<&str, String> {
    document.pointer("/metadata/name").and_then(Value::as_str).ok_or_else(|| "charter document missing metadata.name".into())
}

/// Return None when no registration pointer is declared, preserving the legacy
/// parser's per-document refusals and its original application behavior.
pub(crate) async fn expand_registered_charters(
    files: &[LoadedManifestFile],
    revision: &str,
    default_namespace: &str,
    backend: &ResourceBackend,
    reader: &dyn CharterReader,
) -> Result<Option<Vec<LoadedManifestFile>>, String> {
    let active = files
        .iter()
        .filter_map(|(_, parsed)| parsed.as_ref().ok())
        .flatten()
        .any(|document| is_kind(document, "Project") && document.pointer("/spec/charter").is_some_and(|value| !value.is_null()));
    if !active {
        return Ok(None);
    }
    let mut pending = VecDeque::new();
    let mut frontier = Frontier::default();
    for (path, parsed) in files {
        for value in parsed.as_ref().map_err(|error| format!("{}: {error}", path.display()))? {
            frontier.enqueue(
                &mut pending,
                Document::builder().path(path.clone()).value(value.clone()).source("fleet store".into()).revision(revision.into()).build(),
                default_namespace,
            );
        }
    }
    let mut documents = Vec::<Document>::new();
    let mut repository_fetches = 0;
    let mut claims = BTreeMap::<(String, String), String>::new();
    let mut registration_fleets = BTreeMap::<String, String>::new();
    let mut registration_catalogs = BTreeMap::<String, BTreeMap<String, ProjectSpec>>::new();
    while let Some(mut document) = pending.pop_front() {
        let kind = document.value.get("kind").and_then(Value::as_str).ok_or("charter document missing kind")?;
        document.value["kind"] = json!(flotilla_store::canonical_resource_kind(kind).map_err(|error| error.to_string())?);
        // Finite input documents may embed recursive inline registrations or
        // reference a repository cycle. Duplicate Project claims stop cycles;
        // the budget also bounds adversarial chains of distinct Projects.
        if documents.len() + pending.len() > 10_000 {
            return Err("charter expansion exceeds 10000 documents".into());
        }
        if is_kind(&document.value, "Project") {
            let project = name(&document.value)?.to_string();
            let ns = namespace(&document.value, default_namespace);
            let claimant = format!("{} ({})", document.source, document.path.display());
            if let Some(previous) = claims.insert((ns.clone(), project.clone()), claimant.clone()) {
                return Err(format!("Project `{ns}/{project}` claimed by two sources: {previous} and {claimant}"));
            }
            let spec: ProjectSpec =
                serde_json::from_value(document.value["spec"].clone()).map_err(|error| format!("Project {project}: {error}"))?;
            if let Some(pointer) = &spec.charter {
                pointer.validate()?;
                if let Some((scope_ns, scope_project)) = &document.scope {
                    if ns != *scope_ns {
                        return Err(format!(
                            "{} exceeds delegated scope `{scope_ns}/{scope_project}` with Project `{ns}/{project}`",
                            document.source
                        ));
                    }
                    if !registration_catalogs.contains_key(scope_ns) {
                        let mut catalog = backend
                            .definitions::<Project>(scope_ns)
                            .list()
                            .await
                            .map_err(|error| error.to_string())?
                            .into_iter()
                            .map(|project| (project.metadata.name, project.spec))
                            .collect::<BTreeMap<_, _>>();
                        match backend.definitions::<FleetDesignation>(scope_ns).get(FLEET_DESIGNATION_NAME).await {
                            Ok(fleet) => {
                                registration_fleets.insert(scope_ns.clone(), fleet.spec.project);
                            }
                            Err(ResourceError::NotFound { .. }) => {}
                            Err(error) => return Err(error.to_string()),
                        }
                        // Root registrations are trusted; their candidate parents
                        // can legitimately replace the stored hierarchy.
                        for (_, parsed) in files {
                            for root in parsed.as_ref().map_err(Clone::clone)? {
                                if is_kind(root, "FleetDesignation") && namespace(root, default_namespace) == *scope_ns {
                                    if let Some(fleet) = root.pointer("/spec/project").and_then(Value::as_str) {
                                        registration_fleets.insert(scope_ns.clone(), fleet.into());
                                    }
                                }
                                if is_kind(root, "Project") && namespace(root, default_namespace) == *scope_ns {
                                    catalog.insert(
                                        name(root)?.into(),
                                        serde_json::from_value(root["spec"].clone()).map_err(|error| error.to_string())?,
                                    );
                                }
                            }
                        }
                        registration_catalogs.insert(scope_ns.clone(), catalog);
                    }
                    let original = &registration_catalogs[scope_ns];
                    // The candidate is the trusted catalog overlaid by every
                    // enqueued Project, with this document's spec on top.
                    if let Some(error) = frontier.malformed.get(scope_ns) {
                        return Err(error.clone());
                    }
                    let candidate = CandidateCatalog { current: (&project, &spec), frontier: frontier.projects.get(scope_ns), original };
                    let fleet = registration_fleets.get(scope_ns).map(String::as_str);
                    if !belongs_to_scope(&project, scope_project, &candidate, fleet)
                        || (original.contains_key(&project) && !belongs_to_scope(&project, scope_project, original, fleet))
                    {
                        return Err(format!(
                            "{} exceeds delegated scope `{scope_ns}/{scope_project}` with Project `{ns}/{project}`",
                            document.source
                        ));
                    }
                    // Authorize before following a nested repository pointer.
                    // An out-of-scope source must not cause an external fetch.
                }
                let layer_prefix = format!("{CHARTER_IMAGE_LAYER_PATH}/");
                let (snapshot, source, inline_documents, layer) = match pointer {
                    CharterPointer::Inline { documents, files } => (
                        CharterSnapshot { revision: document.revision.clone(), files: files.clone() },
                        format!("inline charter {ns}/{project} in {}", document.source),
                        documents.clone(),
                        {
                            let layer = files
                                .iter()
                                .filter_map(|(path, contents)| {
                                    path.strip_prefix(&layer_prefix).map(|path| (path.to_string(), contents.clone().into_bytes()))
                                })
                                .collect::<BTreeMap<_, _>>();
                            (!layer.is_empty()).then_some(layer)
                        },
                    ),
                    CharterPointer::Repository { repo, branch, path } => {
                        if repository_fetches == MAX_REPOSITORY_FETCHES {
                            return Err(format!("charter expansion exceeds {MAX_REPOSITORY_FETCHES} repository fetches"));
                        }
                        repository_fetches += 1;
                        let source = CharterSource::Repository { repo: repo.clone(), branch: branch.clone(), path: path.clone() };
                        let snapshot = flotilla_charter::charter_store::source_read(reader.read(&source))
                            .await
                            .map_err(|error| format!("charter scope `{ns}/{project}`: {error}"))?;
                        let layer = flotilla_charter::charter_store::source_read(reader.read_layer(&charter_image_layer_source(&source)))
                            .await
                            .map_err(|error| format!("charter scope `{ns}/{project}` image layer: {error}"))?
                            .map(|layer| layer.files);
                        (snapshot, format!("{repo}@{branch}:{path}"), Vec::new(), layer)
                    }
                };
                let scope = Some((ns.clone(), project.clone()));
                let prefix = document.path.with_extension("").join(format!("charter-{project}"));
                for (index, mut value) in inline_documents.into_iter().enumerate() {
                    inherit_namespace(&mut value, &ns)?;
                    frontier.enqueue(
                        &mut pending,
                        Document::builder()
                            .path(prefix.join(format!("inline-{index}.yaml")))
                            .value(value)
                            .maybe_scope(scope.clone())
                            .source(source.clone())
                            .revision(snapshot.revision.clone())
                            .build(),
                        default_namespace,
                    );
                }
                // A delegated charter's `image/` (its project's named ops
                // home) is the project's layer (ADR 0062), authored as a
                // project-source ImageLayer.
                if let Some(files) = layer {
                    let layer =
                        flotilla_resources::parse_layer_directory(&files, flotilla_resources::ImageLayerSource::Project, Some(&project))
                            .map_err(|error| format!("{source}/{CHARTER_IMAGE_LAYER_PATH}: charter scope `{ns}/{project}`: {error}"))?;
                    // One project, one layer source: the primary repository's
                    // `.flotilla/image/` as well is a within-source clash.
                    let primaries = backend
                        .definitions::<flotilla_resources::ImageLayer>(&ns)
                        .list_selected(flotilla_store::StoreQuery {
                            labels: BTreeMap::from([(flotilla_resources::MANAGED_BY_LABEL.to_string(), PROJECT_IMAGE_MANAGER.to_string())]),
                            ..Default::default()
                        })
                        .await
                        .map_err(|error| error.to_string())?;
                    for other in primaries {
                        let primary = other.metadata.deletion_timestamp.is_none()
                            && other.metadata.labels.get(flotilla_resources::MANAGED_BY_LABEL).map(String::as_str)
                                == Some(PROJECT_IMAGE_MANAGER)
                            && other.spec.current().is_some_and(|spec| spec.project.as_deref() == Some(project.as_str()));
                        if primary {
                            return Err(format!(
                                "project `{ns}/{project}` defines its image layer twice: `{PROJECT_IMAGE_LAYER_PATH}/` in its primary \
                                 repository (ImageLayer `{}`) and `{CHARTER_IMAGE_LAYER_PATH}/` in {source}",
                                other.metadata.name
                            ));
                        }
                    }
                    frontier.enqueue(
                        &mut pending,
                        Document::builder()
                            .path(prefix.join(CHARTER_IMAGE_LAYER_PATH))
                            .value(json!({
                                "apiVersion": "flotilla.work/v1", "kind": "ImageLayer",
                                "metadata": {"name": layer.resource_name(), "namespace": ns},
                                "spec": layer,
                            }))
                            .maybe_scope(scope.clone())
                            .source(source.clone())
                            .revision(snapshot.revision.clone())
                            .build(),
                        default_namespace,
                    );
                }
                for (path, contents) in snapshot.files {
                    if path.starts_with(&layer_prefix) {
                        continue;
                    }
                    let values = parse_charter_file(&path, &contents, &project, &spec)
                        .map_err(|error| format!("{source}/{path}: charter scope `{ns}/{project}`: {error}"))?;
                    for (index, mut value) in values.into_iter().enumerate() {
                        // A delegated source's omitted namespace inherits the registration.
                        inherit_namespace(&mut value, &ns)?;
                        frontier.enqueue(
                            &mut pending,
                            Document::builder()
                                .path(prefix.join(format!("{path}#{index}")))
                                .value(value)
                                .maybe_scope(scope.clone())
                                .source(source.clone())
                                .revision(snapshot.revision.clone())
                                .build(),
                            default_namespace,
                        );
                    }
                }
            }
        }
        documents.push(document);
    }

    // Resolve scope against a candidate catalog, never against an untrusted
    // annotation alone. New children must declare a chain to the delegated parent.
    for document in &documents {
        if let Some((scope_ns, project)) = &document.scope {
            let ns = namespace(&document.value, default_namespace);
            if ns != *scope_ns {
                return Err(format!(
                    "{} ({}) exceeds delegated scope `{scope_ns}/{project}` by authoring in namespace `{ns}`",
                    document.source,
                    document.path.display()
                ));
            }
        }
    }
    let namespaces = documents.iter().map(|document| namespace(&document.value, default_namespace)).collect::<BTreeSet<_>>();
    let mut catalogs = BTreeMap::<String, BTreeMap<String, ProjectSpec>>::new();
    let mut fleets = BTreeMap::new();
    for ns in namespaces {
        let projects = backend.definitions::<Project>(&ns).list().await.map_err(|error| error.to_string())?;
        catalogs.insert(ns.clone(), projects.into_iter().map(|project| (project.metadata.name, project.spec)).collect());
        match backend.definitions::<FleetDesignation>(&ns).get(FLEET_DESIGNATION_NAME).await {
            Ok(fleet) => {
                fleets.insert(ns, fleet.spec.project);
            }
            Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    for document in documents.iter().filter(|document| document.scope.is_none()) {
        let ns = namespace(&document.value, default_namespace);
        if is_kind(&document.value, "Project") {
            let spec = serde_json::from_value(document.value["spec"].clone()).map_err(|error| error.to_string())?;
            catalogs.entry(ns.clone()).or_default().insert(name(&document.value)?.into(), spec);
        }
        if is_kind(&document.value, "FleetDesignation") {
            if let Some(project) = document.value.pointer("/spec/project").and_then(Value::as_str) {
                fleets.insert(ns, project.into());
            }
        }
    }
    let prior_catalogs = catalogs.clone();
    for document in documents.iter().filter(|document| document.scope.is_some()) {
        if is_kind(&document.value, "Project") {
            let ns = namespace(&document.value, default_namespace);
            let spec = serde_json::from_value(document.value["spec"].clone()).map_err(|error| error.to_string())?;
            catalogs.entry(ns).or_default().insert(name(&document.value)?.into(), spec);
        }
    }
    for (ns, catalog) in &catalogs {
        flotilla_resources::ProjectHierarchy::new(
            catalog.iter().map(|(name, spec)| (name.clone(), spec.parent.clone())).collect(),
            fleets.get(ns).cloned(),
        )
        .map_err(|error| format!("charter namespace `{ns}`: {error}"))?;
    }
    // Index member repositories once; scope checks then visit only their owners.
    let owners = catalogs
        .iter()
        .map(|(ns, catalog)| {
            let mut owners = BTreeMap::<&str, Vec<&str>>::new();
            for (name, spec) in catalog {
                for member in &spec.repositories {
                    owners.entry(member.repo.0.as_str()).or_default().push(name.as_str());
                }
            }
            (ns.clone(), owners)
        })
        .collect::<BTreeMap<_, _>>();
    let mut identities = BTreeMap::new();
    let mut result = Vec::new();
    for mut document in documents {
        let ns = namespace(&document.value, default_namespace);
        let kind = document.value.get("kind").and_then(Value::as_str).unwrap_or("");
        let object_name = name(&document.value)?;
        let identity = (ns.clone(), kind.to_string(), object_name.to_string());
        let claimant = format!("{} ({})", document.source, document.path.display());
        if let Some(previous) = identities.insert(identity, claimant.clone()) {
            return Err(format!("duplicate {kind} `{ns}/{object_name}` from {previous} and {claimant}"));
        }
        if let Some((scope_ns, project)) = &document.scope {
            let catalog = &catalogs[scope_ns];
            let owners = &owners[scope_ns];
            let in_scope = |target: &str| belongs_to_scope(target, project, catalog, fleets.get(scope_ns).map(String::as_str));
            let allowed = ns == *scope_ns
                && match kind {
                    "Project" => {
                        in_scope(object_name)
                            && (!prior_catalogs[scope_ns].contains_key(object_name)
                                || belongs_to_scope(
                                    object_name,
                                    project,
                                    &prior_catalogs[scope_ns],
                                    fleets.get(scope_ns).map(String::as_str),
                                ))
                    }
                    "ConvoyEnsure" => document.value.pointer("/spec/project_ref").and_then(Value::as_str).is_some_and(in_scope),
                    "WorkflowTemplate" => {
                        document
                            .value
                            .pointer(&format!("/metadata/annotations/{}", MATERIALIZED_PROJECT_ANNOTATION.replace('/', "~1")))
                            .and_then(Value::as_str)
                            .is_some_and(in_scope)
                            && document.value.pointer("/spec/repository_refs").and_then(Value::as_array).is_some_and(|refs| {
                                refs.iter()
                                    .all(|reference| reference.as_str().is_some_and(|repo| repository_in_scope(repo, &in_scope, owners)))
                            })
                    }
                    "Repository" => repository_in_scope(object_name, &in_scope, owners),
                    // Only the delegated project's own project-source layer,
                    // under its conventional name.
                    "ImageLayer" => serde_json::from_value::<flotilla_resources::ImageLayerResourceSpec>(document.value["spec"].clone())
                        .ok()
                        .and_then(flotilla_resources::ImageLayerResourceSpec::into_current)
                        .is_some_and(|layer| {
                            layer.source == flotilla_resources::ImageLayerSource::Project
                                && layer.project.as_deref() == Some(project.as_str())
                                && layer.resource_name() == object_name
                        }),
                    _ => false,
                };
            if !allowed {
                return Err(format!(
                    "{} ({}) exceeds delegated scope `{scope_ns}/{project}` by authoring {kind} `{ns}/{object_name}`",
                    document.source,
                    document.path.display()
                ));
            }
            if kind == "WorkflowTemplate" {
                let owner = document.value["metadata"]["annotations"][MATERIALIZED_PROJECT_ANNOTATION]
                    .as_str()
                    .ok_or("workflow missing project owner")?;
                if !object_name.starts_with(&format!("{owner}--")) {
                    return Err(format!(
                        "{} exceeds delegated scope `{scope_ns}/{project}`: workflow `{object_name}` must use the `{owner}--` prefix",
                        document.source
                    ));
                }
            }
            validate_repository_targets(&document.value, kind, &in_scope, owners)
                .map_err(|error| format!("{} exceeds delegated scope `{scope_ns}/{project}`: {error}", document.source))?;
            let annotations = document.value["metadata"]
                .as_object_mut()
                .ok_or("missing metadata")?
                .entry("annotations")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or("annotations must be an object")?;
            annotations.insert(CHARTER_SOURCE.into(), json!(document.source));
            annotations.insert(CHARTER_REVISION.into(), json!(document.revision));
            annotations.insert(CHARTER_SCOPE.into(), json!(format!("{scope_ns}/{project}")));
        }
        flotilla_store::validate_resource_document(&document.value).map_err(|error| format!("{}: {error}", document.path.display()))?;
        result.push((document.path, Ok(vec![document.value])));
    }
    Ok(Some(result))
}

/// Parent lookups for scope authorization: `None` for an unknown Project,
/// otherwise its declared parent.
trait ProjectParents {
    fn parent(&self, name: &str) -> Option<Option<&str>>;
}

impl ProjectParents for BTreeMap<String, ProjectSpec> {
    fn parent(&self, name: &str) -> Option<Option<&str>> {
        self.get(name).map(|spec| spec.parent.as_deref())
    }
}

/// Every Project enqueued for expansion, parsed once, by namespace. Later
/// enqueues replace earlier ones, matching the FIFO overlay order.
#[derive(Default)]
struct Frontier {
    projects: BTreeMap<String, BTreeMap<String, ProjectSpec>>,
    /// The first malformed Project per namespace, refused by the next scope
    /// check there; its own pop refuses it with the Project name.
    malformed: BTreeMap<String, String>,
}

impl Frontier {
    fn enqueue(&mut self, pending: &mut VecDeque<Document>, document: Document, default_namespace: &str) {
        if is_kind(&document.value, "Project") {
            let ns = namespace(&document.value, default_namespace);
            let parsed = name(&document.value).and_then(|name| {
                Ok((name.to_string(), serde_json::from_value(document.value["spec"].clone()).map_err(|error| error.to_string())?))
            });
            match parsed {
                Ok((name, spec)) => {
                    self.projects.entry(ns).or_default().insert(name, spec);
                }
                Err(error) => {
                    self.malformed.entry(ns).or_insert(error);
                }
            }
        }
        pending.push_back(document);
    }
}

/// The candidate catalog for one nested pointer, layered without copying:
/// the pointer's own Project, then the frontier, then the trusted catalog.
struct CandidateCatalog<'a> {
    current: (&'a str, &'a ProjectSpec),
    frontier: Option<&'a BTreeMap<String, ProjectSpec>>,
    original: &'a BTreeMap<String, ProjectSpec>,
}

impl ProjectParents for CandidateCatalog<'_> {
    fn parent(&self, name: &str) -> Option<Option<&str>> {
        if name == self.current.0 {
            return Some(self.current.1.parent.as_deref());
        }
        self.frontier.and_then(|frontier| frontier.parent(name)).or_else(|| self.original.parent(name))
    }
}

fn belongs_to_scope<'a>(target: &'a str, scope: &str, catalog: &'a impl ProjectParents, fleet: Option<&'a str>) -> bool {
    let mut current = target;
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert(current) {
            return false;
        }
        let Some(parent) = catalog.parent(current) else {
            return false;
        };
        if current == scope {
            return true;
        }
        match parent.or_else(|| fleet.filter(|fleet| *fleet != current)) {
            Some(parent) => current = parent,
            None => return false,
        }
    }
}

fn repository_in_scope(repo: &str, in_scope: &impl Fn(&str) -> bool, owners: &BTreeMap<&str, Vec<&str>>) -> bool {
    owners.get(repo).is_some_and(|owners| owners.iter().any(|name| in_scope(name)))
}

fn validate_repository_targets(
    document: &Value,
    kind: &str,
    in_scope: &impl Fn(&str) -> bool,
    owners: &BTreeMap<&str, Vec<&str>>,
) -> Result<(), String> {
    let mut references = Vec::new();
    if kind == "ConvoyEnsure" {
        references.extend(document.pointer("/spec/repositories").and_then(Value::as_array).into_iter().flatten());
    }
    if kind == "WorkflowTemplate" {
        references.extend(document.pointer("/spec/repository_refs").and_then(Value::as_array).into_iter().flatten());
        for vessel in document.pointer("/spec/vessels").and_then(Value::as_array).into_iter().flatten() {
            references.extend(vessel.get("repository_refs").and_then(Value::as_array).into_iter().flatten());
        }
    }
    for reference in references {
        if !reference.as_str().is_some_and(|repo| repository_in_scope(repo, in_scope, owners)) {
            return Err(format!("{kind} targets out-of-scope repository {reference}"));
        }
    }
    Ok(())
}

fn parse_charter_file(path: &str, contents: &str, project: &str, spec: &ProjectSpec) -> Result<Vec<Value>, String> {
    match Path::new(path).extension().and_then(|ext| ext.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("yaml" | "yml" | "json") => parse_document_contents(Path::new(path), contents),
        Some("md" | "markdown") => {
            let Some(entry) = parse_operational_entry(contents)? else {
                return Ok(Vec::new());
            };
            let targets = match &entry.repos {
                Some(aliases) => aliases
                    .iter()
                    .map(|alias| {
                        spec.repositories
                            .iter()
                            .find(|member| member.alias.as_deref() == Some(alias) && member.roles.contains(&ProjectRepositoryRole::Code))
                            .map(|member| member.repo.clone())
                            .ok_or_else(|| format!("unknown code repository alias `{alias}`"))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                None => spec
                    .repositories
                    .iter()
                    .filter(|member| member.roles.contains(&ProjectRepositoryRole::Code))
                    .map(|member| member.repo.clone())
                    .collect(),
            };
            if targets.is_empty() {
                return Err("operational entry has no code-role repositories".into());
            }
            let annotations = json!({MATERIALIZED_PROJECT_ANNOTATION: project});
            match entry.definition {
                OperationalEntryDefinition::WorkflowTemplate(mut workflow) => {
                    workflow.repository_refs = Some(targets.clone());
                    for vessel in &mut workflow.vessels {
                        vessel.repository_refs = Some(targets.clone());
                    }
                    flotilla_resources::validate(&workflow)
                        .map_err(|errors| errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))?;
                    Ok(vec![
                        json!({"apiVersion": "flotilla.work/v1", "kind": "WorkflowTemplate", "metadata": {"name": materialized_workflow_name(project, &entry.name), "annotations": annotations}, "spec": workflow}),
                    ])
                }
                OperationalEntryDefinition::Ensure(ensure) => {
                    let digest = Sha256::digest(format!("{project}\0{}", entry.name).as_bytes());
                    let ensure_spec = ConvoyEnsureSpec::builder()
                        .project_ref(project.to_string())
                        .role(entry.name)
                        .workflow_ref(ensure.workflow)
                        .repositories(targets)
                        .maybe_driver_ref(ensure.driver)
                        .maybe_placement_policy(ensure.placement)
                        .maybe_escalation_reason(ensure.escalation_reason)
                        .maybe_presents_as(ensure.presents_as)
                        .agent_overrides(ensure.agent_overrides)
                        .build();
                    Ok(vec![
                        json!({"apiVersion": "flotilla.work/v1", "kind": "ConvoyEnsure", "metadata": {"name": format!("ensure-{digest:x}"), "annotations": annotations}, "spec": ensure_spec}),
                    ])
                }
                OperationalEntryDefinition::VerificationCommand { .. } => {
                    Err("registered charters declare verification commands on their Repository resource".into())
                }
            }
        }
        _ => {
            tracing::debug!(path, project, "ignoring charter file with unrecognized extension");
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod reference;

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use flotilla_resources::{ProjectRepositorySpec, RepositoryKey};
    use flotilla_store_memory::InMemoryBackend;
    use hegel::generators as gs;

    use super::*;

    // A fake stands in for fetching immutable Git objects through the VCS seam.
    struct MemoryReader {
        files: BTreeMap<String, String>,
        layer: Option<BTreeMap<String, Vec<u8>>>,
        revision: String,
        reads: Mutex<Vec<CharterSource>>,
        layer_reads: Mutex<Vec<CharterSource>>,
    }
    #[async_trait]
    impl CharterReader for MemoryReader {
        async fn read(&self, source: &CharterSource) -> Result<CharterSnapshot, String> {
            self.reads.lock().expect("reads").push(source.clone());
            Ok(CharterSnapshot { files: self.files.clone(), revision: self.revision.clone() })
        }
        async fn read_layer(&self, source: &CharterSource) -> Result<Option<LayerDirectorySnapshot>, String> {
            self.layer_reads.lock().expect("layer reads").push(source.clone());
            Ok(self.layer.clone().map(|files| LayerDirectorySnapshot { revision: self.revision.clone(), files }))
        }
    }
    fn reader(files: BTreeMap<String, String>) -> MemoryReader {
        MemoryReader { files, layer: None, revision: "commit-b".into(), reads: Mutex::default(), layer_reads: Mutex::default() }
    }
    fn backend() -> ResourceBackend {
        ResourceBackend::new(InMemoryBackend::default())
    }

    // #2777 review: the fetch budget is independent of the document budget,
    // and rejects the next fetch before contacting its source.
    #[tokio::test]
    async fn repository_fetch_budget_refuses_before_next_read() {
        let reader = reader(BTreeMap::new());
        let registrations = (0..=MAX_REPOSITORY_FETCHES)
            .map(|index| {
                project(
                    &format!("app-{index}"),
                    None,
                    Some(CharterPointer::Repository {
                        repo: format!("https://example.test/ops-{index}"),
                        branch: "main".into(),
                        path: String::new(),
                    }),
                )
            })
            .collect();
        let error = expand(registrations, &backend(), &reader).await.expect_err("fetch budget");
        assert!(error.contains("32 repository fetches"), "{error}");
        assert_eq!(reader.reads.lock().expect("reads").len(), MAX_REPOSITORY_FETCHES);
    }
    fn project(name: &str, parent: Option<&str>, charter: Option<CharterPointer>) -> Value {
        let spec = ProjectSpec::builder()
            .display_name(name.into())
            .default_workflow_ref("single-agent".into())
            .maybe_parent(parent.map(str::to_string))
            .maybe_charter(charter)
            .repositories(vec![ProjectRepositorySpec::builder()
                .repo(RepositoryKey(format!("repo-{name}")))
                .alias("code".into())
                .roles(BTreeSet::from([ProjectRepositoryRole::Code]))
                .build()])
            .build();
        json!({"apiVersion": "flotilla.work/v1", "kind": "Project", "metadata": {"name": name}, "spec": spec})
    }
    fn ensure(project: &str) -> Value {
        let spec = ConvoyEnsureSpec::builder()
            .project_ref(project.into())
            .role("governor".into())
            .workflow_ref("govern".into())
            .repositories(vec![RepositoryKey(format!("repo-{project}"))])
            .build();
        json!({"apiVersion": "flotilla.work/v1", "kind": "ConvoyEnsure", "metadata": {"name": format!("{project}-governor")}, "spec": spec})
    }
    fn inline(documents: Vec<Value>) -> CharterPointer {
        CharterPointer::Inline { documents, files: BTreeMap::new() }
    }
    fn inputs(documents: Vec<Value>) -> Vec<LoadedManifestFile> {
        vec![(PathBuf::from("registrations.yaml"), Ok(documents))]
    }
    async fn expand(
        documents: Vec<Value>,
        backend: &ResourceBackend,
        reader: &dyn CharterReader,
    ) -> Result<Vec<LoadedManifestFile>, String> {
        expand_registered_charters(&inputs(documents), "commit-a", "flotilla", backend, reader)
            .await
            .map(|result| result.expect("registered"))
    }

    fn layer_files(prefix: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                format!("{prefix}layer.yaml"),
                "name: app-native\nversion: 1.0.0\ntier: project\nprovides: [project:app]\nprobe: \"true\"\n".to_string(),
            ),
            (format!("{prefix}Dockerfile.fragment"), "FROM ${BASE}\nRUN true\n".to_string()),
        ])
    }

    fn layer_document(source: flotilla_resources::ImageLayerSource, project: Option<&str>) -> Value {
        let layer = flotilla_resources::ImageLayerSpec::builder()
            .name("native".into())
            .version("1.0.0".into())
            .tier(if project.is_some() { flotilla_resources::ImageLayerTier::Project } else { flotilla_resources::ImageLayerTier::Shared })
            .source(source)
            .maybe_project(project.map(str::to_string))
            .fragment("FROM ${BASE}\n".into())
            .probe("true".into())
            .build();
        json!({"apiVersion": "flotilla.work/v1", "kind": "ImageLayer", "metadata": {"name": layer.resource_name()}, "spec": layer})
    }

    // ADR 0062: a delegated charter's `.flotilla/image/`, inline or fetched,
    // becomes its own project's project-source ImageLayer; a charter can
    // author no other project's layer and no stdlib or installation layer.
    #[tokio::test]
    async fn delegated_charters_author_only_their_own_project_layer() {
        let layer = |files: &[LoadedManifestFile]| {
            files
                .iter()
                .flat_map(|(_, parsed)| parsed.as_ref().expect("parsed"))
                .find(|document| document["kind"] == "ImageLayer")
                .cloned()
                .expect("project layer document")
        };
        let inline = CharterPointer::Inline { documents: vec![], files: layer_files("image/") };
        let inline = layer(&expand(vec![project("app", None, Some(inline))], &backend(), &reader(BTreeMap::new())).await.expect("inline"));
        assert!(inline["metadata"]["name"].as_str().expect("name").starts_with("project-app-app-native-1.0.0-"));
        assert_eq!((inline["spec"]["source"].as_str(), inline["spec"]["project"].as_str()), (Some("project"), Some("app")));
        assert_eq!(inline["metadata"]["annotations"][CHARTER_SCOPE], "flotilla/app");

        let mut fetched = reader(BTreeMap::new());
        fetched.layer = Some(layer_files("").into_iter().map(|(path, contents)| (path, contents.into_bytes())).collect());
        let pointer =
            CharterPointer::Repository { repo: "https://example.test/app-ops".into(), branch: "main".into(), path: "charter".into() };
        let delegated = layer(&expand(vec![project("app", None, Some(pointer))], &backend(), &fetched).await.expect("delegated"));
        assert_eq!(delegated["spec"], inline["spec"]);
        assert_eq!(
            fetched.layer_reads.lock().expect("layer reads").last(),
            Some(&CharterSource::Repository {
                repo: "https://example.test/app-ops".into(),
                branch: "main".into(),
                path: "charter/image".into()
            })
        );

        for (document, kind) in [
            (layer_document(flotilla_resources::ImageLayerSource::Project, Some("other")), "another project's layer"),
            (layer_document(flotilla_resources::ImageLayerSource::Stdlib, None), "a stdlib layer"),
        ] {
            let error = expand(vec![project("app", None, Some(inline_pointer(document)))], &backend(), &reader(BTreeMap::new()))
                .await
                .expect_err(kind);
            assert!(error.contains("exceeds delegated scope `flotilla/app` by authoring ImageLayer"), "{kind}: {error}");
        }

        // The primary repository's layer as well is a within-source clash.
        let backend = backend();
        let primary = flotilla_resources::parse_layer_directory(
            &layer_files("").into_iter().map(|(path, contents)| (path, contents.into_bytes())).collect(),
            flotilla_resources::ImageLayerSource::Project,
            Some("app"),
        )
        .expect("primary layer");
        backend
            .definitions::<flotilla_resources::ImageLayer>("flotilla")
            .apply(
                &flotilla_resources::InputMeta::builder()
                    .name(primary.resource_name())
                    .labels(BTreeMap::from([(flotilla_resources::MANAGED_BY_LABEL.to_string(), PROJECT_IMAGE_MANAGER.to_string())]))
                    .build(),
                &primary.into(),
            )
            .await
            .expect("primary-repository layer");
        let inline = CharterPointer::Inline { documents: vec![], files: layer_files("image/") };
        let error = expand(vec![project("app", None, Some(inline))], &backend, &reader(BTreeMap::new())).await.expect_err("clash");
        assert!(error.contains("defines its image layer twice"), "{error}");
    }

    fn inline_pointer(document: Value) -> CharterPointer {
        inline(vec![document])
    }

    // #2721 / operator ruling: absent pointers leave legacy input handling inert,
    // including malformed documents that the old reconciler refuses individually.
    #[tokio::test]
    async fn absent_pointer_keeps_legacy_handling() {
        let files = vec![(PathBuf::from("legacy.yaml"), Err("invalid YAML".into()))];
        assert!(expand_registered_charters(&files, "legacy", "flotilla", &backend(), &reader(BTreeMap::new()))
            .await
            .expect("legacy")
            .is_none());
    }

    // #2721: a registration written with a supported dynamic kind alias must
    // activate the same checks; spelling cannot bypass delegated scope.
    #[tokio::test]
    async fn alias_registration_activates_scope_checks() {
        let mut registration = project("app", None, Some(inline(vec![ensure("other")])));
        registration["kind"] = json!("projects");
        let error = expand(vec![registration], &backend(), &reader(BTreeMap::new())).await.expect_err("alias scope refusal");
        assert!(error.contains("delegated scope `flotilla/app`"), "{error}");
    }

    // #2721: scoped sources cannot author fleet credentials, placement policies,
    // another Project's governor, foreign repository targets, or cross-namespace records.
    #[hegel::test]
    fn generated_out_of_scope_documents_name_the_scope(tc: hegel::TestCase) {
        // Span forbidden global kinds and project/namespace/reference escape routes.
        let variant = tc.draw(gs::integers::<usize>().min_value(0).max_value(8));
        let mut escaped = ensure("app");
        match variant {
            0 => {
                escaped["kind"] = json!("CredentialSpec");
            }
            1 => {
                escaped["kind"] = json!("PlacementPolicy");
            }
            2 => {
                escaped = ensure("other");
            }
            3 => {
                escaped["metadata"]["namespace"] = json!("other");
            }
            4 => {
                escaped["spec"]["repositories"] = json!(["repo-other"]);
            }
            5 => {
                escaped["kind"] = json!("Forge");
            }
            6 => {
                escaped["kind"] = json!("CrewDefaults");
            }
            7 => {
                escaped["kind"] = json!("placement-policy");
            }
            _ => {
                escaped["kind"] = json!("credential-spec");
            }
        }
        tokio::runtime::Runtime::new().expect("runtime").block_on(async {
            let backend = backend();
            let error = expand(
                vec![project("app", None, Some(inline(vec![escaped]))), project("other", None, None)],
                &backend,
                &reader(BTreeMap::new()),
            )
            .await
            .expect_err("scope refusal");
            assert!(error.contains("delegated scope `flotilla/app`"), "{error}");
            assert!(backend.using::<Project>("flotilla").list().await.expect("read").items.is_empty());
        });
    }

    // #2721: two sources claiming one Project are refused, naming both sources.
    #[hegel::test]
    fn duplicate_claims_name_both_sources(tc: hegel::TestCase) {
        let delegated = tc.draw(gs::booleans());
        tokio::runtime::Runtime::new().expect("runtime").block_on(async {
            let duplicate = project("app", None, None);
            let files = BTreeMap::from([("project.yaml".into(), serde_json::to_string(&duplicate).expect("encode"))]);
            let pointer = if delegated {
                CharterPointer::Repository { repo: "https://example.test/app-ops".into(), branch: "main".into(), path: "ops".into() }
            } else {
                inline(vec![duplicate])
            };
            let error = expand(vec![project("app", None, Some(pointer))], &backend(), &reader(files)).await.expect_err("duplicate claim");
            assert!(error.contains("claimed by two sources"), "{error}");
            assert!(error.contains("fleet store"), "{error}");
            assert!(
                error.contains(if delegated { "https://example.test/app-ops@main:ops" } else { "inline charter flotilla/app" }),
                "{error}"
            );
        });
    }

    // #2721: moving the same charter between inline and repository-backed inputs
    // produces the same resource specs, with the actual input revision as provenance.
    #[tokio::test]
    async fn inline_and_delegated_ops_have_equal_records() {
        let files = BTreeMap::from([
            ("ops/governor.ensure.md".into(), "---\nkind: ensure\nrole: governor\n---\nworkflow: govern\n".into()),
            ("ops/govern.md".into(), "---\nkind: workflow_template\nname: govern\n---\nvessels:\n  - name: work\n    crew: []\n".into()),
        ]);
        let reader = reader(files.clone());
        let inline_result =
            expand(vec![project("app", None, Some(CharterPointer::Inline { documents: vec![], files }))], &backend(), &reader)
                .await
                .expect("inline");
        assert!(reader.reads.lock().expect("reads").is_empty());
        let delegated_result = expand(
            vec![project(
                "app",
                None,
                Some(CharterPointer::Repository {
                    repo: "https://example.test/app-ops".into(),
                    branch: "reviewed".into(),
                    path: "charter".into(),
                }),
            )],
            &backend(),
            &reader,
        )
        .await
        .expect("delegated");
        assert_eq!(
            reader.reads.lock().expect("reads").as_slice(),
            &[CharterSource::Repository { repo: "https://example.test/app-ops".into(), branch: "reviewed".into(), path: "charter".into() }]
        );
        let records = |files: &[LoadedManifestFile]| {
            files
                .iter()
                .flat_map(|(_, parsed)| parsed.as_ref().expect("parsed"))
                .filter(|document| document["kind"] != "Project")
                .map(|document| (document["kind"].clone(), document["metadata"]["name"].clone(), document["spec"].clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(records(&inline_result), records(&delegated_result));
        for (_, parsed) in delegated_result.iter().skip(1) {
            let document = &parsed.as_ref().expect("parsed")[0];
            assert_eq!(document["metadata"]["annotations"][CHARTER_REVISION], "commit-b");
            assert_eq!(document["metadata"]["annotations"][CHARTER_SCOPE], "flotilla/app");
        }
    }

    // #2721: reject a scope-escaping registration before following its pointer,
    // and refuse malformed inline metadata without panicking or writing records.
    #[tokio::test]
    async fn refuses_scope_escape_before_fetch_and_handles_invalid_metadata() {
        let reader = reader(BTreeMap::new());
        let foreign = project(
            "foreign",
            None,
            Some(CharterPointer::Repository { repo: "https://example.test/foreign".into(), branch: "main".into(), path: String::new() }),
        );
        let error = expand(vec![project("app", None, Some(inline(vec![foreign])))], &backend(), &reader).await.expect_err("scope refusal");
        assert!(error.contains("delegated scope `flotilla/app`"), "{error}");
        assert!(reader.reads.lock().expect("reads").is_empty());
        let error =
            expand(vec![project("app", None, Some(inline(vec![json!({"kind": "ConvoyEnsure", "metadata": 42})])))], &backend(), &reader)
                .await
                .expect_err("malformed metadata refused");
        assert!(error.contains("metadata must be an object"), "{error}");
    }

    // Nested registrations over mostly fresh names, so expansion usually runs
    // deep; reused names give duplicates and reparenting, and shared
    // repositories, malformed Projects, escapes and fetched pointers mix in.
    struct Registrations<'a> {
        tc: &'a hegel::TestCase,
        names: Vec<String>,
    }

    impl Registrations<'_> {
        fn pick(&self, choices: usize) -> usize {
            self.tc.draw(gs::integers::<usize>().min_value(0).max_value(choices - 1))
        }

        fn known(&self) -> String {
            let index = self.pick(self.names.len() + 2);
            match index {
                0 => "app".into(),
                1 => "s0".into(),
                index => self.names[index - 2].clone(),
            }
        }

        fn name(&mut self) -> String {
            if !self.names.is_empty() && self.pick(5) == 0 {
                return self.known();
            }
            let name = format!("p{}", self.names.len());
            self.names.push(name.clone());
            name
        }

        fn parent(&self, scope: &str) -> Option<String> {
            match self.pick(4) {
                0 => None,
                1 => Some(scope.into()),
                _ => Some(self.known()),
            }
        }

        fn ensure(&self) -> Value {
            let mut document = ensure(&self.known());
            if self.pick(3) == 0 {
                document["spec"]["repositories"] = json!(["repo-shared"]);
            }
            document
        }

        fn project(&mut self, scope: &str, depth: usize) -> Value {
            let name = self.name();
            let parent = self.parent(scope);
            let charter = match self.pick(if depth < 3 { 4 } else { 1 }) {
                0 => None,
                1 => Some(CharterPointer::Repository {
                    repo: format!("https://example.test/{name}-ops"),
                    branch: "main".into(),
                    path: String::new(),
                }),
                _ => Some(inline((0..self.pick(4)).map(|_| self.charter_document(&name, depth)).collect())),
            };
            let mut document = project(&name, parent.as_deref(), charter);
            if self.pick(3) == 0 {
                document["spec"]["repositories"][0]["repo"] = json!("repo-shared");
            }
            document
        }

        fn charter_document(&mut self, scope: &str, depth: usize) -> Value {
            match self.pick(12) {
                0 => json!({"apiVersion": "flotilla.work/v1", "kind": "Project", "metadata": {"name": self.name()}, "spec": {"parent": 7}}),
                1 => json!({"apiVersion": "flotilla.work/v1", "kind": "Project", "metadata": {}, "spec": {}}),
                2 => {
                    let mut escaped = self.project(scope, 3);
                    escaped["metadata"]["namespace"] = json!("other");
                    escaped
                }
                3..=5 => self.ensure(),
                _ => self.project(scope, depth + 1),
            }
        }
    }

    // #2778: the incremental candidate catalog authorizes exactly as the
    // clone-and-scan catalog did: same records, or the same refusal text.
    #[hegel::test(test_cases = 400)]
    fn generated_authorization_matches_clone_and_scan_reference(tc: hegel::TestCase) {
        let mut registrations = Registrations { tc: &tc, names: Vec::new() };
        // Stored Projects may share generated names: reparenting them is theft.
        let stored = (0..registrations.pick(4))
            .map(|index| (["s0", "p0", "p1", "p2"][index].to_string(), registrations.parent("s0").filter(|parent| parent != "app")))
            .collect::<Vec<_>>();
        let stored_fleet = registrations.pick(2) == 0;
        let mut roots = vec![project(
            "app",
            None,
            Some(inline((0..registrations.pick(5) + 1).map(|_| registrations.charter_document("app", 0)).collect())),
        )];
        if registrations.pick(3) == 0 {
            roots.push(registrations.project("app", 0));
        }
        if registrations.pick(3) == 0 {
            let fleet = registrations.known();
            roots.push(json!({
                "apiVersion": "flotilla.work/v1", "kind": "FleetDesignation",
                "metadata": {"name": FLEET_DESIGNATION_NAME}, "spec": {"project": fleet},
            }));
        }
        tokio::runtime::Runtime::new().expect("runtime").block_on(async {
            let backend = backend();
            // The store refuses drawn hierarchies it cannot hold (cycles, a fleet
            // over an undeclared Project); those draws simply store less.
            for (name, parent) in &stored {
                let spec: ProjectSpec = serde_json::from_value(project(name, parent.as_deref(), None)["spec"].clone()).expect("spec");
                let meta = flotilla_resources::InputMeta::builder().name(name.clone()).build();
                let _ = backend.definitions::<Project>("flotilla").create(&meta, &spec).await;
            }
            if stored_fleet && !stored.is_empty() {
                let spec = flotilla_resources::FleetDesignationSpec { fleet: None, project: stored[0].0.clone(), image_cache: None };
                let meta = flotilla_resources::InputMeta::builder().name(FLEET_DESIGNATION_NAME.into()).build();
                let _ = backend.definitions::<FleetDesignation>("flotilla").create(&meta, &spec).await;
            }
            let files = BTreeMap::from([("child.yaml".into(), serde_json::to_string(&ensure("p1")).expect("encode"))]);
            let incremental =
                expand_registered_charters(&inputs(roots.clone()), "commit-a", "flotilla", &backend, &reader(files.clone())).await;
            let reference = reference::expand_registered_charters(&inputs(roots), "commit-a", "flotilla", &backend, &reader(files)).await;
            assert_eq!(incremental, reference);
        });
    }

    // #2778: shapes the generator rarely reaches. A duplicate sibling is a
    // claim conflict, not a scope escape (the pointer's own spec decides its
    // check); a later duplicate overrides earlier ones for other checks; and a
    // repository shared with a foreign Project stays in scope via its owner.
    #[tokio::test]
    async fn duplicate_names_and_shared_repositories_match_reference() {
        let shared = |mut document: Value| {
            document["spec"]["repositories"][0]["repo"] = json!("repo-shared");
            document
        };
        let mut shared_ensure = ensure("p1");
        shared_ensure["spec"]["repositories"] = json!(["repo-shared"]);
        let cases = [
            vec![project(
                "app",
                None,
                Some(inline(vec![project("x", Some("app"), Some(inline(vec![]))), project("x", Some("nowhere"), None)])),
            )],
            vec![project(
                "app",
                None,
                Some(inline(vec![
                    project("y", Some("app"), Some(inline(vec![project("z", Some("x"), Some(inline(vec![])))]))),
                    project("a", Some("app"), Some(inline(vec![project("x", Some("nowhere"), None)]))),
                    project("x", Some("y"), None),
                ])),
            )],
            vec![
                shared(project("a0", None, None)),
                project("app", None, Some(inline(vec![shared(project("p1", Some("app"), None)), shared_ensure]))),
            ],
        ];
        let mut outcomes = Vec::new();
        for roots in cases {
            let incremental =
                expand_registered_charters(&inputs(roots.clone()), "commit-a", "flotilla", &backend(), &reader(BTreeMap::new())).await;
            let reference =
                reference::expand_registered_charters(&inputs(roots), "commit-a", "flotilla", &backend(), &reader(BTreeMap::new())).await;
            assert_eq!(incremental, reference);
            outcomes.push(incremental.map(|_| ()));
        }
        assert!(outcomes[0].as_ref().expect_err("duplicate").contains("claimed by two sources"), "{outcomes:?}");
        assert!(
            outcomes[1].as_ref().expect_err("later duplicate").contains("delegated scope `flotilla/y` with Project `flotilla/z`"),
            "{outcomes:?}"
        );
        assert_eq!(outcomes[2], Ok(()));
    }

    // Delegated records only: the root registration echoes its input order.
    fn accepted_documents(output: &[LoadedManifestFile]) -> BTreeSet<String> {
        output
            .iter()
            .flat_map(|(_, parsed)| parsed.as_ref().expect("parsed"))
            .filter(|document| document["metadata"]["annotations"].get(CHARTER_SCOPE).is_some())
            .map(Value::to_string)
            .collect()
    }

    // #2778: a child may name a sibling registered later in the same charter as
    // its parent; sibling order does not change what is accepted.
    #[hegel::test]
    fn sibling_order_does_not_change_accepted_records(tc: hegel::TestCase) {
        let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
        // Parents are the scope or an earlier sibling, so the hierarchy is acyclic.
        let children = (0..count)
            .map(|index| {
                let parent = match tc.draw(gs::integers::<usize>().min_value(0).max_value(index)) {
                    0 => "app".to_string(),
                    sibling => format!("c{}", sibling - 1),
                };
                let name = format!("c{index}");
                let mut charter = vec![ensure(&name)];
                if tc.draw(gs::booleans()) {
                    let grandchild = format!("{name}-g");
                    charter.push(project(&grandchild, Some(name.as_str()), Some(inline(vec![ensure(&grandchild)]))));
                }
                project(&name, Some(parent.as_str()), Some(inline(charter)))
            })
            .collect::<Vec<_>>();
        let mut shuffled = children.clone();
        for index in (1..shuffled.len()).rev() {
            shuffled.swap(index, tc.draw(gs::integers::<usize>().min_value(0).max_value(index)));
        }
        tokio::runtime::Runtime::new().expect("runtime").block_on(async {
            let canonical = expand(vec![project("app", None, Some(inline(children)))], &backend(), &reader(BTreeMap::new()))
                .await
                .expect("canonical order");
            let shuffled = expand(vec![project("app", None, Some(inline(shuffled)))], &backend(), &reader(BTreeMap::new()))
                .await
                .expect("shuffled order");
            assert_eq!(accepted_documents(&canonical), accepted_documents(&shuffled));
        });
    }

    fn large_nested_charter(children: usize) -> Vec<Value> {
        let children = (0..children)
            .map(|index| {
                let name = format!("child-{index}");
                let mut charter = vec![ensure(&name)];
                if index % 2 == 0 {
                    let grandchild = format!("grandchild-{index}");
                    charter.push(project(&grandchild, Some(name.as_str()), Some(inline(vec![ensure(&grandchild)]))));
                }
                project(&name, Some("app"), Some(inline(charter)))
            })
            .collect();
        vec![project("app", None, Some(inline(children)))]
    }

    // #2778: a large nested charter (1,500 delegated scopes, 3,001 documents)
    // expands with every record scoped to its own registering Project.
    #[tokio::test]
    async fn large_nested_charter_expands_every_scope() {
        let output = expand(large_nested_charter(1_000), &backend(), &reader(BTreeMap::new())).await.expect("large charter");
        assert_eq!(output.len(), 3_001);
        let mut ensures = BTreeMap::new();
        for document in output.iter().flat_map(|(_, parsed)| parsed.as_ref().expect("parsed")) {
            if document["kind"] == "ConvoyEnsure" {
                let project = document["spec"]["project_ref"].as_str().expect("project").to_string();
                ensures.insert(project, document["metadata"]["annotations"][CHARTER_SCOPE].clone());
            }
        }
        assert_eq!(ensures.len(), 1_500);
        for (project, scope) in ensures {
            assert_eq!(scope, json!(format!("flotilla/{project}")));
        }
    }

    // #2721: a parent charter can register a new child and delegate its charter;
    // a source cannot enlarge its scope by reparenting an existing foreign Project.
    #[tokio::test]
    async fn nested_delegation_allows_children_and_refuses_stolen_project() {
        let child = project("child", Some("app"), Some(inline(vec![ensure("child")])));
        let output = expand(vec![project("app", None, Some(inline(vec![child])))], &backend(), &reader(BTreeMap::new()))
            .await
            .expect("child delegation");
        assert_eq!(output.len(), 3);
        let backend = backend();
        let foreign: ProjectSpec = serde_json::from_value(project("other", None, None)["spec"].clone()).expect("spec");
        backend
            .definitions::<Project>("flotilla")
            .create(&flotilla_resources::InputMeta::builder().name("other".into()).build(), &foreign)
            .await
            .expect("foreign Project");
        let stolen = project("other", Some("app"), None);
        let error = expand(vec![project("app", None, Some(inline(vec![stolen])))], &backend, &reader(BTreeMap::new()))
            .await
            .expect_err("cannot reparent foreign Project");
        assert!(error.contains("delegated scope `flotilla/app`"), "{error}");
    }
}
