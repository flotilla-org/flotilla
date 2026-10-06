//! Project registration, refresh, and operational entry materialization.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_protocol::{
    qualified_path::{HostId, QualifiedPath},
    PrincipalRef, ProviderData, ResourceRef,
};
use flotilla_resources::{
    apply_status_patch as apply_resource_status_patch, ensure_repository, normalize_project_spec, Clock, ConvoyEnsure, ConvoyEnsureSpec,
    ConvoyEnsureStatusPatch, ConvoyRepositorySpec, DeclarationRefusedCondition, Demand, DemandKind, DemandSpec, EventRecorder, Forge,
    InputMeta, ObjectEvent, Project, ProjectRepositoryRole, ProjectRepositorySpec, ProjectSpec, ProjectStatusPatch, Repository,
    RepositoryIdentity, RepositoryKey, RepositorySpec, ResourceBackend, ResourceError, ResourceObject, WorkflowTemplate,
    WorkflowTemplateSpec, WriterIdentity, MANAGED_BY_LABEL,
};
use tracing::{debug, warn};

use super::{
    convoy_ensure_name, ensure_repository_and_default_project_workflow, project_not_ready_error, repository_matches_target, InProcessDaemon,
};
use crate::{
    ops_entry::{
        parse_operational_entry, OperationalEntryDefinition, DECLARATION_REFUSAL_ATTENTION_PREFIX, DECLARATION_REFUSAL_REASON_ANNOTATION,
        DECLARATION_REFUSED_SINCE_ANNOTATION, MATERIALIZED_PROJECT_ANNOTATION, PRESENTS_AS_ANNOTATION, SOURCE_COMMIT_ANNOTATION,
        SOURCE_ENTRY_PATH_ANNOTATION, SOURCE_REPOSITORY_ANNOTATION, VERIFICATION_PROJECT_ANNOTATION, VERIFICATION_PROVENANCE_ANNOTATION,
    },
    project_declaration::{
        parse_project_declaration, ProjectDeclaration, BOOTSTRAP_COMMIT_ANNOTATION, BOOTSTRAP_PATH_ANNOTATION,
        BOOTSTRAP_REPOSITORY_ANNOTATION, DECLARATION_FILE, DECLARATION_FILE_ANNOTATION,
    },
    repository_inspection::{
        LocalCheckoutInspection, OperationalEntriesInspection, ProjectDeclarationInspection, RepositoryInspection, RepositoryInspector,
    },
};

/// A refresh refusal preserves file identity independently of its display text.
struct OperationalEntryRefusal {
    entry_path: Option<String>,
    message: String,
}

impl std::fmt::Display for OperationalEntryRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

pub(super) fn validate_project_name(name: &str) -> Result<(), String> {
    let normalized = normalize_project_name(name)?;
    if normalized != name {
        return Err(format!("project name `{name}` is invalid; use `{normalized}`"));
    }
    Ok(())
}

fn normalize_project_name(name: &str) -> Result<String, String> {
    let normalized = name
        .trim()
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() { character.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if normalized.is_empty() {
        return Err("project name must contain an alphanumeric character".to_string());
    }
    Ok(normalized)
}

fn whole_repository_project_spec(repository_key: RepositoryKey, display_name: String) -> Result<ProjectSpec, String> {
    normalize_project_spec(ProjectSpec {
        charter: None,
        parent: None,
        platform_matrix: Vec::new(),
        display_name,
        default_workflow_ref: "single-agent".to_string(),
        role_needs: BTreeMap::new(),
        skills: BTreeMap::new(),
        supervision: None,
        issue_source_bindings: Vec::new(),
        repositories: vec![ProjectRepositorySpec {
            charter_store: None,
            repo: repository_key,
            alias: None,
            roles: Default::default(),
            subpath: None,
            default_branch: None,
        }],
        dispatch_policy: None,
    })
}

const WHOLE_REPOSITORY_PROJECT_MANAGED_BY_VALUE: &str = "whole-repository-project";

fn whole_repository_project_meta(name: impl Into<String>) -> InputMeta {
    InputMeta::builder()
        .name(name.into())
        .labels(BTreeMap::from([(MANAGED_BY_LABEL.to_string(), WHOLE_REPOSITORY_PROJECT_MANAGED_BY_VALUE.to_string())]))
        .build()
}

/// Marks an existing whole-repository Project as generator-materialized without
/// changing its user-owned definition.
///
/// Materialization is intentionally one-way: it fills a missing Project, but a
/// later refresh must not reinterpret any part of an existing spec as
/// generator-owned. Explicit Project operations are the only way to refresh a
/// materialized definition.
async fn reconcile_whole_repository_project_definition(
    projects: &flotilla_resources::DefinitionResolver<Project>,
    existing: ResourceObject<Project>,
) -> Result<ResourceObject<Project>, String> {
    if is_declaration_backed_project(&existing) {
        return Ok(existing);
    }
    let managed_by_generator =
        existing.metadata.labels.get(MANAGED_BY_LABEL).is_some_and(|value| value == WHOLE_REPOSITORY_PROJECT_MANAGED_BY_VALUE);
    if managed_by_generator {
        return Ok(existing);
    }

    let mut meta = InputMeta::from(&existing.metadata);
    meta.labels.insert(MANAGED_BY_LABEL.to_string(), WHOLE_REPOSITORY_PROJECT_MANAGED_BY_VALUE.to_string());
    let reconciled = projects
        .update_metadata(&meta)
        .await
        .map_err(|error| format!("reconcile generated whole-repository Project {}: {error}", existing.metadata.name))?;
    Ok(reconciled)
}

pub(super) fn is_declaration_backed_project(project: &ResourceObject<Project>) -> bool {
    project.metadata.annotations.contains_key(BOOTSTRAP_REPOSITORY_ANNOTATION)
}

fn is_whole_repository_project(spec: &ProjectSpec, repository_key: &RepositoryKey) -> bool {
    matches!(
        spec.repositories.as_slice(),
        [entry] if &entry.repo == repository_key && entry.subpath.is_none()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectTargetSyntax {
    ExplicitPath,
    QualifiedSlug,
    Ambiguous,
}

fn project_target_syntax(target: &str) -> ProjectTargetSyntax {
    let path = Path::new(target);
    if path.is_absolute() || target.starts_with("./") || target.starts_with("../") {
        ProjectTargetSyntax::ExplicitPath
    } else if target.contains('/') {
        ProjectTargetSyntax::QualifiedSlug
    } else {
        ProjectTargetSyntax::Ambiguous
    }
}

/// Local execution paths come from Checkout facts, joined by Repository key.
pub(super) struct RepositoryIndex<'a> {
    pub(super) backend: &'a ResourceBackend,
    pub(super) observed: &'a ResourceBackend,
    pub(super) namespace: &'a std::sync::RwLock<String>,
    pub(super) host: &'a str,
}

enum CheckoutSelection {
    Unavailable,
    Ambiguous,
    Selected(LocalCheckoutInspection),
}

impl RepositoryIndex<'_> {
    // A sole local checkout is usable even if it is a worktree; with multiple
    // candidates, require a unique main checkout without re-inspecting identity.
    async fn select_checkout(&self, key: &RepositoryKey) -> Result<CheckoutSelection, String> {
        let namespace = self.namespace.read().expect("namespace lock poisoned").clone();
        let checkouts = crate::repository_addressing::local_checkouts(self.backend, self.observed, &namespace, self.host).await?;
        let mut candidates = checkouts.iter().filter(|checkout| checkout.spec.repo_ref() == key).collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(CheckoutSelection::Unavailable);
        }
        if candidates.len() > 1 {
            candidates.retain(|checkout| matches!(&checkout.spec, flotilla_resources::CheckoutSpec::Observed(spec) if spec.is_main));
        }
        match candidates.as_slice() {
            [checkout] => Ok(CheckoutSelection::Selected(
                LocalCheckoutInspection::builder()
                    .path(super::checkout_path(checkout).map(PathBuf::from).ok_or_else(|| format!("repository {key} has no local path"))?)
                    .host_ref(self.host.to_string())
                    .git_ref(checkout.spec.branch().to_string())
                    .is_main(matches!(&checkout.spec, flotilla_resources::CheckoutSpec::Observed(spec) if spec.is_main))
                    .build(),
            )),
            _ => Ok(CheckoutSelection::Ambiguous),
        }
    }

    async fn paths_for(&self, key: &RepositoryKey) -> Result<Vec<PathBuf>, String> {
        let namespace = self.namespace.read().expect("namespace lock poisoned").clone();
        Ok(crate::repository_addressing::local_checkouts(self.backend, self.observed, &namespace, self.host)
            .await?
            .into_iter()
            .filter(|checkout| checkout.spec.repo_ref() == key)
            .filter_map(|checkout| super::checkout_path(&checkout).map(PathBuf::from))
            .collect())
    }
}

/// Repository inspection and convoy teardown operations owned by their existing subsystems.
#[async_trait]
pub(super) trait ProjectOperations: Send + Sync {
    fn remember_bootstrap_checkout(&self, inspection: &RepositoryInspection) -> Result<(), String>;
    async fn repository_inspector(&self) -> Result<Arc<dyn RepositoryInspector>, String>;
    async fn inspect_repository_path(&self, path: &Path, remote: Option<&str>) -> Result<RepositoryInspection, String>;
    async fn resolve_repository_remote(&self, remote: &str) -> Result<RepositorySpec, String>;
    async fn resolve_forge_identity(&self, spec: RepositorySpec) -> Result<RepositorySpec, String>;
    async fn reap_ensured_convoy(&self, namespace: &str, ensure_name: &str, convoy_name: &str, force: bool) -> Result<(), String>;
}

#[async_trait]
impl ProjectOperations for InProcessDaemon {
    fn remember_bootstrap_checkout(&self, inspection: &RepositoryInspection) -> Result<(), String> {
        use crate::path_context::ExecutionEnvironmentPath;
        let path = ExecutionEnvironmentPath::new(&inspection.checkout.path);
        self.config.add_observation_root(&path)?;
        self.config.set_checkout_config(&path, inspection.spec.vcs().clone());
        Ok(())
    }

    async fn repository_inspector(&self) -> Result<Arc<dyn RepositoryInspector>, String> {
        InProcessDaemon::repository_inspector(self).await
    }

    async fn inspect_repository_path(&self, path: &Path, remote: Option<&str>) -> Result<RepositoryInspection, String> {
        InProcessDaemon::inspect_repository_path(self, path, remote).await
    }

