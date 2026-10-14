//! Test oracle for #2778: the clone-and-scan scope authorization this module
//! replaced, kept verbatim so property tests can compare refusals and output.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use flotilla_charter::{
    charter_store::{
        charter_image_layer_source, CharterSnapshot, CHARTER_IMAGE_LAYER_PATH, PROJECT_IMAGE_LAYER_PATH, PROJECT_IMAGE_MANAGER,
    },
    ops_entry::MATERIALIZED_PROJECT_ANNOTATION,
};
use flotilla_resources::{CharterPointer, CharterSource, FleetDesignation, Project, ProjectSpec, ResourceError, FLEET_DESIGNATION_NAME};
use flotilla_store::ResourceBackend;
use serde_json::{json, Value};

use super::{
    inherit_namespace, is_kind, name, namespace, parse_charter_file, CharterReader, Document, CHARTER_REVISION, CHARTER_SCOPE,
    CHARTER_SOURCE, MAX_REPOSITORY_FETCHES,
};
use crate::resource_manifest::LoadedManifestFile;

/// Return None when no registration pointer is declared, preserving the legacy
/// parser's per-document refusals and its original application behavior.
pub(super) async fn expand_registered_charters(
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
    for (path, parsed) in files {
        for value in parsed.as_ref().map_err(|error| format!("{}: {error}", path.display()))? {
            pending.push_back(
                Document::builder().path(path.clone()).value(value.clone()).source("fleet store".into()).revision(revision.into()).build(),
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
                    let mut candidate = original.clone();
                    for known in documents.iter().chain(pending.iter()).chain(std::iter::once(&document)) {
                        if is_kind(&known.value, "Project") && namespace(&known.value, default_namespace) == *scope_ns {
                            candidate.insert(
                                name(&known.value)?.into(),
                                serde_json::from_value(known.value["spec"].clone()).map_err(|error| error.to_string())?,
                            );
                        }
                    }
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
                    pending.push_back(
                        Document::builder()
                            .path(prefix.join(format!("inline-{index}.yaml")))
                            .value(value)
                            .maybe_scope(scope.clone())
                            .source(source.clone())
                            .revision(snapshot.revision.clone())
                            .build(),
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
                    pending.push_back(
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
                        pending.push_back(
                            Document::builder()
                                .path(prefix.join(format!("{path}#{index}")))
                                .value(value)
                                .maybe_scope(scope.clone())
                                .source(source.clone())
                                .revision(snapshot.revision.clone())
                                .build(),
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
                                    .all(|reference| reference.as_str().is_some_and(|repo| repository_in_scope(repo, &in_scope, catalog)))
                            })
                    }
                    "Repository" => repository_in_scope(object_name, &in_scope, catalog),
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
            validate_repository_targets(&document.value, kind, &in_scope, catalog)
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

fn belongs_to_scope(target: &str, scope: &str, catalog: &BTreeMap<String, ProjectSpec>, fleet: Option<&str>) -> bool {
    let mut current = target;
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert(current) {
            return false;
        }
        let Some(project) = catalog.get(current) else {
            return false;
        };
        if current == scope {
            return true;
        }
        match project.parent.as_deref().or_else(|| fleet.filter(|fleet| *fleet != current)) {
            Some(parent) => current = parent,
            None => return false,
        }
    }
}

fn repository_in_scope(repo: &str, in_scope: &impl Fn(&str) -> bool, catalog: &BTreeMap<String, ProjectSpec>) -> bool {
    catalog.iter().any(|(name, spec)| in_scope(name) && spec.repositories.iter().any(|member| member.repo.0 == repo))
}

fn validate_repository_targets(
    document: &Value,
    kind: &str,
    in_scope: &impl Fn(&str) -> bool,
    catalog: &BTreeMap<String, ProjectSpec>,
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
        if !reference.as_str().is_some_and(|repo| repository_in_scope(repo, in_scope, catalog)) {
            return Err(format!("{kind} targets out-of-scope repository {reference}"));
        }
    }
    Ok(())
}
