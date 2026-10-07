use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_resources::{ForgeSpec, Project, ProjectRepositoryRole, RepositoryKey, RepositorySpec, ResourceObject};

use crate::{
    ops_entry::OperationalEntryFile,
    providers::{vcs::git_worktree::GitWorktreeStrategy, ChannelLabel, CommandRunner},
    vcs::{CheckoutVcsResolver, RepositoryRead, Vcs},
};

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
    async fn charter_snapshot(&self, _source: &flotilla_resources::CharterSource) -> Result<crate::charter_store::CharterSnapshot, String> {
        Err("bound charter inspection is unavailable".into())
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
        let declaration_path = repository.checkout.path.join(crate::project_declaration::DECLARATION_FILE);
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
        let mut files = crate::probe::blocking("operational entries", crate::probe::PROBE_TIMEOUT, move || {
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

fn collect_operational_entry_files(root: &Path, directory: &Path, files: &mut Vec<OperationalEntryFile>) -> Result<(), String> {
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

pub struct GitRepositoryInspector {
    runner: Arc<dyn CommandRunner>,
    vcs: Arc<dyn CheckoutVcsResolver>,
    vcs_cache: tokio::sync::Mutex<HashMap<PathBuf, Arc<dyn Vcs>>>,
    host_ref: String,
    forges: Vec<ForgeSpec>,
    charter_cache: PathBuf,
}

impl GitRepositoryInspector {
    pub fn new(runner: Arc<dyn CommandRunner>, vcs: Arc<dyn CheckoutVcsResolver>, host_ref: impl Into<String>) -> Self {
        Self {
            charter_cache: std::env::temp_dir().join("flotilla-charter-cache"),
            runner,
            vcs,
            vcs_cache: tokio::sync::Mutex::new(HashMap::new()),
            host_ref: host_ref.into(),
            forges: Vec::new(),
        }
    }

    pub fn with_charter_cache(mut self, cache: PathBuf) -> Self {
        self.charter_cache = cache;
        self
    }

    pub fn with_forges(mut self, forges: Vec<ForgeSpec>) -> Self {
        self.forges = forges;
        self
    }

    async fn read(&self, cwd: &Path, read: RepositoryRead<'_>) -> Result<String, String> {
        self.provider(cwd).await?.read_repository(cwd, read).await.map(|output| output.trim().to_string())
    }

    async fn provider(&self, path: &Path) -> Result<Arc<dyn Vcs>, String> {
        let mut cache = self.vcs_cache.lock().await;
        if let Some(provider) = cache.get(path).cloned() {
            return Ok(provider);
        }
        let provider = self.vcs.vcs_for(None, path).await?;
        cache.insert(path.to_path_buf(), Arc::clone(&provider));
        Ok(provider)
    }

    async fn configured_remote_url(&self, cwd: &Path, remote: &str) -> Result<String, String> {
        self.read(cwd, RepositoryRead::ConfiguredRemoteUrls(remote))
            .await?
            .lines()
            .map(str::trim)
            .find(|url| !url.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("remote `{remote}` has no configured URL"))
    }

    async fn remote_urls(&self, cwd: &Path, remote: &str) -> Result<(String, String), String> {
        let configured = self.configured_remote_url(cwd, remote).await?;
        let effective = self.read(cwd, RepositoryRead::EffectiveRemoteUrl(remote)).await?;
        Ok((configured, effective))
    }

    async fn selected_remote(&self, cwd: &Path, branch: &str, requested: Option<&str>) -> Result<Option<(String, String)>, String> {
        if let Some(requested) = requested {
            if looks_like_remote_url(requested) {
                return Ok(Some((requested.to_string(), requested.to_string())));
            }
            return self
                .remote_urls(cwd, requested)
                .await
                .map(Some)
                .map_err(|_| format!("remote `{requested}` is not configured for {}", cwd.display()));
        }

        let remotes = self
            .read(cwd, RepositoryRead::RemoteNames)
            .await?
            .lines()
            .map(str::trim)
            .filter(|remote| !remote.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        match remotes.as_slice() {
            [] => Ok(None),
            [remote] => self.remote_urls(cwd, remote).await.map(Some),
            _ => {
                let tracked = self.read(cwd, RepositoryRead::TrackedRemote(branch)).await.ok();
                match tracked.filter(|tracked| remotes.contains(tracked)) {
                    Some(remote) => self.remote_urls(cwd, &remote).await.map(Some),
                    None => {
                        let mut identities = std::collections::BTreeMap::new();
                        for remote in &remotes {
                            let url = self.configured_remote_url(cwd, remote).await?;
                            let canonical = self.canonical_remote(cwd, &url).await?;
                            let identity = self
                                .forges
                                .iter()
                                .find_map(|forge| {
                                    forge
                                        .repository_path(&canonical)
                                        .ok()
                                        .flatten()
                                        .map(|(owner, repo)| format!("{}:{owner}/{repo}", forge.forge_id))
                                })
                                .unwrap_or(canonical);
                            identities.insert(identity, (remote.clone(), url));
                        }
                        if identities.len() == 1 {
                            let (remote, configured) = identities.into_values().next().expect("one identity has one remote");
                            let effective = self.read(cwd, RepositoryRead::EffectiveRemoteUrl(&remote)).await?;
                            Ok(Some((configured, effective)))
                        } else {
                            Err(format!(
                                "repository {} has multiple distinct remotes ({}); select one with --remote",
                                cwd.display(),
                                remotes.join(", ")
                            ))
                        }
                    }
                }
            }
        }
    }

    async fn canonical_remote(&self, cwd: &Path, remote: &str) -> Result<String, String> {
        let Some(host) = ssh_remote_host(remote) else {
            return flotilla_resources::canonicalize_repo_url(remote);
        };
        if self.forges.iter().any(|forge| forge.matches_host(host)) {
            return flotilla_resources::canonicalize_repo_url(remote);
        }
        let ssh_config = self
            .runner
            .run("ssh", &["-G", host], cwd, &ChannelLabel::Default)
            .await
            .map_err(|_| format!("unrecognised remote host alias `{host}`"))?;
        let resolved = ssh_config
            .lines()
            .find_map(|line| {
                let (key, value) = line.trim().split_once(char::is_whitespace)?;
                key.eq_ignore_ascii_case("hostname").then(|| value.trim())
            })
            .filter(|resolved| !resolved.is_empty())
            .ok_or_else(|| format!("unrecognised remote host alias `{host}`"))?;
        if resolved == host {
            if !host.contains('.') {
                return Err(format!("unrecognised remote host alias `{host}`"));
            }
            flotilla_resources::canonicalize_repo_url(remote)
        } else {
            flotilla_resources::canonicalize_repo_url(&remote.replacen(host, resolved, 1))
        }
    }
}

#[async_trait]
impl RepositoryInspector for GitRepositoryInspector {
    fn owns_charter(&self, host: &str) -> bool {
        self.host_ref == host
    }

    async fn charter_snapshot(&self, source: &flotilla_resources::CharterSource) -> Result<crate::charter_store::CharterSnapshot, String> {
        let vcs = crate::vcs::FlotillaVcs::new(
            crate::path_context::ExecutionEnvironmentPath::new("/"),
            self.runner.clone(),
            crate::vcs::GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new("unused".into(), self.runner.clone()))),
        );
        crate::charter_store::read_charter_source(source, &self.charter_cache, Some(&vcs)).await
    }

    async fn inspect_path(&self, path: &Path, remote: Option<&str>) -> Result<RepositoryInspection, String> {
        let path = crate::probe::canonicalize(path).await?;
        let top_level = PathBuf::from(self.read(&path, RepositoryRead::CheckoutRoot).await?);
        let top_level = crate::probe::canonicalize(&top_level).await?;
        let provider = self.provider(&path).await?;
        self.vcs_cache.lock().await.insert(top_level.clone(), provider);
        // `rev-parse HEAD` can fail before the first commit, while the symbolic
        // ref still exposes the initial branch name.
        let branch = match self.read(&top_level, RepositoryRead::CurrentBranch).await {
            Ok(branch) => branch,
            Err(_) => self.read(&top_level, RepositoryRead::SymbolicBranch).await?,
        };
        let git_ref = if branch == "HEAD" { self.read(&top_level, RepositoryRead::HeadRevision).await? } else { branch.clone() };
        let selected_remote = self.selected_remote(&top_level, &branch, remote).await?;
        let (spec, transport_url) = match selected_remote {
            Some((configured, effective)) => {
                let identity_remote = self.canonical_remote(&top_level, &configured).await?;
                let live_remote =
                    if configured == effective { identity_remote.clone() } else { self.canonical_remote(&top_level, &effective).await? };
                (RepositorySpec::remote(identity_remote)?.update_remotes(live_remote)?, Some(effective))
            }
            None => {
                let common_dir = PathBuf::from(self.read(&top_level, RepositoryRead::SharedMetadataDir).await?);
                let common_dir = if common_dir.is_absolute() { common_dir } else { top_level.join(common_dir) };
                let common_dir = crate::probe::canonicalize(&common_dir).await?;
                (RepositorySpec::local(&self.host_ref, common_dir.to_string_lossy())?, None)
            }
        };
        Ok(RepositoryInspection {
            spec,
            checkout: LocalCheckoutInspection {
                path: top_level,
                host_ref: self.host_ref.clone(),
                git_ref: git_ref.clone(),
                is_main: matches!(git_ref.as_str(), "main" | "master" | "trunk"),
            },
            transport_url,
            replaces_prior_repository: false,
        })
    }

    async fn inspect_project_declaration(&self, path: &Path) -> Result<ProjectDeclarationInspection, String> {
        let repository = self.inspect_path(path, None).await?;
        let commit = self.read(&repository.checkout.path, RepositoryRead::HeadRevision).await?;
        let declaration_ref = format!("{commit}:{}", crate::project_declaration::DECLARATION_FILE);
        let yaml = self.read(&repository.checkout.path, RepositoryRead::FileAtRevision(&declaration_ref)).await.map_err(|error| {
            format!(
                "read {} from bootstrap commit {commit}: {error}",
                repository.checkout.path.join(crate::project_declaration::DECLARATION_FILE).display()
            )
        })?;
        Ok(ProjectDeclarationInspection { repository, yaml, commit })
    }

    async fn operational_entry_files_at(&self, checkout: &Path) -> Result<(String, Vec<OperationalEntryFile>), String> {
        let commit = self.read(checkout, RepositoryRead::HeadRevision).await?;
        // Use one tree-wide grep to find content candidates. Operational entry
        // kind and scope remain content-authoritative; this only avoids one
        // `git show` subprocess for every unrelated file in a large ops+code
        // repository.
        let grep = self
            .provider(checkout)
            .await?
            .operational_entry_paths(checkout, &commit)
            .await
            .map_err(|error| format!("git grep operational entries in {}: {error}", checkout.display()))?;
        let paths = if grep.success() || grep.stderr.trim().is_empty() {
            grep.stdout
        } else {
            return Err(format!("git grep operational entries in {}: {}", checkout.display(), grep.stderr.trim()));
        };
        let prefix = format!("{commit}:");
        let mut files = Vec::new();
        for entry_path in paths.lines().filter_map(|path| path.strip_prefix(&prefix)).filter(|path| !path.is_empty()) {
            let object_ref = format!("{commit}:{entry_path}");
            let contents = self
                .read(checkout, RepositoryRead::FileAtRevision(&object_ref))
                .await
                .map_err(|error| format!("read operational entry {entry_path}: {error}"))?;
            files.push(OperationalEntryFile { path: entry_path.to_string(), contents });
        }
        Ok((commit, files))
    }

    async fn resolve_remote(&self, remote: &str) -> Result<RepositorySpec, String> {
        RepositorySpec::remote(self.canonical_remote(Path::new("/"), remote).await?)
    }

    async fn verify_continuity(&self, path: &Path, previous: &RepositorySpec) -> RepositoryContinuity {
        let Some(previous_remote) = previous.live_remote() else {
            return RepositoryContinuity::Unproven { evidence: "previous Repository has no transport remote".to_string() };
        };
        let advertised = match self.read(path, RepositoryRead::AdvertisedRefs(previous_remote)).await {
            Ok(advertised) => advertised,
            Err(error) => return RepositoryContinuity::Unproven { evidence: format!("old remote refs unavailable: {error}") },
        };
        let mut refs = 0;
        for line in advertised.lines() {
            let Some((commit, reference)) = line.split_once(char::is_whitespace) else {
                continue;
            };
            refs += 1;
            if self.read(path, RepositoryRead::IsAncestor { ancestor: commit, descendant: "HEAD" }).await.is_ok() {
                return RepositoryContinuity::Continuous {
                    evidence: format!("old remote ref {reference} ({commit}) is reachable from HEAD"),
                };
            }
        }
        RepositoryContinuity::Unproven { evidence: format!("none of {refs} advertised old-remote refs is reachable from HEAD") }
    }

    async fn inspect_checkouts(&self, inspection: &RepositoryInspection) -> Result<Vec<LocalCheckoutInspection>, String> {
        self.provider(&inspection.checkout.path).await?.enumerate_checkouts().await.map(|checkouts| {
            checkouts
                .into_iter()
                .map(|checkout| LocalCheckoutInspection {
                    path: checkout.path.into_path_buf(),
                    host_ref: self.host_ref.clone(),
                    git_ref: checkout.git_ref,
                    is_main: checkout.is_main,
                })
                .collect()
        })
    }
}

fn looks_like_remote_url(value: &str) -> bool {
    value.contains("://") || value.contains(':')
}

fn ssh_remote_host(remote: &str) -> Option<&str> {
    if let Some(rest) = remote.strip_prefix("ssh://") {
        return rest.split('/').next()?.rsplit_once('@').map_or(Some(rest.split('/').next()?), |(_, host)| Some(host));
    }
    if remote.contains("://") {
        return None;
    }
    let (authority, path) = remote.split_once(':')?;
    (!path.is_empty()).then(|| authority.rsplit_once('@').map_or(authority, |(_, host)| host))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        path::Path,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    use flotilla_resources::{ForgeKind, ForgeSpec, RepositoryIdentity, RepositorySpec};

    use super::{GitRepositoryInspector, LocalCheckoutInspection, RepositoryContinuity, RepositoryInspection, RepositoryInspector};
    use crate::{
        path_context::ExecutionEnvironmentPath,
        providers::{
            discovery::test_support::{test_vcs_resolver, DiscoveryMockRunner},
            ChannelLabel, CommandOutput, CommandRunner,
        },
        vcs::EnumeratedCheckout,
    };

    // Process boundary: count and reject every Git command except worktree enumeration.
    struct ObservationRunner {
        result: Result<String, String>,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl CommandRunner for ObservationRunner {
        async fn run(&self, cmd: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
            assert_eq!((cmd, args), ("git", ["worktree", "list", "--porcelain"].as_slice()), "observation must not enrich worktrees");
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }

        async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
            panic!("observation must only enumerate worktrees")
        }

        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            panic!("observation must not probe tools")
        }
    }

    fn test_inspector(runner: Arc<dyn CommandRunner>, host_ref: &str) -> GitRepositoryInspector {
        GitRepositoryInspector::new(Arc::clone(&runner), test_vcs_resolver(runner), host_ref)
    }

    fn git_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let root = temp.path().join("repo");
        std::fs::create_dir(&root).expect("repo dir");
        std::fs::create_dir(root.join(".git")).expect("git dir");
        (temp, root)
    }

    #[tokio::test]
    async fn continuity_accepts_an_old_remote_ref_reachable_from_head() {
        let (_temp, root) = git_repo();
        let commit = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["ls-remote", "--refs", "https://github.com/org/old"], Ok(format!("{commit}\trefs/heads/main\n")))
            .on_run("git", &["merge-base", "--is-ancestor", commit, "HEAD"], Ok(String::new()))
            .build();
        let inspector = test_inspector(Arc::new(runner), "host-01");
        let previous = RepositorySpec::remote("https://github.com/org/old").expect("old repository");

        assert!(matches!(inspector.verify_continuity(&root, &previous).await, RepositoryContinuity::Continuous { .. }));
    }

    #[tokio::test]
    async fn continuity_rejects_old_remote_refs_unreachable_from_head() {
        let (_temp, root) = git_repo();
        let commit = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["ls-remote", "--refs", "https://github.com/org/old"], Ok(format!("{commit}\trefs/heads/main\n")))
            .on_run("git", &["merge-base", "--is-ancestor", commit, "HEAD"], Err("unrelated histories".to_string()))
            .build();
        let inspector = test_inspector(Arc::new(runner), "host-01");
        let previous = RepositorySpec::remote("https://github.com/org/old").expect("old repository");

        assert!(matches!(inspector.verify_continuity(&root, &previous).await, RepositoryContinuity::Unproven { .. }));
    }

    // Glue: observation maps identity facts and host ownership in one call, without enrichment.
    #[tokio::test]
    async fn git_inspection_enumerates_main_and_linked_worktree_checkouts() {
        let (_temp, root) = git_repo();
        let feature = root.parent().expect("repo parent").join("repo.feature");
        let porcelain = format!(
            "worktree {}\nHEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbranch refs/heads/main\n\n\
             worktree {}\nHEAD bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\nbranch refs/heads/feature\n",
            root.display(),
            feature.display()
        );
        let runner = Arc::new(ObservationRunner { result: Ok(porcelain), calls: AtomicUsize::new(0) });
        let inspector = test_inspector(runner.clone(), "host-01");
        let inspection = RepositoryInspection {
            spec: RepositorySpec::remote("https://github.com/org/repo").expect("repository spec"),
            checkout: LocalCheckoutInspection {
                path: root.clone(),
                host_ref: "host-01".to_string(),
                git_ref: "main".to_string(),
                is_main: true,
            },
            transport_url: None,
            replaces_prior_repository: false,
        };

        let checkouts = inspector.inspect_checkouts(&inspection).await.expect("checkout inspection");

        assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            checkouts,
            vec![
                LocalCheckoutInspection { path: root, host_ref: "host-01".to_string(), git_ref: "main".to_string(), is_main: true },
                LocalCheckoutInspection { path: feature, host_ref: "host-01".to_string(), git_ref: "feature".to_string(), is_main: false },
            ]
        );
    }

    // Enumeration preserves the enriched parser's empty/bare filtering and detached labels,
    // and propagates command errors. No concurrent state or reconciliation is changed here.
    #[tokio::test]
    async fn lightweight_enumeration_handles_empty_bare_detached_and_errors() {
        let (_temp, root) = git_repo();
        for (result, expected) in [
            (Ok(String::new()), Ok(vec![])),
            (Ok("worktree /bare\nbare\n\n".into()), Ok(vec![])),
            (
                Ok("worktree /bare\nbare\n\nworktree /detached\nHEAD abcdef0123456789\ndetached\n\n".into()),
                Ok(vec![EnumeratedCheckout {
                    path: ExecutionEnvironmentPath::new("/detached"),
                    git_ref: "(detached: abcdef0)".into(),
                    is_main: true,
                }]),
            ),
            (Err("worktree list failed".into()), Err("worktree list failed".into())),
        ] {
            let runner = Arc::new(ObservationRunner { result, calls: AtomicUsize::new(0) });
            let vcs = test_vcs_resolver(runner.clone()).vcs_for(None, &root).await.expect("resolve VCS");
            assert_eq!(vcs.enumerate_checkouts().await, expected);
            assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn ssh_alias_resolves_to_machine_independent_remote_identity() {
        let (_temp, root) = git_repo();
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(root.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Ok("main\n".to_string()))
            .on_run("git", &["remote"], Ok("origin\n".to_string()))
            .on_run("git", &["config", "--get-all", "remote.origin.url"], Ok("work-github:org/repo.git\n".to_string()))
            .on_run("git", &["remote", "get-url", "origin"], Ok("work-github:org/repo.git\n".to_string()))
            .on_run("ssh", &["-G", "work-github"], Ok("hostname github.com\nuser git\n".to_string()))
            .build();
        let inspector = test_inspector(Arc::new(runner), "host-01");

        let inspected = inspector.inspect_path(&root, None).await.expect("inspection should succeed");

        assert!(matches!(
            inspected.spec.identity(),
            RepositoryIdentity::Remote { canonical_remote } if canonical_remote == "https://github.com/org/repo"
        ));
    }

    #[tokio::test]
    async fn federated_forge_alias_resolves_without_local_ssh_config() {
        let forge = ForgeSpec::builder()
            .forge_id("flotilla-lab".to_string())
            .kind(ForgeKind::Forgejo)
            .hosts(BTreeSet::from(["forgejo-manchego".to_string(), "forgejo.lab.flotilla.work".to_string()]))
            .https_url("https://forgejo.lab.flotilla.work".to_string())
            .git_ssh_host("manchego.lab.flotilla.work".to_string())
            .build();
        let inspector = test_inspector(Arc::new(DiscoveryMockRunner::builder().build()), "host-01").with_forges(vec![forge.clone()]);
        let resolved = inspector.resolve_remote("forgejo-manchego:robert/ghostty-ops.git").await.expect("resolve alias");
        let on_forge = resolved.on_forge(&forge).expect("forge identity");
        assert!(matches!(on_forge.identity(), RepositoryIdentity::Forge { forge_ref, .. } if forge_ref == "flotilla-lab"));
    }

    #[tokio::test]
    async fn configured_https_remote_determines_identity_while_effective_url_drives_live_transport() {
        let (_temp, root) = git_repo();
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(root.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Ok("main\n".to_string()))
            .on_run("git", &["remote"], Ok("origin\n".to_string()))
            .on_run(
                "git",
                &["config", "--get-all", "remote.origin.url"],
                Ok("https://forgejo.lab.flotilla.work/fork-issues/ghostty.git\n".to_string()),
            )
            .on_run("git", &["remote", "get-url", "origin"], Ok("forgejo-manchego:fork-issues/ghostty.git\n".to_string()))
            .on_run("ssh", &["-G", "forgejo-manchego"], Ok("hostname manchego.lab.flotilla.work\nuser git\n".to_string()))
            .build();
        let inspector = test_inspector(Arc::new(runner), "host-01");

        let inspected = inspector.inspect_path(&root, None).await.expect("inspection should use the configured URL");

        assert!(matches!(
            inspected.spec.identity(),
            RepositoryIdentity::Remote { canonical_remote }
                if canonical_remote == "https://forgejo.lab.flotilla.work/fork-issues/ghostty"
        ));
        assert_eq!(inspected.transport_url.as_deref(), Some("forgejo-manchego:fork-issues/ghostty.git"));
        assert_eq!(inspected.spec.live_remote(), Some("https://manchego.lab.flotilla.work/fork-issues/ghostty"));
        let forge = inspected.spec.forge().expect("effective live remote should determine forge attribution");
        assert_eq!(forge.service_url, "https://manchego.lab.flotilla.work");
        assert_eq!(forge.repository, "fork-issues/ghostty");
    }

    #[tokio::test]
    async fn first_configured_url_determines_identity_for_a_multi_url_remote() {
        let (_temp, root) = git_repo();
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(root.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Ok("main\n".to_string()))
            .on_run("git", &["remote"], Ok("origin\n".to_string()))
            .on_run(
                "git",
                &["config", "--get-all", "remote.origin.url"],
                Ok("https://github.com/org/repo.git\nhttps://mirror.example/org/repo.git\n".to_string()),
            )
            .on_run("git", &["remote", "get-url", "origin"], Ok("https://github.com/org/repo.git\n".to_string()))
            .build();
        let inspector = test_inspector(Arc::new(runner), "host-01");

        let inspected = inspector.inspect_path(&root, None).await.expect("multi-URL remote should use its first URL");

        assert!(matches!(
            inspected.spec.identity(),
            RepositoryIdentity::Remote { canonical_remote } if canonical_remote == "https://github.com/org/repo"
        ));
    }

    #[tokio::test]
    async fn unresolved_ssh_alias_fails_instead_of_becoming_a_repository_key() {
        let (_temp, root) = git_repo();
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(root.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Ok("main\n".to_string()))
            .on_run("git", &["remote"], Ok("origin\n".to_string()))
            .on_run("git", &["config", "--get-all", "remote.origin.url"], Ok("mystery:org/repo.git\n".to_string()))
            .on_run("git", &["remote", "get-url", "origin"], Ok("mystery:org/repo.git\n".to_string()))
            .on_run("ssh", &["-G", "mystery"], Ok("hostname mystery\n".to_string()))
            .build();
        let inspector = test_inspector(Arc::new(runner), "host-01");

        let error = inspector.inspect_path(&root, None).await.expect_err("unknown alias should fail");

        assert!(error.contains("unrecognised remote host alias"));
    }

    #[tokio::test]
    async fn dotted_ssh_alias_is_resolved_instead_of_assumed_to_be_a_hostname() {
        let (_temp, root) = git_repo();
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(root.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Ok("main\n".to_string()))
            .on_run("git", &["remote"], Ok("origin\n".to_string()))
            .on_run("git", &["config", "--get-all", "remote.origin.url"], Ok("github.work:org/repo.git\n".to_string()))
            .on_run("git", &["remote", "get-url", "origin"], Ok("github.work:org/repo.git\n".to_string()))
            .on_run("ssh", &["-G", "github.work"], Ok("hostname github.com\nuser git\n".to_string()))
            .build();
        let inspector = test_inspector(Arc::new(runner), "host-01");

        let inspected = inspector.inspect_path(&root, None).await.expect("dotted alias should resolve");

        assert!(matches!(
            inspected.spec.identity(),
            RepositoryIdentity::Remote { canonical_remote } if canonical_remote == "https://github.com/org/repo"
        ));
    }

    #[tokio::test]
    async fn remote_less_worktrees_with_one_common_dir_converge() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let common = temp.path().join("common.git");
        std::fs::create_dir(&first).expect("first");
        std::fs::create_dir(&second).expect("second");
        std::fs::create_dir(&common).expect("common");
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(first.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(second.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Ok("main\n".to_string()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Ok("feature\n".to_string()))
            .on_run("git", &["remote"], Ok(String::new()))
            .on_run("git", &["remote"], Ok(String::new()))
            .on_run("git", &["rev-parse", "--git-common-dir"], Ok(common.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--git-common-dir"], Ok(common.to_string_lossy().into_owned()))
            .build();
        let inspector = test_inspector(Arc::new(runner), "host-01");

        let first = inspector.inspect_path(&first, None).await.expect("first inspection");
        let second = inspector.inspect_path(&second, None).await.expect("second inspection");

        assert_eq!(first.key(), second.key());
    }

    #[tokio::test]
    async fn unborn_head_uses_the_symbolic_branch_name() {
        let (_temp, root) = git_repo();
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(root.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Err("unborn HEAD".to_string()))
            .on_run("git", &["symbolic-ref", "--short", "HEAD"], Ok("main\n".to_string()))
            .on_run("git", &["remote"], Ok(String::new()))
            .on_run("git", &["rev-parse", "--git-common-dir"], Ok(".git\n".to_string()))
            .build();
        let inspector = test_inspector(Arc::new(runner), "host-01");

        let inspected = inspector.inspect_path(&root, None).await.expect("unborn repository should be inspected");

        assert!(matches!(inspected.spec.identity(), RepositoryIdentity::Local { host_ref, .. } if host_ref == "host-01"));
        assert_eq!(inspected.checkout.git_ref, "main");
        assert!(inspected.checkout.is_main);
    }

    #[tokio::test]
    async fn ambiguous_multiple_remotes_require_an_explicit_selection() {
        let (_temp, root) = git_repo();
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(root.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Ok("main\n".to_string()))
            .on_run("git", &["remote"], Ok("origin\nupstream\n".to_string()))
            .on_run("git", &["config", "--get-all", "remote.origin.url"], Ok("https://github.com/fork/repo.git\n".to_string()))
            .on_run("git", &["config", "--get-all", "remote.upstream.url"], Ok("https://github.com/upstream/repo.git\n".to_string()))
            .build();
        let inspector = test_inspector(Arc::new(runner), "host-01");

        let error = inspector.inspect_path(&root, None).await.expect_err("ambiguous remotes should fail");

        assert!(error.contains("multiple distinct remotes"));
        assert!(error.contains("--remote"));
    }

    #[tokio::test]
    async fn multiple_remote_names_with_one_normalized_identity_are_unambiguous() {
        let (_temp, root) = git_repo();
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(root.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Ok("main\n".to_string()))
            .on_run("git", &["remote"], Ok("origin\nmirror\n".to_string()))
            .on_run("git", &["config", "--get-all", "remote.origin.url"], Ok("https://github.com/org/repo.git\n".to_string()))
            .on_run("git", &["config", "--get-all", "remote.mirror.url"], Ok("git@github.com:org/repo.git\n".to_string()))
            .on_run("git", &["remote", "get-url", "mirror"], Ok("git@github.com:org/repo.git\n".to_string()))
            .on_run("ssh", &["-G", "github.com"], Ok("hostname github.com\nuser git\n".to_string()))
            .on_run("ssh", &["-G", "github.com"], Ok("hostname github.com\nuser git\n".to_string()))
            .build();
        let inspector = test_inspector(Arc::new(runner), "host-01");

        let inspected = inspector.inspect_path(&root, None).await.expect("same identity should be unambiguous");

        assert!(matches!(
            inspected.spec.identity(),
            RepositoryIdentity::Remote { canonical_remote } if canonical_remote == "https://github.com/org/repo"
        ));
    }
}