    async fn resolve_repository_remote(&self, remote: &str) -> Result<RepositorySpec, String> {
        InProcessDaemon::resolve_repository_remote(self, remote).await
    }

    async fn resolve_forge_identity(&self, spec: RepositorySpec) -> Result<RepositorySpec, String> {
        InProcessDaemon::resolve_forge_identity(self, spec).await
    }

    async fn reap_ensured_convoy(&self, namespace: &str, ensure_name: &str, convoy_name: &str, force: bool) -> Result<(), String> {
        InProcessDaemon::reap_ensured_convoy(self, namespace, ensure_name, convoy_name, force).await
    }
}

pub(super) struct ProjectService<'a> {
    pub(super) resource_backend: &'a ResourceBackend,
    pub(super) observed_resource_backend: &'a ResourceBackend,
    pub(super) clock: &'a Arc<dyn Clock>,
    pub(super) namespace: &'a std::sync::RwLock<String>,
    pub(super) repository_index: RepositoryIndex<'a>,
    pub(super) operations: &'a dyn ProjectOperations,
}

impl ProjectService<'_> {
    fn provisioning_namespace(&self) -> String {
        self.namespace.read().expect("provisioning namespace lock poisoned").clone()
    }

    pub(super) async fn repository_transport_url(&self, namespace: &str, repository: &RepositorySpec) -> Result<String, String> {
        repository_transport_url_with_backend(self.resource_backend, namespace, repository).await
    }

    pub(super) async fn snapshot_project_repositories(
        &self,
        namespace: &str,
        project_ref: &str,
        selected: Option<&[RepositoryKey]>,
    ) -> Result<Vec<ConvoyRepositorySpec>, String> {
        snapshot_project_repositories_with_backend(self.resource_backend, namespace, project_ref, selected).await
    }

    pub(super) async fn project_register(&self, target: &str) -> Result<(String, usize), String> {
        let namespace = self.provisioning_namespace();
        let path = if Path::new(target).exists() {
            PathBuf::from(target)
        } else {
            let matches = self
                .resource_backend
                .clone()
                .using::<Repository>(&namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .filter(|repository| repository_matches_target(repository, target))
                .collect::<Vec<_>>();
            let [repository] = matches.as_slice() else {
                return Err(match matches.len() {
                    0 => format!("`{target}` is neither a bootstrap repository path nor a repository catalog slug"),
                    _ => format!("bootstrap repository slug `{target}` is ambiguous"),
                });
            };
            let key = RepositoryKey(repository.metadata.name.clone());
            let mut paths = self.repository_index.paths_for(&key).await?;
            paths.sort();
            match paths.as_slice() {
                [] => return Err(format!("bootstrap repository `{target}` has no local checkout on this host")),
                [path] => path.clone(),
                _ => {
                    let inspector = self.operations.repository_inspector().await?;
                    let mut main_paths = Vec::new();
                    for path in paths {
                        if inspector.inspect_path(&path, None).await?.checkout.is_main {
                            main_paths.push(path);
                        }
                    }
                    match main_paths.as_slice() {
                        [path] => path.clone(),
                        _ => {
                            return Err(format!(
                                "bootstrap repository `{target}` has multiple local checkouts; pass the intended checkout path explicitly"
                            ));
                        }
                    }
                }
            }
        };
        let inspection = self.operations.repository_inspector().await?.inspect_project_declaration(&path).await?;
        let declaration = parse_project_declaration(&inspection.yaml)?;
        let name = declaration.name.clone();
        self.materialize_project_declaration(declaration, inspection).await?;
        let project = self.resource_backend.clone().using::<Project>(&namespace).get(&name).await.map_err(|error| match error {
            ResourceError::NotFound { .. } => format!("project {name} is homed by another root; register it at its home"),
            error => error.to_string(),
        })?;
        Ok((name, project.spec.repositories.len()))
    }

    pub(super) async fn project_refresh(&self, name: &str) -> Result<(usize, bool, Vec<String>, Vec<String>), String> {
        validate_project_name(name)?;
        let namespace = self.provisioning_namespace();
        let project =
            self.resource_backend.clone().definitions::<Project>(&namespace).get(name).await.map_err(|error| error.to_string())?;
        let bootstrap_key = project
            .metadata
            .annotations
            .get(BOOTSTRAP_REPOSITORY_ANNOTATION)
            .ok_or_else(|| format!("project {name} was registered without a declaration"))?;
        // Definitions includes merged replicas; using reads only this root's
        // primary record, so a merged Project alone does not establish homing.
        match self.resource_backend.clone().using::<Project>(&namespace).get(name).await {
            Ok(_) => {}
            Err(ResourceError::NotFound { .. }) => {
                debug!(project = %name, "skipping project materialization away from its home");
                return Ok((project.spec.repositories.len(), false, Vec::new(), Vec::new()));
            }
            Err(error) => return Err(error.to_string()),
        }
        let bootstrap_path = match self.repository_index.select_checkout(&RepositoryKey(bootstrap_key.clone())).await? {
            CheckoutSelection::Selected(checkout) => checkout.path,
            CheckoutSelection::Unavailable => return Err(format!(
                "project {name} has no local bootstrap checkout on this host; re-register it with `flotilla project register /path/to/bootstrap`"
            )),
            CheckoutSelection::Ambiguous => return Err(format!(
                "project {name} bootstrap repository {bootstrap_key} has multiple local checkouts with no unique main checkout"
            )),
        };
        let inspection = self.operations.repository_inspector().await?.inspect_project_declaration(&bootstrap_path).await?;
        let declaration = parse_project_declaration(&inspection.yaml)?;
        if declaration.name != name {
            return Err(format!(
                "{} now declares project `{}` instead of `{name}`",
                bootstrap_path.join(DECLARATION_FILE).display(),
                declaration.name
            ));
        }
        let (changes, operational_entries) = self.materialize_project_declaration(declaration, inspection).await?;
        let members = self
            .resource_backend
            .clone()
            .definitions::<Project>(&namespace)
            .get(name)
            .await
            .map_err(|error| error.to_string())?
            .spec
            .repositories
            .len();
        Ok((members, !changes.is_empty(), changes, operational_entries))
    }

    pub(super) async fn materialize_project_declaration(
        &self,
        declaration: ProjectDeclaration,
        mut inspection: ProjectDeclarationInspection,
    ) -> Result<(Vec<String>, Vec<String>), String> {
        validate_project_name(&declaration.name)?;
        inspection.repository.spec = self.operations.resolve_forge_identity(inspection.repository.spec).await?;
        let namespace = self.provisioning_namespace();
        let authoring_lock = crate::charter_store::authoring_lock(&namespace);
        let authoring_guard = authoring_lock.lock().await;
        let projects = self.resource_backend.clone().definitions::<Project>(&namespace);
        let repositories = self.resource_backend.clone().using::<Repository>(&namespace);
        let existing_project = match projects.get(&declaration.name).await {
            Ok(project) => Some(project),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(error.to_string()),
        };
        if existing_project.as_ref().is_some_and(|project| project.spec.charter.is_some()) {
            debug!(project = %declaration.name, "leaving registered charter authoritative during legacy bootstrap refresh");
            return Ok((Vec::new(), vec!["project managed by registered charter".into()]));
        }
        let locally_homed = match self.resource_backend.clone().using::<Project>(&namespace).get(&declaration.name).await {
            Ok(_) => true,
            Err(ResourceError::NotFound { .. }) => false,
            Err(error) => return Err(error.to_string()),
        };
        if existing_project.is_some() && !locally_homed {
            debug!(project = %declaration.name, "skipping project materialization away from its home");
            return Ok((Vec::new(), Vec::new()));
        }
        let aliases = existing_project
            .as_ref()
            .into_iter()
            .flat_map(|project| &project.spec.repositories)
            .filter_map(|member| member.alias.as_ref().map(|alias| (alias.clone(), member.repo.clone())))
            .collect::<BTreeMap<_, _>>();
        // Adoption is intentional before materialization, like repo add: an
        // explicitly registered bootstrap remains discoverable after a later
        // member/ops refusal so the operator can repair it and retry.
        self.operations.remember_bootstrap_checkout(&inspection.repository)?;
        let bootstrap_key = inspection.repository.key();
        ensure_repository(&repositories, &bootstrap_key, &inspection.repository.spec).await.map_err(|error| error.to_string())?;
        self.reconcile_project_checkouts(&namespace, &bootstrap_key, &inspection.repository.spec, inspection.repository.checkout.clone())
            .await?;
        let bootstrap_inspection = inspection.clone();
        let provenance = BTreeMap::from([
            (BOOTSTRAP_REPOSITORY_ANNOTATION.to_string(), bootstrap_key.to_string()),
            (BOOTSTRAP_COMMIT_ANNOTATION.to_string(), inspection.commit.clone()),
            (DECLARATION_FILE_ANNOTATION.to_string(), DECLARATION_FILE.to_string()),
        ]);
        let mut converged = false;
        let mut members = Vec::with_capacity(declaration.members.len());
        for member in declaration.members {
            let declared_spec = self.operations.resolve_repository_remote(&member.url).await?;
            let key = match aliases.get(&member.alias) {
                Some(existing_key) => match repositories.get(&existing_key.to_string()).await {
                    Ok(existing)
                        if existing.spec.declares_remote(declared_spec.live_remote().expect("remote RepositorySpec has a live remote")) =>
                    {
                        existing_key.clone()
                    }
                    Ok(_) | Err(ResourceError::NotFound { .. }) => declared_spec.key(),
                    Err(error) => return Err(error.to_string()),
                },
                None => declared_spec.key(),
            };
            let mut repository = if key == declared_spec.key() {
                ensure_repository(&repositories, &key, &declared_spec).await.map_err(|error| error.to_string())?
            } else {
                repositories
                    .get(&key.to_string())
                    .await
                    .map_err(|error| format!("project member alias `{}` refers to unavailable repository {key}: {error}", member.alias))?
            };
            let live_remote = declared_spec.live_remote().expect("remote RepositorySpec has a live remote");
            let updated_spec = repository.spec.clone().update_remotes(live_remote)?;
            let mut repository_meta = InputMeta::from(&repository.metadata);
            for (annotation, value) in &provenance {
                if repository_meta.annotations.get(annotation) != Some(value) {
                    converged = true;
                    repository_meta.annotations.insert(annotation.clone(), value.clone());
                }
            }
            if repository_meta.annotations != repository.metadata.annotations || updated_spec != repository.spec {
                converged = true;
                repository = repositories
                    .update(&repository_meta, &repository.metadata.resource_version, &updated_spec)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            repository
                .spec
                .verify_key(&key)
                .map_err(|error| format!("project member alias `{}` resolved to invalid repository {key}: {error}", member.alias))?;
            members.push(ProjectRepositorySpec {
                charter_store: member.charter_store,
                repo: key,
                alias: Some(member.alias),
                roles: member.roles,
                subpath: None,
                default_branch: None,
            });
        }
        let spec = normalize_project_spec(ProjectSpec {
            charter: None,
            parent: declaration.parent.clone(),
            display_name: declaration.name.clone(),
            default_workflow_ref: declaration.default_workflow.unwrap_or_else(|| "single-agent".to_string()),
            role_needs: declaration.role_needs,
            skills: declaration.skills,
            platform_matrix: declaration.platform_matrix,
            supervision: existing_project.as_ref().and_then(|project| project.spec.supervision.clone()),
            issue_source_bindings: Vec::new(),
            repositories: members,
            dispatch_policy: existing_project.as_ref().and_then(|project| project.spec.dispatch_policy.clone()),
        })?;
        let mut meta = existing_project
            .as_ref()
            .map_or_else(|| InputMeta::builder().name(declaration.name.clone()).build(), |project| InputMeta::from(&project.metadata));
        for (annotation, value) in provenance {
            meta.annotations.insert(annotation, value);
        }
        // ADR 0047: accept old annotations, but never rewrite the retired path.
        // Remove this cleanup one fleet roll after #2484 ships.
        meta.annotations.remove(BOOTSTRAP_PATH_ANNOTATION);
        converged |=
            existing_project.as_ref().is_none_or(|project| project.spec != spec || project.metadata.annotations != meta.annotations);
        projects
            .apply_as(&WriterIdentity::operator().with_source("project-declaration"), &meta, &spec)
            .await
            .map_err(|error| error.to_string())?;
        let mut changes = if converged { vec![format!("Project/{}", declaration.name)] } else { Vec::new() };
        drop(authoring_guard);
        let (operational_changes, operational_entries) = match self
            .materialize_project_operational_entries(&declaration.name, Some(&bootstrap_inspection))
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                self.patch_project_operational_entries(&namespace, &declaration.name, false, error.entry_path.as_deref(), &error.message)
                    .await?;
                self.record_project_operational_refusal(&namespace, &declaration.name, &error.message).await;
                return Err(format!("operational entry refused: {error}"));
            }
        };
        changes.extend(operational_changes);
        Ok((changes, operational_entries))
    }

    pub(super) async fn record_project_operational_refusal(&self, namespace: &str, project_name: &str, error: &str) {
        let Ok(project) = self.resource_backend.definitions::<Project>(namespace).get(project_name).await else {
            return;
        };
        let reason = if error.contains("duplicate") { "DuplicateOperationalEntryRefused" } else { "ProjectOperationalEntriesRefused" };
        if let Err(record_error) = EventRecorder::new(self.resource_backend.clone())
            .record(ObjectEvent::for_object(&project, reason, error), self.clock.now())
            .await
        {
            warn!(project = %project_name, %record_error, "failed to record operational-entry refusal event");
        }
    }

    pub(super) async fn reconcile_bound_charters(&self) -> Result<(), String> {
        let namespace = self.provisioning_namespace();
        let projects = self.resource_backend.definitions::<Project>(&namespace).list().await.map_err(|error| error.to_string())?;
        let desired = projects
            .iter()
            .filter(|project| project.spec.charter.is_none())
            .flat_map(|project| {
                project.spec.repositories.iter().filter_map(|member| {
                    member
                        .charter_store
                        .as_ref()
                        .filter(|binding| binding.host == self.repository_index.host)
                        .map(|binding| crate::charter_store::ops_root_name(&project.metadata.name, member, &binding.host))
                })
            })
            .collect::<BTreeSet<_>>();
        let roots = self.resource_backend.using::<flotilla_resources::ManifestRoot>(&namespace);
        for root in roots.list().await.map_err(|error| error.to_string())?.items {
            if root.spec.host == self.repository_index.host
                && root.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str) == Some("ops-charter")
                && !desired.contains(&root.metadata.name)
            {
                // Removing a source retires its health, not the records it authored.
                roots.delete(&root.metadata.name).await.map_err(|error| error.to_string())?;
            }
        }
        for project in projects {
            if project.spec.charter.is_some() {
                self.patch_project_operational_entries(
                    &namespace,
                    &project.metadata.name,
                    true,
                    None,
                    "operational entries managed by registered charter",
                )
                .await?;
                continue;
            }
            if !project
                .spec
                .repositories
                .iter()
                .any(|member| member.charter_store.as_ref().is_some_and(|binding| binding.host == self.repository_index.host))
            {
                continue;
            }
            if let Err(error) = self.materialize_project_operational_entries(&project.metadata.name, None).await {
                self.patch_project_operational_entries(
                    &namespace,
                    &project.metadata.name,
                    false,
                    error.entry_path.as_deref(),
                    &error.message,
                )
                .await?;
                self.record_project_operational_refusal(&namespace, &project.metadata.name, &error.message).await;
            }
        }
        Ok(())
    }

    async fn materialize_project_operational_entries(
        &self,
        project_name: &str,
        bootstrap: Option<&ProjectDeclarationInspection>,
    ) -> Result<(Vec<String>, Vec<String>), OperationalEntryRefusal> {
        let mut entry_path = None;
        self.reconcile_operational_stores(project_name, bootstrap, &mut entry_path)
            .await
            .map_err(|message| OperationalEntryRefusal { entry_path, message })
    }

    async fn reconcile_operational_stores(
        &self,
        project_name: &str,
        bootstrap: Option<&ProjectDeclarationInspection>,
        entry_path: &mut Option<String>,
    ) -> Result<(Vec<String>, Vec<String>), String> {
        use flotilla_resources::{CharterSource, ManifestRoot, ManifestRootSpec};
        let namespace = self.provisioning_namespace();
        let lock = crate::charter_store::authoring_lock(&namespace);
        let _guard = lock.lock().await;
        let project =
            self.resource_backend.definitions::<Project>(&namespace).get(project_name).await.map_err(|error| error.to_string())?;
        if project.spec.charter.is_some() {
            // Registration delegates reconciliation to the fleet store. Do not
            // continue authoring or pruning through previous ops member bindings.
            return Ok((Vec::new(), vec!["operational entries managed by registered charter".into()]));
        }
        let roots = self.resource_backend.using::<ManifestRoot>(&namespace);
        let mut names = Vec::new();
        for member in &project.spec.repositories {
            let Some(binding) = &member.charter_store else {
                continue;
            };
            if binding.host != self.repository_index.host {
                continue;
            }
            let name = crate::charter_store::ops_root_name(project_name, member, &binding.host);
            let (source, path) = match &binding.source {
                CharterSource::Repository { repo, path, .. } => (repo.clone(), path.clone()),
                CharterSource::LocalDirectory { directory } => ("local".into(), directory.clone()),
            };
            let current = match roots.get(&name).await {
                Ok(root) => Some(root),
                Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error.to_string()),
            };
            let mut spec = current.as_ref().map(|root| root.spec.clone()).unwrap_or_else(|| {
                ManifestRootSpec::builder().host(binding.host.clone()).path(path.clone()).source(source.clone()).build()
            });
            spec.binding = Some(binding.source.clone());
            spec.host.clone_from(&binding.host);
            spec.path = path;
            spec.source = source;
            match current {
                Some(root) if root.spec != spec => {
                    roots
                        .update(&InputMeta::from(&root.metadata), &root.metadata.resource_version, &spec)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                None => {
                    roots
                        .create(
                            &InputMeta::builder()
                                .name(name.clone())
                                .labels(BTreeMap::from([(MANAGED_BY_LABEL.into(), "ops-charter".into())]))
                                .build(),
                            &spec,
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                }
                _ => {}
            }
            names.push(name);
        }
        let mut revisions = BTreeMap::new();
        let result = self.materialize_project_operational_entries_inner(project_name, bootstrap, entry_path, &mut revisions).await;
        for name in names {
            let publication_error = |error: ResourceError| match &result {
                Err(source_error) => format!("{source_error}; publish ManifestRoot/{name} status: {error}"),
                Ok(_) => format!("publish ManifestRoot/{name} status: {error}"),
            };
            let root = roots.get(&name).await.map_err(publication_error)?;
            let status = crate::charter_store::source_status(
                root.status.clone().unwrap_or_default(),
                revisions.remove(&name),
                result.as_ref().err().map(String::as_str),
                &name,
            );
            if root.status.as_ref() != Some(&status) {
                roots.update_status(&name, &root.metadata.resource_version, &status).await.map_err(publication_error)?;
            }
        }
        result
    }

    async fn materialize_project_operational_entries_inner(
        &self,
        project_name: &str,
        bootstrap: Option<&ProjectDeclarationInspection>,
        refused_entry_path: &mut Option<String>,
        bound_revisions: &mut BTreeMap<String, String>,
    ) -> Result<(Vec<String>, Vec<String>), String> {
        let namespace = self.provisioning_namespace();
        let project =
            self.resource_backend.clone().definitions::<Project>(&namespace).get(project_name).await.map_err(|e| e.to_string())?;
        let aliases = project
            .spec
            .repositories
            .iter()
            .filter_map(|member| member.alias.as_ref().map(|alias| (alias.clone(), member.repo.clone())))
            .collect::<BTreeMap<_, _>>();
        let all_code = project
            .spec
            .repositories
            .iter()
            .filter(|member| member.roles.contains(&ProjectRepositoryRole::Code))
            .map(|member| member.repo.clone())
            .collect::<Vec<_>>();
        let inspector = self.operations.repository_inspector().await?;
        let mut sources = Vec::new();
        let mut unavailable_source = false;
        for member in project.spec.repositories.iter().filter(|member| member.roles.contains(&ProjectRepositoryRole::Ops)) {
            if let Some(binding) = &member.charter_store {
                if binding.host != self.repository_index.host {
                    unavailable_source = true;
                    continue;
                }
                let snapshot = crate::charter_store::source_read(inspector.charter_snapshot(&binding.source)).await?;
                let spec = self
                    .resource_backend
                    .including_replicas::<Repository>(&namespace)
                    .get(&member.repo.to_string())
                    .await
                    .map_err(|error| error.to_string())?
                    .object
                    .spec;
                let source_root = crate::charter_store::ops_root_name(project_name, member, &binding.host);
                bound_revisions.insert(source_root.clone(), snapshot.revision.clone());
                let files =
                    snapshot.files.into_iter().map(|(path, contents)| crate::ops_entry::OperationalEntryFile { path, contents }).collect();
                sources.push(OperationalEntriesInspection {
                    source_root: Some(source_root),
                    repository: RepositoryInspection {
                        spec,
                        checkout: LocalCheckoutInspection::builder()
                            .path(PathBuf::from("/"))
                            .host_ref(binding.host.clone())
                            .git_ref(snapshot.revision.clone())
                            .is_main(true)
                            .build(),
                        transport_url: None,
                        replaces_prior_repository: false,
                    },
                    commit: snapshot.revision,
                    files,
                });
                continue;
            }
            // ADR 0047: remove incidental-checkout loading after one fleet roll
            // has migrated existing ops members to explicit charter bindings.
            let repository = if let Some(bootstrap) = bootstrap.filter(|bootstrap| member.repo == bootstrap.repository.key()) {
                bootstrap.repository.clone()
            } else {
                let checkout = match self.repository_index.select_checkout(&member.repo).await? {
                    CheckoutSelection::Selected(checkout) => checkout,
                    CheckoutSelection::Unavailable => {
                        unavailable_source = true;
                        continue;
                    }
                    CheckoutSelection::Ambiguous => {
                        return Err(format!(
                            "ops member {} has multiple local checkouts; its main checkout cannot be selected unambiguously",
                            member.alias.as_deref().unwrap_or(&member.repo.0)
                        ))
                    }
                };
                let spec = self
                    .resource_backend
                    .clone()
                    .using::<Repository>(&namespace)
                    .get(&member.repo.to_string())
                    .await
                    .map_err(|error| error.to_string())?
                    .spec;
                RepositoryInspection { spec, checkout, transport_url: None, replaces_prior_repository: false }
            };
            let (mut commit, files) =
                crate::charter_store::source_read(inspector.operational_entry_files_at(&repository.checkout.path)).await?;
            if let Some(bootstrap) = bootstrap.filter(|bootstrap| member.repo == bootstrap.repository.key()) {
                commit.clone_from(&bootstrap.commit);
            }
            let source = OperationalEntriesInspection { source_root: None, repository, commit, files };
            sources.push(source);
        }
        // Registration remains possible from a bootstrap repository that does
        // not host every ops member. Never infer an empty desired set from an
        // unavailable source: that could erase definitions materialized by a
        // previous refresh on a host that had the checkout.
        if unavailable_source {
            let message = "operational entries refused: an ops member has no local checkout on this host".to_string();
            self.patch_project_operational_entries(&namespace, project_name, false, None, &message).await?;
            if !bound_revisions.is_empty() {
                return Err(message);
            }
            return Ok((Vec::new(), vec![message]));
        }

        let mut workflows = BTreeMap::new();
        let mut ensures = BTreeMap::new();
        let mut commands = BTreeMap::<RepositoryKey, BTreeMap<String, String>>::new();
        let mut command_provenance = BTreeMap::<RepositoryKey, Vec<serde_json::Value>>::new();
        let mut outcomes = Vec::new();
        for source in sources {
            Self::collect_operational_entries(
                project_name,
                &aliases,
                &all_code,
                source,
                &mut workflows,
                &mut ensures,
                &mut commands,
                &mut command_provenance,
                &mut outcomes,
                refused_entry_path,
            )?;
        }

        let templates = self.resource_backend.clone().definitions::<WorkflowTemplate>(&namespace);
        let mut changes = Vec::new();
        for (name, (meta, spec)) in &workflows {
            *refused_entry_path = meta.annotations.get(SOURCE_ENTRY_PATH_ANNOTATION).cloned();
            let stored_name = crate::ops_entry::materialized_workflow_name(project_name, name);
            let mut stored_meta = meta.clone();
            stored_meta.name.clone_from(&stored_name);
            let current = match templates.get(&stored_name).await {
                Ok(current) => Some(current),
                Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error.to_string()),
            };
            if let Some(current) = &current {
                match current.metadata.annotations.get(MATERIALIZED_PROJECT_ANNOTATION) {
                    Some(owner) if owner == project_name => {}
                    Some(owner) => return Err(format!("WorkflowTemplate `{name}` is materialized by project `{owner}`")),
                    None => return Err(format!("WorkflowTemplate `{name}` already exists and is not materialized by a project")),
                }
            }
            if current.as_ref().is_none_or(|current| current.spec != *spec || current.metadata.annotations != meta.annotations) {
                templates.apply(&stored_meta, spec).await.map_err(|error| error.to_string())?;
                changes.push(format!("WorkflowTemplate/{name}"));
            }
        }
        *refused_entry_path = None;
        let desired_workflow_names =
            workflows.keys().map(|name| crate::ops_entry::materialized_workflow_name(project_name, name)).collect::<BTreeSet<_>>();
        let project_workflow_prefix = format!("{project_name}--");
        for stale in templates.list().await.map_err(|error| error.to_string())?.into_iter().filter(|template| {
            template.metadata.annotations.get(MATERIALIZED_PROJECT_ANNOTATION).map(String::as_str) == Some(project_name)
                && !desired_workflow_names.contains(&template.metadata.name)
        }) {
            templates.delete(&stale.metadata.name).await.map_err(|error| error.to_string())?;
            let logical_name = stale.metadata.name.strip_prefix(&project_workflow_prefix).unwrap_or(&stale.metadata.name);
            changes.push(format!("deleted WorkflowTemplate/{logical_name}"));
        }

        let convoy_ensures = self.resource_backend.clone().definitions::<ConvoyEnsure>(&namespace);
        for (name, (meta, spec)) in &ensures {
            *refused_entry_path = meta.annotations.get(SOURCE_ENTRY_PATH_ANNOTATION).cloned();
            let stored_workflow_ref = crate::ops_entry::materialized_workflow_name(project_name, &spec.workflow_ref);
            let workflow = match templates.get(&stored_workflow_ref).await {
                Ok(workflow) => Ok(workflow),
                Err(ResourceError::NotFound { .. }) => templates.get(&spec.workflow_ref).await,
                Err(error) => Err(error),
            }
            .map_err(|error| {
                format!(
                    "{} ensure `{name}` references workflow template {}: {error}",
                    meta.annotations.get(SOURCE_ENTRY_PATH_ANNOTATION).map(String::as_str).unwrap_or("operational entry"),
                    spec.workflow_ref
                )
            })?;
            if workflow.metadata.annotations.get(MATERIALIZED_PROJECT_ANNOTATION).is_some_and(|owner| owner != project_name) {
                return Err(format!(
                    "{} ensure `{name}` references workflow template {} materialized by project {}",
                    meta.annotations.get(SOURCE_ENTRY_PATH_ANNOTATION).map(String::as_str).unwrap_or("operational entry"),
                    spec.workflow_ref,
                    workflow.metadata.annotations[MATERIALIZED_PROJECT_ANNOTATION]
                ));
            }
            if workflow.spec.exit.is_some() {
                return Err(format!(
                    "{} ensure `{name}` references workflow template {} with an exit declaration",
                    meta.annotations.get(SOURCE_ENTRY_PATH_ANNOTATION).map(String::as_str).unwrap_or("operational entry"),
                    spec.workflow_ref
                ));
            }
            let current = match convoy_ensures.get(name).await {
                Ok(current) => Some(current),
                Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error.to_string()),
            };
            if let Some(current) = &current {
                match current.metadata.annotations.get(MATERIALIZED_PROJECT_ANNOTATION) {
                    Some(owner) if owner == project_name => {}
                    Some(owner) => return Err(format!("ConvoyEnsure `{name}` is materialized by project `{owner}`")),
                    None => return Err(format!("ConvoyEnsure `{name}` already exists and is not materialized by a project")),
                }
            }
            if current.as_ref().is_none_or(|current| current.spec != *spec || current.metadata.annotations != meta.annotations) {
                convoy_ensures.apply(meta, spec).await.map_err(|error| error.to_string())?;
                changes.push(format!("ConvoyEnsure/{name}"));
            }
        }
        *refused_entry_path = None;
        for stale in convoy_ensures.list().await.map_err(|error| error.to_string())?.into_iter().filter(|ensure| {
            ensure.metadata.annotations.get(MATERIALIZED_PROJECT_ANNOTATION).map(String::as_str) == Some(project_name)
                && !ensures.contains_key(&ensure.metadata.name)
        }) {
            if let Some(convoy_ref) = stale.status.as_ref().and_then(|status| status.convoy_ref.as_deref()) {
                self.operations.reap_ensured_convoy(&namespace, &stale.metadata.name, convoy_ref, false).await?;
            }
            convoy_ensures.delete(&stale.metadata.name).await.map_err(|error| error.to_string())?;
            changes.push(format!("deleted ConvoyEnsure/{}", stale.metadata.name));
        }

        let repositories = self.resource_backend.using::<Repository>(&namespace);
        let current_code_members = project
            .spec
            .repositories
            .iter()
            .filter(|member| member.roles.contains(&ProjectRepositoryRole::Code))
            .map(|member| member.repo.clone())
            .collect::<BTreeSet<_>>();
        for member in project.spec.repositories.iter().filter(|member| member.roles.contains(&ProjectRepositoryRole::Code)) {
            let current = match repositories.get(&member.repo.to_string()).await {
                Ok(current) => current,
                Err(ResourceError::NotFound { .. }) => {
                    let remote = self
                        .resource_backend
                        .including_replicas::<Repository>(&namespace)
                        .get(&member.repo.to_string())
                        .await
                        .map_err(|error| error.to_string())?
                        .object;
                    // Repository is a convergent fact, not a Definition. The
                    // source home authors its local record before adding ops data.
                    repositories.create(&InputMeta::from(&remote.metadata), &remote.spec).await.map_err(|error| error.to_string())?
                }
                Err(error) => return Err(error.to_string()),
            };
            let desired_commands = commands.remove(&member.repo).unwrap_or_default();
            let owner = current.metadata.annotations.get(VERIFICATION_PROJECT_ANNOTATION).map(String::as_str);
            match owner {
                Some(owner) if owner == project_name => {}
                Some(owner) if !desired_commands.is_empty() => {
                    return Err(format!("Repository {} verification commands are materialized by project `{owner}`", member.repo));
                }
                None if !desired_commands.is_empty() && !current.spec.verification_commands().is_empty() => {
                    return Err(format!(
                        "Repository {} already has verification commands that are not materialized by a project",
                        member.repo
                    ));
                }
                None if !desired_commands.is_empty() => {}
                _ => continue,
            }
            let desired_spec = current.spec.clone().with_verification_commands(desired_commands);
            let mut meta = InputMeta::from(&current.metadata);
            match command_provenance.remove(&member.repo) {
                Some(provenance) => {
                    meta.annotations.insert(VERIFICATION_PROJECT_ANNOTATION.to_string(), project_name.to_string());
                    meta.annotations.insert(
                        VERIFICATION_PROVENANCE_ANNOTATION.to_string(),
                        serde_json::to_string(&provenance).expect("JSON provenance values serialize"),
                    );
                }
                None => {
                    meta.annotations.remove(VERIFICATION_PROJECT_ANNOTATION);
                    meta.annotations.remove(VERIFICATION_PROVENANCE_ANNOTATION);
                }
            }
            if current.spec != desired_spec || current.metadata.annotations != meta.annotations {
                repositories.update(&meta, &current.metadata.resource_version, &desired_spec).await.map_err(|error| error.to_string())?;
                changes.push(format!("Repository/{} verification commands", member.repo));
            }
        }
        for stale in repositories.list().await.map_err(|error| error.to_string())?.items.into_iter().filter(|repository| {
            repository.metadata.annotations.get(VERIFICATION_PROJECT_ANNOTATION).map(String::as_str) == Some(project_name)
                && !current_code_members.contains(&RepositoryKey(repository.metadata.name.clone()))
        }) {
            let mut meta = InputMeta::from(&stale.metadata);
            meta.annotations.remove(VERIFICATION_PROJECT_ANNOTATION);
            meta.annotations.remove(VERIFICATION_PROVENANCE_ANNOTATION);
            let spec = stale.spec.clone().with_verification_commands(BTreeMap::new());
            repositories.update(&meta, &stale.metadata.resource_version, &spec).await.map_err(|error| error.to_string())?;
            changes.push(format!("Repository/{} verification commands", stale.metadata.name));
        }
        changes.sort();
        self.patch_project_operational_entries(&namespace, project_name, true, None, &outcomes.join("; ")).await?;
        Ok((changes, outcomes))
    }

    async fn patch_project_operational_entries(
        &self,
        namespace: &str,
        project_name: &str,
        ready: bool,
        entry_path: Option<&str>,
        message: &str,
    ) -> Result<(), String> {
        let projects = self.resource_backend.using::<Project>(namespace);
        let project = match projects.get(project_name).await {
            Ok(project) => project,
            // A bound ops store may live away from the Project's author. Its
            // own ManifestRoot carries health; never write another home's status.
            Err(ResourceError::NotFound { .. }) => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        let now = self.clock.now();
        let condition = (!ready).then(|| DeclarationRefusedCondition {
            // Source-level refusals have no entry; never manufacture a path from prose.
            entry_path: entry_path.unwrap_or_default().to_string(),
            message: message.to_string(),
            since: project.status.as_ref().and_then(|status| status.declaration_refused.as_ref()).map_or(now, |old| old.since),
            observed_at: now,
        });
        let mut errors = Vec::new();
        // Raise attention before fan-out, then attempt every condition patch.
        // One failed status write must not hide the refusal from the governor.
        let attention = async {
            let demands = self.resource_backend.using::<Demand>(namespace);
            let name = format!("{DECLARATION_REFUSAL_ATTENTION_PREFIX}{project_name}");
            if ready {
                return match demands.delete(&name).await {
                    Ok(()) | Err(ResourceError::NotFound { .. }) => Ok(()),
                    Err(error) => Err(error.to_string()),
                };
            }
            let target = ResourceRef::new("flotilla.work/v1", "Project", namespace, project_name);
            let spec =
                DemandSpec::for_dispatching_principal(target, DemandKind::HumanGate, PrincipalRef::implicit_for_namespace(namespace));
            let mut annotations = BTreeMap::from([(DECLARATION_REFUSAL_REASON_ANNOTATION.into(), message.into())]);
            if let Some(condition) = &condition {
                annotations.insert(DECLARATION_REFUSED_SINCE_ANNOTATION.into(), condition.since.to_rfc3339());
            }
            let meta = InputMeta::builder().name(name).annotations(annotations).build();
            match demands.create(&meta, &spec).await {
                Ok(_) => Ok(()),
                Err(ResourceError::Conflict { .. }) => {
                    let current = demands.get(&meta.name).await.map_err(|error| error.to_string())?;
                    demands.update(&meta, &current.metadata.resource_version, &spec).await.map(|_| ()).map_err(|error| error.to_string())
                }
                Err(error) => Err(error.to_string()),
            }
        }
        .await;
        if let Err(error) = attention {
            errors.push(error);
        }
        for patch in [
            ProjectStatusPatch::ReplaceOperationalEntries { ready, message: message.to_string() },
            ProjectStatusPatch::DeclarationRefused { condition: condition.clone() },
        ] {
            if let Err(error) = apply_resource_status_patch(&projects, project_name, &patch).await {
                errors.push(error.to_string());
            }
        }
        match self.resource_backend.definitions::<ConvoyEnsure>(namespace).list().await {
            Ok(ensures) => {
                for ensure in ensures.into_iter().filter(|ensure| ensure.spec.project_ref == project_name) {
                    if let Err(error) = apply_resource_status_patch(
                        &self.resource_backend.using::<ConvoyEnsure>(namespace),
                        &ensure.metadata.name,
                        &ConvoyEnsureStatusPatch::DeclarationRefused { condition: condition.clone() },
                    )
                    .await
                    {
                        errors.push(format!("ConvoyEnsure/{}: {error}", ensure.metadata.name));
                    }
                }
            }
            Err(error) => errors.push(error.to_string()),
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_operational_entries(
        project_name: &str,
        aliases: &BTreeMap<String, RepositoryKey>,
        all_code: &[RepositoryKey],
        source: OperationalEntriesInspection,
        workflows: &mut BTreeMap<String, (InputMeta, WorkflowTemplateSpec)>,
        ensures: &mut BTreeMap<String, (InputMeta, ConvoyEnsureSpec)>,
        commands: &mut BTreeMap<RepositoryKey, BTreeMap<String, String>>,
        command_provenance: &mut BTreeMap<RepositoryKey, Vec<serde_json::Value>>,
        outcomes: &mut Vec<String>,
        refused_entry_path: &mut Option<String>,
    ) -> Result<(), String> {
        let source_repository = source.repository.key();
        for file in source.files {
            *refused_entry_path = Some(file.path.clone());
            let Some(entry) = parse_operational_entry(&file.contents).map_err(|error| format!("{}: {error}", file.path))? else {
                *refused_entry_path = None;
                continue;
            };
            let requires_code_role = matches!(&entry.definition, OperationalEntryDefinition::VerificationCommand { .. });
            let targets = match entry.repos {
                Some(repo_aliases) => repo_aliases
                    .into_iter()
                    .map(|alias| {
                        let target = aliases
                            .get(&alias)
                            .cloned()
                            .ok_or_else(|| format!("{} names unknown repository alias `{alias}`", file.path))?;
                        if requires_code_role && !all_code.contains(&target) {
                            return Err(format!(
                                "{} operational entry `{}` targets repository alias `{alias}` without the code role",
                                file.path, entry.name
                            ));
                        }
                        Ok(target)
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                None => all_code.to_vec(),
            };
            if targets.is_empty() {
                return Err(format!("{} has no code-role repositories to target", file.path));
            }
            let mut provenance = serde_json::json!({
                "sourceRepository": source_repository,
                "sourceCommit": source.commit,
                "entryPath": file.path,
            });
            if let Some(root) = &source.source_root {
                provenance["manifestRoot"] = serde_json::json!(root);
            }
            match entry.definition {
                OperationalEntryDefinition::WorkflowTemplate(mut spec) => {
                    outcomes.push(format!("{}: WorkflowTemplate/{} accepted", file.path, entry.name));
                    spec.repository_refs = Some(targets.clone());
                    for vessel in &mut spec.vessels {
                        vessel.repository_refs = Some(targets.clone());
                    }
                    flotilla_resources::validate(&spec).map_err(|errors| {
                        let errors = errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ");
                        format!("{} contains invalid workflow template `{}`: {errors}", file.path, entry.name)
                    })?;
                    let meta = InputMeta::builder()
                        .name(entry.name.clone())
                        .annotations(
                            BTreeMap::from([
                                (MATERIALIZED_PROJECT_ANNOTATION.to_string(), project_name.to_string()),
                                (SOURCE_REPOSITORY_ANNOTATION.to_string(), source_repository.to_string()),
                                (SOURCE_COMMIT_ANNOTATION.to_string(), source.commit.clone()),
                                (SOURCE_ENTRY_PATH_ANNOTATION.to_string(), file.path.clone()),
                            ])
                            .into_iter()
                            .chain(source.source_root.as_ref().map(|root| ("flotilla.work/manifest-reconciler-root".into(), root.clone())))
                            .collect(),
                        )
                        .build();
                    if workflows.insert(entry.name.clone(), (meta, *spec)).is_some() {
                        return Err(format!("duplicate materialized WorkflowTemplate `{}`", entry.name));
                    }
                }
                OperationalEntryDefinition::VerificationCommand { command } => {
                    outcomes.push(format!("{}: verification command `{}` accepted", file.path, entry.name));
                    for target in targets {
                        if commands.entry(target.clone()).or_default().insert(entry.name.clone(), command.clone()).is_some() {
                            return Err(format!("duplicate verification command `{}` for repository {target}", entry.name));
                        }
                        command_provenance.entry(target).or_default().push(provenance.clone());
                    }
                }
                OperationalEntryDefinition::Ensure(ensure) => {
                    let role = entry.name;
                    let ensure_name = convoy_ensure_name(project_name, &role);
                    outcomes.push(format!("{}: ConvoyEnsure/{} accepted", file.path, ensure_name));
                    let mut meta = InputMeta::builder()
                        .name(ensure_name.clone())
                        .annotations(
                            BTreeMap::from([
                                (MATERIALIZED_PROJECT_ANNOTATION.to_string(), project_name.to_string()),
                                (SOURCE_REPOSITORY_ANNOTATION.to_string(), source_repository.to_string()),
                                (SOURCE_COMMIT_ANNOTATION.to_string(), source.commit.clone()),
                                (SOURCE_ENTRY_PATH_ANNOTATION.to_string(), file.path.clone()),
                            ])
                            .into_iter()
                            .chain(source.source_root.as_ref().map(|root| ("flotilla.work/manifest-reconciler-root".into(), root.clone())))
                            .collect(),
                        )
                        .build();
                    if let Some(presents_as) = &ensure.presents_as {
                        meta.annotations.insert(PRESENTS_AS_ANNOTATION.to_string(), presents_as.clone());
                    }
                    let spec = ConvoyEnsureSpec {
                        project_ref: project_name.to_string(),
                        role: role.clone(),
                        driver_ref: ensure.driver,
                        workflow_ref: ensure.workflow,
                        placement_policy: ensure.placement,
                        escalation_reason: ensure.escalation_reason,
                        repositories: targets,
                        presents_as: ensure.presents_as,
                        agent_overrides: ensure.agent_overrides,
                    };
                    if ensures.insert(ensure_name, (meta, spec)).is_some() {
                        return Err(format!("duplicate standing convoy role `{role}` in project `{project_name}`"));
                    }
                }
            }
        }
        *refused_entry_path = None;
        Ok(())
    }

    pub(super) async fn project_add(
        &self,
        target: &str,
        explicit_name: Option<&str>,
        explicit_display_name: Option<&str>,
        remote: Option<&str>,
    ) -> Result<String, String> {
        let namespace = self.provisioning_namespace();
        let repositories = self.resource_backend.clone().using::<Repository>(&namespace);
        let target_path = Path::new(target);
        let target_syntax = project_target_syntax(target);
        let path_is_explicit = target_syntax == ProjectTargetSyntax::ExplicitPath;
        let qualified_slug = target_syntax == ProjectTargetSyntax::QualifiedSlug;
        let path_candidate = if !qualified_slug && target_path.exists() {
            Some(self.operations.inspect_repository_path(target_path, remote).await?)
        } else if path_is_explicit {
            return Err(format!("repository path {} does not exist", target_path.display()));
        } else {
            None
        };

        let catalog_matches = if path_is_explicit {
            Vec::new()
        } else {
            repositories
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .filter(|repository| repository_matches_target(repository, target))
                .collect::<Vec<_>>()
        };
        let mut catalog_by_key = BTreeMap::new();
        for repository in catalog_matches {
            let key = RepositoryKey(repository.metadata.name.clone());
            repository.spec.verify_key(&key)?;
            catalog_by_key.insert(key, repository.spec);
        }
        if catalog_by_key.len() > 1 {
            return Err(format!(
                "repository slug `{target}` is ambiguous: {}",
                catalog_by_key.keys().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            ));
        }
        let catalog_candidate = catalog_by_key.into_iter().next();
        if remote.is_some() && path_candidate.is_none() && catalog_candidate.is_some() {
            return Err("--remote can only select identity while inspecting a local repository path".to_string());
        }

        let (key, repository_spec, checkout) = match (path_candidate, catalog_candidate) {
            (Some(inspection), Some((catalog_key, _))) if inspection.key() != catalog_key => {
                return Err(format!(
                    "`{target}` resolves to different path and catalog repositories: {} and {catalog_key}",
                    inspection.key()
                ));
            }
            (Some(inspection), _) => (inspection.key(), inspection.spec, Some(inspection.checkout)),
            (None, Some((key, spec))) => (key, spec, None),
            (None, None) => return Err(format!("`{target}` is neither a repository path nor a repository catalog slug")),
        };

        ensure_repository_and_default_project_workflow(self.resource_backend, &namespace, &key, &repository_spec).await?;
        if let Some(checkout) = checkout {
            self.reconcile_project_checkouts(&namespace, &key, &repository_spec, checkout).await?;
        }

        let default_name = normalize_project_name(&repository_spec.leaf_slug())?;
        let project_name = explicit_name.map(str::to_string).unwrap_or(default_name.clone());
        validate_project_name(&project_name)?;
        let projects = self.resource_backend.clone().definitions::<Project>(&namespace);
        match projects.get(&project_name).await {
            Ok(existing) => {
                if is_declaration_backed_project(&existing) {
                    return Err(format!("project {project_name} is managed by a declaration; use project refresh to update it"));
                }
                if !is_whole_repository_project(&existing.spec, &key) {
                    return Err(format!("project {project_name} already exists with a different repository definition"));
                }
                if explicit_display_name.is_some_and(|display_name| display_name != existing.spec.display_name) {
                    return Err(format!(
                        "project {project_name} already exists with display name `{}`; use project apply to change it",
                        existing.spec.display_name
                    ));
                }
                reconcile_whole_repository_project_definition(&projects, existing).await?;
                return Ok(project_name);
            }
            Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(error.to_string()),
        }

        let spec = whole_repository_project_spec(key, explicit_display_name.map(str::to_string).unwrap_or(default_name))?;
        projects
            .apply_as(
                &WriterIdentity::reconcile_loop().with_source("whole-repository-project"),
                &whole_repository_project_meta(project_name.clone()),
                &spec,
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(project_name)
    }

    pub(super) async fn reconcile_project_checkouts(
        &self,
        namespace: &str,
        repository_key: &RepositoryKey,
        repository_spec: &RepositorySpec,
        checkout: crate::repository_inspection::LocalCheckoutInspection,
    ) -> Result<(), String> {
        let inspection =
            RepositoryInspection { spec: repository_spec.clone(), checkout, transport_url: None, replaces_prior_repository: false };
        let inspector = self.operations.repository_inspector().await?;
        let mut providers = ProviderData::default();
        // Each main Checkout is an independent observation producer. Gather
        // their inventories before reconciling the Repository/host scope so
        // refreshing one clone cannot erase a sibling clone's identity facts.
        let mut inspections = vec![inspection.clone()];
        for known in crate::repository_addressing::local_checkouts(
            self.resource_backend,
            self.observed_resource_backend,
            namespace,
            &inspection.checkout.host_ref,
        )
        .await?
        {
            if let flotilla_resources::CheckoutSpec::Observed(known) = known.spec {
                if known.repo_ref == *repository_key && known.is_main && Path::new(&known.path) != inspection.checkout.path {
                    inspections.push(RepositoryInspection {
                        spec: repository_spec.clone(),
                        checkout: crate::repository_inspection::LocalCheckoutInspection {
                            path: PathBuf::from(known.path),
                            host_ref: known.host_ref,
                            git_ref: known.r#ref,
                            is_main: true,
                        },
                        transport_url: None,
                        replaces_prior_repository: false,
                    });
                }
            }
        }
        for inspection in inspections {
            for checkout in inspector.inspect_checkouts(&inspection).await? {
                providers.checkouts.insert(
                    QualifiedPath::host(HostId::new(checkout.host_ref), checkout.path),
                    flotilla_protocol::Checkout {
                        branch: checkout.git_ref,
                        is_main: checkout.is_main,
                        trunk_ahead_behind: None,
                        remote_ahead_behind: None,
                        working_tree: None,
                        last_commit: None,
                        host_name: None,
                        environment_id: None,
                    },
                );
            }
        }
        crate::observed_resources::reconcile_checkouts(
            self.observed_resource_backend,
            namespace,
            repository_key,
            &repository_spec.catalog_slug(),
            &providers,
            &inspection.checkout.host_ref,
        )
        .await
        .map_err(|error| error.to_string())
    }
}

pub(super) async fn repository_transport_url_with_backend(
    backend: &ResourceBackend,
    namespace: &str,
    repository: &RepositorySpec,
) -> Result<String, String> {
    match repository.identity() {
        RepositoryIdentity::Forge { forge_ref, owner, repo_name } => {
            let forge = backend
                .including_replicas::<Forge>(namespace)
                .get(forge_ref)
                .await
                .map_err(|error| format!("Forge {forge_ref}: {error}"))?;
            Ok(format!("{}/{owner}/{repo_name}", forge.object.spec.https_url.trim_end_matches('/')))
        }
        RepositoryIdentity::Remote { .. } => repository.live_remote().map(str::to_string).ok_or("no transport remote".to_string()),
        RepositoryIdentity::Local { .. } => Err("local Repository has no transport URL".to_string()),
    }
}

pub(super) async fn snapshot_project_repositories_with_backend(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: &str,
    selected: Option<&[RepositoryKey]>,
) -> Result<Vec<ConvoyRepositorySpec>, String> {
    let project = backend
        .clone()
        .including_replicas::<Project>(namespace)
        .get(project_ref)
        .await
        .map(|project| project.object)
        .map_err(|error| project_not_ready_error(namespace, project_ref, error))?;
    let repositories = backend.including_replicas::<Repository>(namespace);
    let repository_sources = repositories.list_replica_sources().await.map_err(|error| error.to_string())?;
    let mut unresolved = Vec::new();
    let mut snapshots = BTreeMap::<RepositoryKey, (String, RepositorySpec, Option<String>, BTreeSet<String>)>::new();
    for entry in &project.spec.repositories {
        if !entry.roles.is_empty()
            && !entry.roles.contains(&ProjectRepositoryRole::Code)
            && selected.is_none_or(|selected| !selected.contains(&entry.repo))
        {
            continue;
        }
        match repositories.get(&entry.repo.to_string()).await {
            Ok(repository) => {
                let repository = repository.object;
                if let Err(error) = repository.spec.verify_key(&entry.repo) {
                    unresolved.push(error);
                    continue;
                }
                let url = match repository_transport_url_with_backend(backend, namespace, &repository.spec).await {
                    Ok(url) => url,
                    Err(error) => {
                        unresolved.push(format!("repository {}: {error}", entry.repo));
                        continue;
                    }
                };
                let observed_default_refs = repository_sources
                    .items
                    .iter()
                    .filter(|source| source.object.metadata.name == entry.repo.to_string())
                    .filter_map(|source| source.object.status.as_ref()?.default_branch.clone())
                    .collect::<BTreeSet<_>>();
                let default_ref = if let Some(default_branch) = &entry.default_branch {
                    Some(default_branch.clone())
                } else if observed_default_refs.len() == 1 {
                    observed_default_refs.into_iter().next()
                } else {
                    if observed_default_refs.len() > 1 {
                        unresolved.push(format!("repository {} has conflicting observed default branches", entry.repo));
                    }
                    None
                };
                let snapshot = snapshots
                    .entry(entry.repo.clone())
                    .or_insert_with(|| (url, repository.spec.clone(), default_ref.clone(), BTreeSet::new()));
                if snapshot.2 != default_ref {
                    unresolved.push(format!("repository {} has conflicting project default branches", entry.repo));
                }
                if let Some(subpath) = &entry.subpath {
                    snapshot.3.insert(subpath.clone());
                }
            }
            Err(error) => unresolved.push(format!("repository {}: {error}", entry.repo)),
        }
    }
    for (repo_ref, (_, _, default_ref, _)) in &snapshots {
        if default_ref.is_none() {
            unresolved.push(format!("repository {repo_ref} has no resolved default branch"));
        }
    }
    if !unresolved.is_empty() {
        return Err(format!("project {project_ref} is not ready: {}", unresolved.join("; ")));
    }

    let workspace_slugs = flotilla_resources::repository_workspace_slugs(snapshots.iter().map(|(key, (_, spec, _, _))| (key, spec)));
    let mut repositories = snapshots
        .into_iter()
        .map(|(repo_ref, (url, _, default_ref, subpaths))| {
            let default_ref = default_ref.expect("missing default refs were rejected");
            ConvoyRepositorySpec {
                url,
                workspace_slug: workspace_slugs[&repo_ref].clone(),
                repo_ref,
                source_ref: default_ref.clone(),
                target_ref: default_ref,
                subpaths: subpaths.into_iter().collect(),
            }
        })
        .collect::<Vec<_>>();
    repositories.sort_by(|left, right| left.workspace_slug.cmp(&right.workspace_slug).then_with(|| left.repo_ref.cmp(&right.repo_ref)));
    Ok(repositories)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use flotilla_resources::{InMemoryBackend, SystemClock};

    use super::*;
    use crate::ops_entry::OperationalEntryFile;

    struct FakeRepositoryInspector {
        inspection: ProjectDeclarationInspection,
    }

    #[async_trait]
    impl RepositoryInspector for FakeRepositoryInspector {
        async fn inspect_path(&self, _: &Path, _: Option<&str>) -> Result<RepositoryInspection, String> {
            Ok(self.inspection.repository.clone())
        }

        async fn inspect_project_declaration(&self, _: &Path) -> Result<ProjectDeclarationInspection, String> {
            Ok(self.inspection.clone())
        }
    }

    struct FakeProjectOperations {
        inspector: Arc<dyn RepositoryInspector>,
        identity_resolutions: AtomicUsize,
    }

    #[async_trait]
    impl ProjectOperations for FakeProjectOperations {
        fn remember_bootstrap_checkout(&self, _: &RepositoryInspection) -> Result<(), String> {
            Ok(())
        }

        async fn repository_inspector(&self) -> Result<Arc<dyn RepositoryInspector>, String> {
            Ok(Arc::clone(&self.inspector))
        }

        async fn inspect_repository_path(&self, _: &Path, _: Option<&str>) -> Result<RepositoryInspection, String> {
            Err("unexpected direct repository inspection".into())
        }

        async fn resolve_repository_remote(&self, remote: &str) -> Result<RepositorySpec, String> {
            RepositorySpec::remote(remote)
        }

        async fn resolve_forge_identity(&self, spec: RepositorySpec) -> Result<RepositorySpec, String> {
            self.identity_resolutions.fetch_add(1, Ordering::SeqCst);
            Ok(spec)
        }

        async fn reap_ensured_convoy(&self, _: &str, _: &str, _: &str, _: bool) -> Result<(), String> {
            Err("unexpected convoy reap".into())
        }
    }

    // #2720: both store adapters feed the same operational materializer, without
    // checkout facts. A bad revision cannot replace its last-good workflow.
    #[tokio::test]
    async fn bound_ops_sources_apply_without_checkouts_and_preserve_last_good() {
        use flotilla_resources::{CharterSource, CharterStoreBinding, ManifestRoot};

        use crate::{
            providers::{discovery::test_support::test_vcs_resolver, ProcessCommandRunner},
            repository_inspection::GitRepositoryInspector,
        };
        for local in [false, true] {
            let source_dir = tempfile::tempdir().expect("source");
            let cache = tempfile::tempdir().expect("cache");
            let git = |args: &[&str]| {
                let output = std::process::Command::new("git").args(args).current_dir(source_dir.path()).output().expect("fixture git");
                assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            };
            let entry =
                |vessel: &str| format!("---\nkind: workflow_template\nname: review\n---\nvessels:\n  - name: {vessel}\n    crew: []\n");
            std::fs::write(source_dir.path().join("review.md"), entry("first")).expect("entry");
            if !local {
                git(&["init", "-b", "main"]);
                git(&["config", "user.name", "Test"]);
                git(&["config", "user.email", "test@example.com"]);
                git(&["add", "."]);
                git(&["commit", "-m", "initial"]);
            }
            let source = if local {
                CharterSource::LocalDirectory { directory: source_dir.path().to_string_lossy().into_owned() }
            } else {
                CharterSource::Repository {
                    repo: source_dir.path().to_string_lossy().into_owned(),
                    branch: "main".into(),
                    path: String::new(),
                }
            };
            let runner: Arc<dyn crate::providers::CommandRunner> = Arc::new(ProcessCommandRunner);
            let inspector = Arc::new(
                GitRepositoryInspector::new(runner.clone(), test_vcs_resolver(runner), "local-host")
                    .with_charter_cache(cache.path().to_path_buf()),
            );
            let operations = FakeProjectOperations { inspector: inspector.clone(), identity_resolutions: AtomicUsize::new(0) };
            let backend = ResourceBackend::InMemory(InMemoryBackend::default());
            let observed = ResourceBackend::InMemory(InMemoryBackend::observed());
            let clock: Arc<dyn Clock> = Arc::new(SystemClock);
            let namespace = std::sync::RwLock::new("flotilla".to_string());
            let service = ProjectService {
                resource_backend: &backend,
                observed_resource_backend: &observed,
                clock: &clock,
                namespace: &namespace,
                repository_index: RepositoryIndex { backend: &backend, observed: &observed, namespace: &namespace, host: "local-host" },
                operations: &operations,
            };
            let repository = RepositorySpec::remote("https://github.com/example/ops").expect("repository");
            backend
                .using::<Repository>("flotilla")
                .create(&InputMeta::builder().name(repository.key().0.clone()).build(), &repository)
                .await
                .expect("repository record");
            let mut spec = whole_repository_project_spec(repository.key(), "app".into()).expect("project spec");
            spec.repositories[0].roles.extend([ProjectRepositoryRole::Ops, ProjectRepositoryRole::Code]);
            spec.repositories[0].charter_store = Some(CharterStoreBinding { host: "local-host".into(), source });
            let project_author = if local { ResourceBackend::InMemory(InMemoryBackend::default()) } else { backend.clone() };
            project_author
                .using::<Project>("flotilla")
                .create(&InputMeta::builder().name("app".into()).build(), &spec)
                .await
                .expect("project");
            if local {
                // The source home can receive its Project from the fleet store;
                // it owns the source status, not that Project's status.
                let snapshot = project_author.using::<Project>("flotilla").list().await.expect("fleet project");
                backend
                    .replica_writer::<Project>(flotilla_protocol::NodeId::new("fleet-store"), "flotilla")
                    .replace(&snapshot, chrono::Utc::now())
                    .await
                    .expect("federate project");
            }
            service.reconcile_bound_charters().await.expect("first pass");
            let root_name = crate::charter_store::ops_root_name("app", &spec.repositories[0], "local-host");
            let roots = backend.using::<ManifestRoot>("flotilla");
            let first_status = roots.get(&root_name).await.expect("root").status.expect("status");
            assert!(first_status.source_error.is_none(), "{first_status:?}");
            let first = first_status.applied_revision.expect("revision");
            let workflows = backend.using::<WorkflowTemplate>("flotilla");
            let workflow = workflows.get("app--review").await.expect("workflow");
            assert_eq!(workflow.spec.vessels[0].name, "first");
            assert_eq!(workflow.metadata.annotations[SOURCE_COMMIT_ANNOTATION], first);
            // Re-reading an identical revision must not publish another source
            // status or rewrite its authored definitions.
            let settled_root = roots.get(&root_name).await.expect("settled source");
            service.reconcile_bound_charters().await.expect("unchanged pass");
            assert_eq!(roots.get(&root_name).await.expect("source").metadata.resource_version, settled_root.metadata.resource_version);
            assert_eq!(workflows.get("app--review").await.expect("workflow").metadata.resource_version, workflow.metadata.resource_version);
            // Candidate inventory reads the same bound head, despite no checkout.
            let projects = backend.definitions::<Project>("flotilla").list().await.expect("projects");
            let inventory = crate::repository_inspection::inspect_project_ops_entries(&projects, &BTreeMap::new(), &*inspector)
                .await
                .expect("inventory");
            assert!(inventory.entries.iter().any(|file| file.contents == entry("first")));
            std::fs::write(source_dir.path().join("review.md"), entry("second")).expect("edit");
            std::fs::write(source_dir.path().join("bad.md"), "---\nkind: workflow_template\nname: bad\n---\nvessels: [")
                .expect("bad entry");
            if !local {
                git(&["add", "."]);
                git(&["commit", "-m", "bad revision"]);
            }
            service.reconcile_bound_charters().await.expect("refusal published");
            let status = roots.get(&root_name).await.expect("root").status.expect("status");
            assert_eq!(status.applied_revision.as_deref(), Some(first.as_str()));
            assert!(status.source_error.as_deref().expect("reason").contains("bad.md"));
            assert!(status.stalled.is_some());
            assert_eq!(workflows.get("app--review").await.expect("last live").spec, workflow.spec);
            std::fs::remove_file(source_dir.path().join("bad.md")).expect("repair");
            if !local {
                git(&["add", "."]);
                git(&["commit", "-m", "repair"]);
            }
            service.reconcile_bound_charters().await.expect("recovery");
            let status = roots.get(&root_name).await.expect("root").status.expect("status");
            assert!(status.source_error.is_none());
            assert!(status.stalled.is_none());
            assert_ne!(status.applied_revision.as_deref(), Some(first.as_str()));
            assert_eq!(workflows.get("app--review").await.expect("updated").spec.vessels[0].name, "second");
            let projects = backend.definitions::<Project>("flotilla");
            let project = projects.get("app").await.expect("project definition");
            let mut spec = project.spec.clone();
            let mut unavailable = spec.repositories[0].clone();
            unavailable.repo = RepositoryKey("missing-ops".into());
            unavailable.charter_store = None;
            spec.repositories.push(unavailable);
            projects.apply(&InputMeta::from(&project.metadata), &spec).await.expect("unavailable ops member");
            service.reconcile_bound_charters().await.expect("partial-source refusal");
            let refused = roots.get(&root_name).await.expect("root").status.expect("status");
            assert_eq!(refused.applied_revision, status.applied_revision);
            assert!(refused.source_error.expect("missing source reason").contains("no local checkout"));
            assert_eq!(workflows.get("app--review").await.expect("retained workflow").spec.vessels[0].name, "second");
            // #2721: declaring a registration pointer retires old-source health
            // and stops its reads/pruning while preserving materialized records.
            spec.repositories.pop();
            spec.charter = Some(flotilla_resources::CharterPointer::Inline { documents: Vec::new(), files: BTreeMap::new() });
            projects.apply(&InputMeta::from(&project.metadata), &spec).await.expect("declare pointer");
            service.reconcile_bound_charters().await.expect("registered charter supersedes legacy ops");
            assert!(matches!(roots.get(&root_name).await, Err(ResourceError::NotFound { .. })));
            let registered = projects.get("app").await.expect("registered Project");
            if local {
                assert!(registered.status.is_none(), "replicated Project status remains owned by the fleet home");
            } else {
                let status = registered.status.expect("Project status");
                assert!(status.declaration_refused.is_none());
                assert!(status.operational_entries.expect("operational health").ready);
            }
            assert_eq!(workflows.get("app--review").await.expect("charter transition retains workflow").spec.vessels[0].name, "second");
            spec.charter = None;
            spec.repositories[0].charter_store = None;
            projects.apply(&InputMeta::from(&project.metadata), &spec).await.expect("remove binding");
            service.reconcile_bound_charters().await.expect("retire source");
            assert!(matches!(roots.get(&root_name).await, Err(ResourceError::NotFound { .. })));
            assert_eq!(workflows.get("app--review").await.expect("retained authored record").spec.vessels[0].name, "second");
        }
    }

    #[tokio::test]
    async fn project_registration_uses_injected_inspection_and_identity_resolution() {
        let temp = tempfile::tempdir().expect("checkout directory");
        let remote = "https://github.com/example/app";
        let repository_spec = RepositorySpec::remote(remote).expect("repository spec");
        let operations = FakeProjectOperations {
            inspector: Arc::new(FakeRepositoryInspector {
                inspection: ProjectDeclarationInspection {
                    repository: RepositoryInspection {
                        spec: repository_spec.clone(),
                        checkout: crate::repository_inspection::LocalCheckoutInspection::builder()
                            .path(temp.path().to_path_buf())
                            .host_ref("local-host".to_string())
                            .git_ref("main".to_string())
                            .is_main(true)
                            .build(),
                        transport_url: Some(remote.to_string()),
                        replaces_prior_repository: false,
                    },
                    yaml: format!("name: app\nmembers:\n  - alias: app\n    url: {remote}\n    roles: [code]\n"),
                    commit: "abc123".into(),
                },
            }),
            identity_resolutions: AtomicUsize::new(0),
        };
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let observed = ResourceBackend::InMemory(InMemoryBackend::observed());
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let namespace = std::sync::RwLock::new("flotilla".to_string());
        let service = ProjectService {
            resource_backend: &backend,
            observed_resource_backend: &observed,
            clock: &clock,
            namespace: &namespace,
            repository_index: RepositoryIndex { backend: &backend, observed: &observed, namespace: &namespace, host: "local-host" },
            operations: &operations,
        };

        // #2777 review: exercise bootstrap's handoff to ops materialization
        // while the fleet writer's namespace critical section contends. A
        // retained non-reentrant guard must fail this deadline, not hang CI.
        let fleet_lock = crate::charter_store::authoring_lock("flotilla");
        let fleet_guard = fleet_lock.lock().await;
        let (registration, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(service.project_register(temp.path().to_str().expect("UTF-8 checkout path")), async {
                tokio::task::yield_now().await;
                drop(fleet_guard);
            })
        })
        .await
        .expect("bootstrap and fleet authoring must not deadlock");
        assert_eq!(registration.expect("register"), ("app".into(), 1));
        assert_eq!(operations.identity_resolutions.load(Ordering::SeqCst), 1);
        let project = backend.definitions::<Project>("flotilla").get("app").await.expect("registered project");
        assert_eq!(project.spec.repositories[0].repo, repository_spec.key());
        assert_eq!(project.metadata.annotations[BOOTSTRAP_COMMIT_ANNOTATION], "abc123");
        service
            .patch_project_operational_entries("flotilla", "app", false, None, "source unavailable: no checkout")
            .await
            .expect("source-level refusal");
        let refused = backend
            .using::<Project>("flotilla")
            .get("app")
            .await
            .expect("project")
            .status
            .expect("status")
            .declaration_refused
            .expect("refusal");
        assert!(refused.entry_path.is_empty(), "a source error must not invent an entry path");
    }

    // Selection is a host-local fact: foreign-host and unrelated-repository
    // checkouts do not participate. A sole checkout is usable; among multiple
    // candidates only a unique main is selectable, otherwise refuse ambiguity.
    #[hegel::test]
    fn checkout_selection_uses_local_facts_and_refuses_ambiguity(tc: hegel::TestCase) {
        use flotilla_resources::{Checkout, CheckoutSpec, ObservedCheckoutSpec};
        use hegel::generators as gs;

        // Generate 0–8 facts spanning both hosts, repository membership and main/
        // worktree state; pinned equivalence classes guarantee boundary coverage.
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(8));
        let generated = (0..count).map(|_| (tc.draw(gs::booleans()), tc.draw(gs::booleans()), tc.draw(gs::booleans()))).collect();
        let cases = [
            vec![],
            vec![(true, true, true)],
            vec![(true, false, true)],
            vec![(true, true, true), (true, false, true)],
            vec![(true, true, true), (true, true, true)],
            vec![(true, false, true), (true, false, true)],
            vec![(false, true, true), (true, false, true)],
            vec![(true, true, false), (true, false, true)],
            generated,
        ];
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("checkout paths");
            for facts in cases {
                let backend = ResourceBackend::InMemory(InMemoryBackend::default());
                let observed = ResourceBackend::InMemory(InMemoryBackend::observed());
                let namespace = std::sync::RwLock::new("flotilla".to_string());
                let key = RepositorySpec::remote("https://github.com/example/ops").expect("repository").key();
                for (index, (local, main, member)) in facts.iter().enumerate() {
                    observed
                        .using::<Checkout>("flotilla")
                        .create(
                            &InputMeta::builder().name(format!("checkout-{index}")).build(),
                            &CheckoutSpec::Observed(
                                ObservedCheckoutSpec::builder()
                                    .path(temp.path().join(index.to_string()).to_string_lossy().into_owned())
                                    .host_ref(if *local { "local-host" } else { "foreign-host" }.to_string())
                                    .repo_ref(if *member { key.clone() } else { RepositoryKey("other-repository".into()) })
                                    .r#ref(if *main { "main" } else { "convoy/work" }.to_string())
                                    .is_main(*main)
                                    .build(),
                            ),
                        )
                        .await
                        .expect("observed checkout");
                }
                let local = facts.iter().enumerate().filter(|(_, (local, _, member))| *local && *member).collect::<Vec<_>>();
                let mains = local.iter().filter(|(_, (_, main, _))| *main).collect::<Vec<_>>();
                let selected = if local.len() == 1 {
                    Some(local[0].0)
                } else if mains.len() == 1 {
                    Some(mains[0].0)
                } else {
                    None
                };
                let index = RepositoryIndex { backend: &backend, observed: &observed, namespace: &namespace, host: "local-host" };
                match index.select_checkout(&key).await.expect("selection") {
                    CheckoutSelection::Unavailable => assert!(local.is_empty(), "available local facts must not be lost"),
                    CheckoutSelection::Ambiguous => assert!(!local.is_empty() && selected.is_none(), "a unique checkout must be usable"),
                    CheckoutSelection::Selected(checkout) => {
                        let expected = selected.expect("ambiguous facts must never select an arbitrary checkout");
                        assert_eq!(checkout.path, temp.path().join(expected.to_string()));
                        assert_eq!(checkout.is_main, facts[expected].1);
                        assert_eq!(checkout.git_ref, if facts[expected].1 { "main" } else { "convoy/work" });
                    }
                }
            }
        });
    }

    #[test]
    fn omitted_repos_expand_to_current_code_members_on_each_collection() {
        let source_spec = RepositorySpec::remote("https://github.com/example/ops").expect("ops repository");
        let first = RepositorySpec::remote("https://github.com/example/app").expect("first code repository").key();
        let second = RepositorySpec::remote("https://github.com/example/lib").expect("second code repository").key();
        let source = OperationalEntriesInspection {
            source_root: None,
            repository: RepositoryInspection {
                spec: source_spec,
                checkout: crate::repository_inspection::LocalCheckoutInspection::builder()
                    .path(PathBuf::from("/ops"))
                    .host_ref("host".to_string())
                    .git_ref("main".to_string())
                    .is_main(true)
                    .build(),
                transport_url: None,
                replaces_prior_repository: false,
            },
            commit: "abc123".to_string(),
            files: vec![
                OperationalEntryFile {
                    path: "verify.md".to_string(),
                    contents: "---\nkind: verification_command\nname: test\n---\ncommand: cargo test\n".to_string(),
                },
                OperationalEntryFile {
                    path: "ensure.md".to_string(),
                    contents: "---\nkind: ensure\nrole: coder\n---\nworkflow: default\n".to_string(),
                },
            ],
        };

        for members in [vec![first.clone()], vec![first.clone(), second.clone()]] {
            let mut workflows = BTreeMap::new();
            let mut ensures = BTreeMap::new();
            let mut commands = BTreeMap::new();
            let mut provenance = BTreeMap::new();
            let mut outcomes = Vec::new();
            ProjectService::collect_operational_entries(
                "demo",
                &BTreeMap::new(),
                &members,
                source.clone(),
                &mut workflows,
                &mut ensures,
                &mut commands,
                &mut provenance,
                &mut outcomes,
                &mut None,
            )
            .expect("collect operational entries");
            assert_eq!(commands.keys().cloned().collect::<Vec<_>>(), members);
            assert_eq!(ensures.values().next().expect("ensure").1.repositories, members);
        }
    }
}
