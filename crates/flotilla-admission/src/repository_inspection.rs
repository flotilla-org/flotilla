use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use flotilla_charter::ops_entry::OperationalEntryFile;
use flotilla_resources::{Project, ProjectRepositoryRole, RepositoryKey, RepositorySpec, ResourceObject};

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct LocalCheckoutInspection {
    pub path: PathBuf,
    pub host_ref: String,
    pub git_ref: String,
    pub is_main: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryInspection {
    pub spec: RepositorySpec,
    pub checkout: LocalCheckoutInspection,
    pub transport_url: Option<String>,
    /// The checkout path was associated with a different Repository whose
    /// history could not be connected to this inspection.
    pub replaces_prior_repository: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDeclarationInspection {
    pub repository: RepositoryInspection,
    pub yaml: String,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct OperationalEntriesInspection {
    pub source_root: Option<String>,
    pub repository: RepositoryInspection,
    pub commit: String,
    pub files: Vec<OperationalEntryFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryContinuity {
    Continuous { evidence: String },
    Unproven { evidence: String },
}

impl RepositoryInspection {
    pub fn key(&self) -> RepositoryKey {
        self.spec.key()
    }
}

#[async_trait]
pub trait RepositoryInspector: Send + Sync {
    fn owns_charter(&self, _host: &str) -> bool {
        true
    }
    async fn charter_snapshot(
        &self,
        _source: &flotilla_resources::CharterSource,
    ) -> Result<flotilla_charter::charter_store::CharterSnapshot, String> {
        Err("bound charter inspection is unavailable".into())
    }

    /// A project image layer directory at the source's head (ADR 0062);
    /// `None` when the directory is absent.
    async fn image_layer_snapshot(
        &self,
        _source: &flotilla_resources::CharterSource,
    ) -> Result<Option<flotilla_charter::charter_store::LayerDirectorySnapshot>, String> {
        Err("image layer inspection is unavailable".into())
    }

    /// Retire charter object caches unused for the retention window (#2743).
    /// Inspectors without a cache have nothing to retire.
    async fn retire_unused_charter_caches(&self) -> Result<Vec<PathBuf>, String> {
        Ok(Vec::new())
    }

    async fn inspect_path(&self, path: &Path, remote: Option<&str>) -> Result<RepositoryInspection, String>;

    /// Enumerate every local checkout that belongs to an inspected repository.
    ///
    /// Test and non-git inspectors may only know about the checkout used to
    /// establish repository identity. Git-backed inspection overrides this to
    /// include every worktree.
    async fn inspect_checkouts(&self, inspection: &RepositoryInspection) -> Result<Vec<LocalCheckoutInspection>, String> {
        Ok(vec![inspection.checkout.clone()])
    }

    async fn resolve_remote(&self, remote: &str) -> Result<RepositorySpec, String> {
        RepositorySpec::remote(remote)
    }

    async fn verify_continuity(&self, _path: &Path, _previous: &RepositorySpec) -> RepositoryContinuity {
        RepositoryContinuity::Unproven { evidence: "repository inspector cannot compare Git history".to_string() }
    }

    async fn inspect_project_declaration(&self, path: &Path) -> Result<ProjectDeclarationInspection, String> {
        let repository = self.inspect_path(path, None).await?;
        let declaration_path = repository.checkout.path.join(flotilla_charter::project_declaration::DECLARATION_FILE);
        let yaml = std::fs::read_to_string(&declaration_path).map_err(|error| format!("read {}: {error}", declaration_path.display()))?;
        Ok(ProjectDeclarationInspection { commit: repository.checkout.git_ref.clone(), repository, yaml })
    }

    async fn inspect_operational_entries(&self, path: &Path) -> Result<OperationalEntriesInspection, String> {
        let repository = self.inspect_path(path, None).await?;
        let (commit, files) = self.operational_entry_files_at(&repository.checkout.path).await?;
        Ok(OperationalEntriesInspection { source_root: None, commit, repository, files })
    }

    /// Read the committed operational entry candidates of a checkout whose
    /// repository identity the caller has already established. Returns the
    /// commit read and its files.
    ///
    /// Non-git inspectors walk the checkout's files; git-backed inspection
    /// overrides this to read blobs at `HEAD`.
    async fn operational_entry_files_at(&self, checkout: &Path) -> Result<(String, Vec<OperationalEntryFile>), String> {
        let repository = self.inspect_path(checkout, None).await?;
        let path = repository.checkout.path.clone();
        let mut files =
            flotilla_provider_runtime::probe::blocking("operational entries", flotilla_provider_runtime::probe::PROBE_TIMEOUT, move || {
                let mut files = Vec::new();
                collect_operational_entry_files(&path, &path, &mut files)?;
                Ok(files)
            })
            .await?;
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok((repository.checkout.git_ref, files))
    }
}

/// Committed ops declarations readable on this host, for candidate-side
/// pre-roll validation.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OperationalEntryInventory {
    pub entries: Vec<OperationalEntryFile>,
    /// Ops members with no checkout on this host. The daemon refuses to load
    /// these sources here, so their entries cannot change this host's behaviour;
    /// they are validated on the hosts that hold a checkout.
    #[serde(default)]
    pub unavailable: Vec<String>,
}

/// Export the committed declaration inputs without parsing them with the running
/// daemon. A pre-roll candidate applies its own parser to this inventory.
pub async fn inspect_project_ops_entries(
    projects: &[ResourceObject<Project>],
    paths: &BTreeMap<RepositoryKey, Vec<PathBuf>>,
    inspector: &dyn RepositoryInspector,
) -> Result<OperationalEntryInventory, String> {
    let mut inventory = OperationalEntryInventory::default();
    for project in projects {
        for member in project.spec.repositories.iter().filter(|member| member.roles.contains(&ProjectRepositoryRole::Ops)) {
            if let Some(binding) = &member.charter_store {
                if !inspector.owns_charter(&binding.host) {
                    continue;
                }
                let snapshot = inspector.charter_snapshot(&binding.source).await?;
                inventory.entries.extend(snapshot.files.into_iter().map(|(path, contents)| OperationalEntryFile {
                    path: format!("{}/{}/{}/{}", project.metadata.namespace, project.metadata.name, member.repo, path),
                    contents,
                }));
                continue;
            }
            let mut candidates = paths.get(&member.repo).cloned().unwrap_or_default();
            candidates.sort();
            candidates.dedup();
            let path = match candidates.as_slice() {
                [path] => path.clone(),
                [] => {
                    inventory.unavailable.push(format!(
                        "Project/{}: ops member {} has no checkout on this host",
                        project.metadata.name,
                        member.alias.as_deref().unwrap_or(&member.repo.0)
                    ));
                    continue;
                }
                _ => {
                    let mut main_paths = Vec::new();
                    for path in candidates {
                        if inspector.inspect_path(&path, None).await?.checkout.is_main {
                            main_paths.push(path);
                        }
                    }
                    match main_paths.as_slice() {
                        [path] => path.clone(),
                        // Several branch checkouts (such as convoy worktrees) and none
                        // on main: the hosts holding the main checkout validate it.
                        [] => {
                            inventory.unavailable.push(format!(
                                "Project/{}: ops member {} has no main checkout on this host",
                                project.metadata.name,
                                member.alias.as_deref().unwrap_or(&member.repo.0)
                            ));
                            continue;
                        }
                        _ => {
                            return Err(format!(
                                "Project/{}: ops member {} has no unambiguous main checkout",
                                project.metadata.name, member.repo
                            ))
                        }
                    }
                }
            };
            // The member's identity already selected this checkout, so read its
            // committed entries without re-deriving identity, which refuses a
            // checkout whose remotes are ambiguous without a tracked branch.
            let (_, source) = inspector
                .operational_entry_files_at(&path)
                .await
                .map_err(|error| format!("Project/{} ops member {}: {error}", project.metadata.name, member.repo))?;
            for mut file in source {
                file.path = format!(
                    "{}/{}/{}/{}",
                    project.metadata.namespace,
                    project.metadata.name,
                    member.alias.as_deref().unwrap_or(&member.repo.0),
                    file.path
                );
                inventory.entries.push(file);
            }
        }
    }
    Ok(inventory)
}

pub fn collect_operational_entry_files(root: &Path, directory: &Path, files: &mut Vec<OperationalEntryFile>) -> Result<(), String> {
    let entries = std::fs::read_dir(directory).map_err(|error| format!("read {}: {error}", directory.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read {}: {error}", directory.display()))?;
        let path = entry.path();
        if path.file_name().is_some_and(|name| name == ".git") {
            continue;
        }
        if path.is_dir() {
            collect_operational_entry_files(root, &path, files)?;
        } else if let Ok(contents) = std::fs::read_to_string(&path) {
            let relative = path.strip_prefix(root).expect("walked paths remain beneath root").to_string_lossy().replace('\\', "/");
            files.push(OperationalEntryFile { path: relative, contents });
        }
    }
    Ok(())
}
