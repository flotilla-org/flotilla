//! Expand opt-in charter registrations and validate the complete candidate before
//! the manifest reconciler writes anything. Legacy inputs do not enter this path.
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use flotilla_core::{
    charter_store::CharterSnapshot,
    ops_entry::{materialized_workflow_name, parse_operational_entry, OperationalEntryDefinition, MATERIALIZED_PROJECT_ANNOTATION},
};
use flotilla_resources::{
    CharterPointer, CharterSource, ConvoyEnsureSpec, FleetDesignation, Project, ProjectRepositoryRole, ProjectSpec, ResourceBackend,
    ResourceError, FLEET_DESIGNATION_NAME,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::resource_manifest::{parse_document_contents, LoadedManifestFile};

pub(crate) const CHARTER_SOURCE: &str = "flotilla.work/charter-source";
pub(crate) const CHARTER_REVISION: &str = "flotilla.work/charter-revision";
pub(crate) const CHARTER_SCOPE: &str = "flotilla.work/charter-scope";

#[async_trait]
pub(crate) trait CharterReader: Send + Sync {
    async fn read(&self, source: &CharterSource) -> Result<CharterSnapshot, String>;
}

pub(crate) struct BoundCharterReader<'a> {
    pub cache: &'a Path,
    pub vcs: Option<&'a dyn flotilla_core::vcs::Vcs>,
}

#[async_trait]
impl CharterReader for BoundCharterReader<'_> {
    async fn read(&self, source: &CharterSource) -> Result<CharterSnapshot, String> {
        let identity = serde_json::to_vec(source).map_err(|error| error.to_string())?;
        let cache = self.cache.join(format!("charter-{:x}", Sha256::digest(identity)));
        flotilla_core::charter_store::read_charter_source(source, &cache, self.vcs).await
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
    document.get("kind").and_then(Value::as_str).and_then(|kind| flotilla_resources::canonical_resource_kind(kind).ok()) == Some(expected)
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
    for (path, parsed) in files {
        for value in parsed.as_ref().map_err(|error| format!("{}: {error}", path.display()))? {
            pending.push_back(
                Document::builder().path(path.clone()).value(value.clone()).source("fleet store".into()).revision(revision.into()).build(),
            );
        }
    }
    let mut documents = Vec::<Document>::new();
    let mut claims = BTreeMap::<(String, String), String>::new();
    let mut registration_fleets = BTreeMap::<String, String>::new();
    let mut registration_catalogs = BTreeMap::<String, BTreeMap<String, ProjectSpec>>::new();
    while let Some(mut document) = pending.pop_front() {
        let kind = document.value.get("kind").and_then(Value::as_str).ok_or("charter document missing kind")?;
        document.value["kind"] = json!(flotilla_resources::canonical_resource_kind(kind).map_err(|error| error.to_string())?);
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
                let (snapshot, source, inline_documents) = match pointer {
                    CharterPointer::Inline { documents, files } => (
                        CharterSnapshot { revision: document.revision.clone(), files: files.clone() },
                        format!("inline charter {ns}/{project} in {}", document.source),
                        documents.clone(),
                    ),
                    CharterPointer::Repository { repo, branch, path } => {
                        let source = CharterSource::Repository { repo: repo.clone(), branch: branch.clone(), path: path.clone() };
                        (
                            reader.read(&source).await.map_err(|error| format!("charter scope `{ns}/{project}`: {error}"))?,
                            format!("{repo}@{branch}:{path}"),
                            Vec::new(),
                        )
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
                for (path, contents) in snapshot.files {
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
        flotilla_resources::validate_resource_document(&document.value).map_err(|error| format!("{}: {error}", document.path.display()))?;
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
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use flotilla_resources::{InMemoryBackend, ProjectRepositorySpec, RepositoryKey};
    use hegel::generators as gs;

    use super::*;

    // A fake stands in for fetching immutable Git objects through the VCS seam.
    struct MemoryReader {
        files: BTreeMap<String, String>,
        revision: String,
        reads: Mutex<Vec<CharterSource>>,
    }
    #[async_trait]
    impl CharterReader for MemoryReader {
        async fn read(&self, source: &CharterSource) -> Result<CharterSnapshot, String> {
            self.reads.lock().expect("reads").push(source.clone());
            Ok(CharterSnapshot { files: self.files.clone(), revision: self.revision.clone() })
        }
    }
    fn reader(files: BTreeMap<String, String>) -> MemoryReader {
        MemoryReader { files, revision: "commit-b".into(), reads: Mutex::default() }
    }
    fn backend() -> ResourceBackend {
        ResourceBackend::InMemory(InMemoryBackend::default())
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
        assert_eq!(reader.reads.lock().expect("reads").as_slice(), &[CharterSource::Repository {
            repo: "https://example.test/app-ops".into(),
            branch: "reviewed".into(),
            path: "charter".into()
        }]);
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
