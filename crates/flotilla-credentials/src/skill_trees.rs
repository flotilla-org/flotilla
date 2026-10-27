//! Skill trees fetched and selected on the daemon host (#3663).
//!
//! The convoy's controlling daemon fetches each frozen revision through
//! [`Vcs::fetch_commit`] with credentials that never leave its host, checks the
//! selected trees against the current supply's authority and the tree policy,
//! and builds one canonical archive per crew ([`SkillArchive`]). The vessel
//! agent's content cache stores it by digest.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use flotilla_resources::SkillCatalogEntry;
use flotilla_vcs::vcs::{CommitFetchError, GitCredentialFile, TreeEntry, Vcs};

use crate::{
    agent_material::FLOTILLA_SKILLS_DIR_ENV,
    skill_archive::{ArchiveFile, SkillArchive},
};
use url::Url;

/// The file every delivered skill tree carries: the canonical record of what it holds.
pub const SKILL_TREE_MANIFEST: &str = SKILL_BUNDLE_MANIFEST;
/// The generation supply's manifest; a published tree carries its own under
/// the same name.
pub(crate) const SKILL_BUNDLE_MANIFEST: &str = ".flotilla-sources.json";
const MAX_ENTRIES: usize = 4096;
const MAX_DEPTH: usize = 16;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TREE_BYTES: u64 = 64 * 1024 * 1024;
/// Object caches unused this long are retired; a retired cache costs one refetch.
const CACHE_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// A refusal or failure, with its disposition. Permanent refusals come only
/// from positive evidence: supply authority, a pin the server cannot serve,
/// the forge refusing authority, or the tree policy. Everything else is transient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillTreeError {
    pub permanent: bool,
    pub message: String,
}

impl SkillTreeError {
    pub(crate) fn permanent(message: impl Into<String>) -> Self {
        Self { permanent: true, message: message.into() }
    }

    fn transient(message: impl Into<String>) -> Self {
        Self { permanent: false, message: message.into() }
    }
}

impl std::fmt::Display for SkillTreeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// Mints the narrowed read token for a private source's declared credential.
/// The mint is the live authority check: it runs on every staging, cache hit
/// or not.
#[async_trait]
pub trait SkillSourceTokens: Send + Sync {
    async fn mint(&self, credential: &str, repository: &str) -> Result<String, String>;
}

/// Operator-supplied token files keyed by credential name, for the candidate
/// pre-roll probe. Nothing is minted.
pub struct TokenFiles<'a>(pub &'a BTreeMap<String, PathBuf>);

#[async_trait]
impl SkillSourceTokens for TokenFiles<'_> {
    async fn mint(&self, credential: &str, _repository: &str) -> Result<String, String> {
        let path = self.0.get(credential).ok_or_else(|| format!("credential {credential} requires an explicit probe token file"))?;
        let token = tokio::fs::read_to_string(path).await.map_err(|error| format!("read probe token for {credential}: {error}"))?;
        Ok(token.trim_end().to_string())
    }
}

/// A crew's frozen skill selection with the sources the current supply authorizes
/// for it, resolved once: its digest names the delivery, and its archive is built
/// from these same sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillTree {
    sources: Vec<SkillSource>,
    selected: Vec<SkillCatalogEntry>,
}

impl SkillTree {
    /// Resolve `selected` against the current supply at `supply` (ADR 0052). An empty
    /// selection needs no supply. A supply that is missing or invalid, and a selection
    /// it does not authorize, are permanent refusals.
    pub async fn resolve(supply: Option<&Path>, selected: &[SkillCatalogEntry]) -> Result<Self, SkillTreeError> {
        let sources = if selected.is_empty() {
            Vec::new()
        } else {
            let supply = supply
                .ok_or_else(|| {
                    SkillTreeError::permanent(format!("frozen skills need the generation's skill supply ({FLOTILLA_SKILLS_DIR_ENV})"))
                })?
                .to_path_buf();
            let inspection = tokio::task::spawn_blocking(move || inspect_skill_sources(&supply))
                .await
                .map_err(|error| SkillTreeError::transient(format!("inspect skill supply task: {error}")))?
                .map_err(SkillTreeError::permanent)?;
            resolve_frozen_sources(&inspection.sources, selected).map_err(SkillTreeError::permanent)?
        };
        Ok(Self { sources, selected: selected.to_vec() })
    }

    /// Content identity: every source at its pinned revision, and every selected path.
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let identity = serde_json::json!({"sources": self.sources, "selected": self.selected});
        format!("{:x}", Sha256::digest(identity.to_string().as_bytes()))
    }
}

/// Host-side acquisition: one daemon-private object cache per repository.
pub struct SkillTrees {
    vcs: Arc<dyn Vcs>,
    cache: PathBuf,
}

#[derive(serde::Serialize)]
struct ManifestSource<'a> {
    name: &'a str,
    repository: &'a str,
    revision: &'a str,
    paths: &'a [String],
}

#[derive(serde::Serialize)]
struct ManifestSkill<'a> {
    name: &'a str,
    source: &'a str,
    path: &'a str,
}

/// Canonical: sorted entries, fixed key order, no whitespace, and no
/// timestamps or credential names, so the digest is a function of the
/// selection and the source bytes alone.
#[derive(serde::Serialize)]
struct TreeManifest<'a> {
    schema_version: u32,
    sources: Vec<ManifestSource<'a>>,
    skills: Vec<ManifestSkill<'a>>,
}

impl SkillTrees {
    pub fn new(vcs: Arc<dyn Vcs>, cache: PathBuf) -> Self {
        Self { vcs, cache }
    }

    /// The canonical archive of one crew's resolved tree. An empty selection is a
    /// tree holding only its manifest, so delivering it removes stale managed skills.
    pub async fn crew_archive(&self, tree: &SkillTree, tokens: Option<&dyn SkillSourceTokens>) -> Result<SkillArchive, SkillTreeError> {
        let (sources, selection) = (&tree.sources, &tree.selected);
        self.retire_unused_caches().await;
        let mut files = Vec::new();
        let mut names = BTreeSet::new();
        // Admitted from tree metadata, before any blob is read.
        let mut budget = Budget { files: MAX_ENTRIES, bytes: MAX_TREE_BYTES };
        for source in sources {
            self.fetch(source, tokens).await?;
            for entry in selection.iter().filter(|entry| entry.source == source.name) {
                if !names.insert(entry.name.as_str()) {
                    return Err(SkillTreeError::permanent(format!("duplicate skill name {} from {}", entry.name, source.repository)));
                }
                files.extend(self.skill_files(source, entry, &mut budget).await?);
            }
        }
        let mut skills =
            selection.iter().map(|entry| ManifestSkill { name: &entry.name, source: &entry.source, path: &entry.path }).collect::<Vec<_>>();
        skills.sort_by(|left, right| left.name.cmp(right.name));
        let manifest = TreeManifest {
            schema_version: 1,
            sources: sources
                .iter()
                .map(|source| ManifestSource {
                    name: &source.name,
                    repository: &source.repository,
                    revision: &source.revision,
                    paths: &source.paths,
                })
                .collect(),
            skills,
        };
        let manifest = serde_json::to_vec(&manifest).expect("skill tree manifest serializes");
        files.push(ArchiveFile { path: SKILL_TREE_MANIFEST.into(), executable: false, contents: manifest });
        SkillArchive::build(files).map_err(SkillTreeError::permanent)
    }

    async fn fetch(&self, source: &SkillSource, tokens: Option<&dyn SkillSourceTokens>) -> Result<(), SkillTreeError> {
        let context = format!("skill source {} revision {}", source.name, source.revision);
        check_transport(&source.repository).map_err(|error| SkillTreeError::permanent(format!("{context}: {error}")))?;
        let credential = match &source.credential {
            Some(credential) => {
                let tokens = tokens.ok_or_else(|| {
                    SkillTreeError::permanent(format!("{context}: credential {credential} needs the host-local credential store"))
                })?;
                let token = tokens
                    .mint(credential, &source.repository)
                    .await
                    .map_err(|error| SkillTreeError::transient(format!("{context}: credential {credential} mint failed: {error}")))?;
                let directory = tokio::task::spawn_blocking(move || private_token_file(&token))
                    .await
                    .map_err(|error| SkillTreeError::transient(format!("{context}: token file task: {error}")))?
                    .map_err(|error| SkillTreeError::transient(format!("{context}: {error}")))?;
                Some(Arc::new(directory))
            }
            None => None,
        };
        // A failure is retried whole by vessel delivery, with backoff.
        let file = credential.map(|directory| GitCredentialFile::new(directory.path().join("token"), directory));
        self.vcs.fetch_commit(&self.cache, &source.repository, &source.revision, file).await.map_err(|error| match error {
            CommitFetchError::Transient(error) | CommitFetchError::FallbackFailed(error) => {
                SkillTreeError::transient(format!("{context}: {error}"))
            }
            CommitFetchError::Unavailable(error) => {
                SkillTreeError::permanent(format!("{context}: {error}; re-pin the crew's skills to a revision the server serves"))
            }
            CommitFetchError::Refused(error) => SkillTreeError::permanent(format!("{context}: {error}")),
        })
    }

    /// One selected skill's files, under its frozen name. Only regular files
    /// are admitted; symlinks and submodules are refused, and imports are
    /// never discovered.
    async fn skill_files(
        &self,
        source: &SkillSource,
        entry: &SkillCatalogEntry,
        budget: &mut Budget,
    ) -> Result<Vec<ArchiveFile>, SkillTreeError> {
        let context = format!("skill source {} revision {} skill {} at {}", source.name, source.revision, entry.name, entry.path);
        let tree = self
            .vcs
            .files_at_revision(&self.cache, &source.repository, &source.revision, &entry.path)
            .await
            .map_err(|error| SkillTreeError::transient(format!("{context}: {error}")))?;
        if tree.len() > MAX_ENTRIES {
            return Err(SkillTreeError::permanent(format!("{context}: more than {MAX_ENTRIES} entries")));
        }
        let mut admitted = Vec::new();
        for (path, file) in tree {
            match file {
                TreeEntry::File(file) => {
                    if path.split('/').count() > MAX_DEPTH {
                        return Err(SkillTreeError::permanent(format!("{context}: {path} is deeper than {MAX_DEPTH}")));
                    }
                    if file.size() > MAX_FILE_BYTES {
                        return Err(SkillTreeError::permanent(format!("{context}: {path} exceeds {MAX_FILE_BYTES} bytes")));
                    }
                    budget
                        .admit(file.size())
                        .map_err(|bound| SkillTreeError::permanent(format!("{context}: selected skills exceed {bound}")))?;
                    admitted.push((path, file));
                }
                TreeEntry::Symlink => {
                    return Err(SkillTreeError::permanent(format!("{context}: {path} is a symlink, which skill trees refuse")))
                }
                TreeEntry::Submodule => {
                    return Err(SkillTreeError::permanent(format!("{context}: {path} is a submodule, which skill trees refuse")))
                }
            }
        }
        let Some((_, skill)) = admitted.iter().find(|(path, _)| path == "SKILL.md") else {
            return Err(SkillTreeError::permanent(format!("{context}: SKILL.md is missing")));
        };
        let skill = skill.read().await.map_err(|error| SkillTreeError::transient(format!("{context}: read SKILL.md: {error}")))?;
        match frontmatter_name(&String::from_utf8_lossy(&skill)) {
            Some(name) if name == entry.name => {}
            Some(name) => {
                return Err(SkillTreeError::permanent(format!("{context}: SKILL.md names {name}, not the frozen {}", entry.name)))
            }
            None => return Err(SkillTreeError::permanent(format!("{context}: SKILL.md has no frontmatter name"))),
        }
        let mut files = Vec::new();
        for (path, file) in admitted {
            let contents = file.read().await.map_err(|error| SkillTreeError::transient(format!("{context}: read {path}: {error}")))?;
            files.push(ArchiveFile { path: format!("{}/{path}", entry.name), executable: file.executable(), contents });
        }
        Ok(files)
    }

    async fn retire_unused_caches(&self) {
        let Some(cutoff) = SystemTime::now().checked_sub(CACHE_RETENTION) else { return };
        if let Err(error) = self.vcs.retire_revision_caches(&self.cache, cutoff).await {
            tracing::warn!(cache = %self.cache.display(), %error, "skill source cache retirement failed");
        }
    }
}

/// What the crew tree may still hold, across every selected skill.
struct Budget {
    files: usize,
    bytes: u64,
}

impl Budget {
    fn admit(&mut self, size: u64) -> Result<(), String> {
        self.files = self.files.checked_sub(1).ok_or_else(|| format!("the {MAX_ENTRIES}-file bound"))?;
        self.bytes = self.bytes.checked_sub(size).ok_or_else(|| format!("the {MAX_TREE_BYTES}-byte bound"))?;
        Ok(())
    }
}

/// Host-side fetches use HTTPS, or a local repository. Other transports would
/// reach for the daemon user's ambient keys. A local repository is read from
/// the daemon host: the operator-controlled supply manifest is trusted to name
/// one (tests and operator mirrors). A credential on a local source is inert,
/// since Git asks a credential helper only over HTTP.
fn check_transport(repository: &str) -> Result<(), String> {
    if repository.starts_with('/') {
        return Ok(());
    }
    match Url::parse(repository) {
        Ok(url) if matches!(url.scheme(), "https" | "file") => Ok(()),
        _ => Err(format!("repository {repository} must be an HTTPS or local repository for host-side fetching")),
    }
}

/// A `0600` token file in a fresh `0700` directory; dropping the directory
/// removes it.
fn private_token_file(token: &str) -> Result<tempfile::TempDir, String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let directory =
        tempfile::Builder::new().prefix("flotilla-skill-token-").tempdir().map_err(|error| format!("create token directory: {error}"))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.path().join("token"))
        .map_err(|error| format!("write token file: {error}"))?;
    file.write_all(token.as_bytes()).map_err(|error| format!("write token file: {error}"))?;
    Ok(directory)
}

/// The `name:` of SKILL.md's frontmatter, by the catalog producer's rule.
fn frontmatter_name(text: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()? != "---" {
        return None;
    }
    let fields = lines.take_while(|line| *line != "---").collect::<Vec<_>>();
    let names = fields
        .iter()
        .filter_map(|line| line.strip_prefix("name:"))
        .map(|value| value.split('#').next().unwrap_or_default().trim().trim_matches(['"', '\'']).to_string())
        .collect::<Vec<_>>();
    match names.as_slice() {
        [name] if !name.is_empty() => Some(name.clone()),
        _ => None,
    }
}

/// Candidate pre-roll probe: each crew aboard each vessel is resolved and
/// fetched on its own, exactly as at launch, without publishing anything.
/// Crews of one vessel selecting different revisions of one source are
/// permitted; each such vessel yields a notice naming the crews' revisions.
pub async fn validate_frozen_vessel_skills(
    trees: &SkillTrees,
    supply: &Path,
    crews: &BTreeMap<String, Vec<SkillCatalogEntry>>,
    declared_credentials: &BTreeMap<String, flotilla_resources::CredentialSpecSpec>,
    credential_tokens: &BTreeMap<String, PathBuf>,
) -> Result<Vec<String>, String> {
    let supply_path = supply.to_path_buf();
    let inspection = tokio::task::spawn_blocking(move || inspect_skill_sources(&supply_path))
        .await
        .map_err(|error| format!("inspect skill supply task: {error}"))??;
    let mut revisions = BTreeMap::<&str, BTreeMap<&str, &str>>::new();
    for (crew, selected) in crews {
        let sources = resolve_frozen_sources(&inspection.sources, selected).map_err(|error| format!("crew {crew}: {error}"))?;
        authorize_frozen_sources(&sources, declared_credentials).map_err(|error| format!("crew {crew}: {error}"))?;
        if !selected.is_empty() {
            let tree = SkillTree { sources, selected: selected.clone() };
            trees.crew_archive(&tree, Some(&TokenFiles(credential_tokens))).await.map_err(|error| format!("crew {crew}: {error}"))?;
        }
        for entry in selected {
            revisions.entry(entry.source.as_str()).or_default().insert(crew.as_str(), entry.revision.as_str());
        }
    }
    Ok(revisions
        .into_iter()
        .filter(|(_, crews)| crews.values().collect::<BTreeSet<_>>().len() > 1)
        .map(|(source, crews)| {
            let crews = crews.iter().map(|(crew, revision)| format!("{crew} at {revision}")).collect::<Vec<_>>().join(", ");
            format!("skill source {source} is selected at different revisions by crews {crews}")
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct SkillBundleManifest {
    schema_version: u32,
    sources: Vec<SkillSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SkillSource {
    pub(crate) name: String,
    pub(crate) repository: String,
    pub(crate) revision: String,
    pub(crate) credential: Option<String>,
    #[serde(default = "default_skill_source_paths")]
    pub(crate) paths: Vec<String>,
}

/// A standing convoy retains its admission pin after a generation roll. The
/// current supply still authorizes the repository, credential and path roots;
/// only the revision comes from the durable selection (ADR 0052).
pub(crate) fn resolve_frozen_sources(
    sources: &[SkillSource],
    selected: &[flotilla_resources::SkillCatalogEntry],
) -> Result<Vec<SkillSource>, String> {
    let mut resolved = BTreeMap::new();
    for entry in selected {
        let source = sources.iter().find(|source| source.name == entry.source);
        let credential = source.and_then(|source| source.credential.as_deref()).unwrap_or("none");
        let context = format!("skill source {} revision {} credential {}", entry.source, entry.revision, credential);
        let resolve = || -> Result<SkillSource, String> {
            let source = source.ok_or_else(|| format!("{context}: source is not declared by this generation"))?;
            if entry.revision.len() != 40 || !entry.revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(format!("{context}: frozen revision must be a full commit SHA"));
            }
            flotilla_resources::validate_skill_ref(&entry.name).map_err(|error| format!("{context}: {error}"))?;
            flotilla_resources::crew_defaults::validate_skill_path(&entry.path).map_err(|error| format!("{context}: {error}"))?;
            if entry.repository != source.repository
                && flotilla_resources::crew_defaults::source_repository_path(&source.repository) != Some(entry.repository.as_str())
            {
                return Err(format!(
                    "{context}: frozen repository {} differs from authorized source {}",
                    entry.repository, source.repository
                ));
            }
            if !source.paths.iter().any(|path| flotilla_resources::crew_defaults::path_within_source(&entry.path, path)) {
                return Err(format!("{context}: frozen skill {} is outside the authorized source paths", entry.path));
            }
            let mut source = source.clone();
            source.revision = entry.revision.clone();
            Ok(source)
        };
        let source = resolve()?;
        if resolved.keys().any(|(name, revision)| name == &source.name && revision != &source.revision) {
            return Err(format!("{context}: one crew cannot select two revisions of the same source"));
        }
        resolved.insert((source.name.clone(), source.revision.clone()), source);
    }
    Ok(resolved.into_values().collect())
}

pub(crate) fn authorize_frozen_sources(
    sources: &[SkillSource],
    declared_credentials: &BTreeMap<String, flotilla_resources::CredentialSpecSpec>,
) -> Result<(), String> {
    let mut credential_repositories = BTreeMap::new();
    for source in sources {
        if let Some(credential) = &source.credential {
            let spec = declared_credentials.get(credential).ok_or_else(|| {
                format!("skill source {} revision {}: credential {credential} has no declared CredentialSpec", source.name, source.revision)
            })?;
            crate::credential::skill_source_repository(credential, spec, &source.repository)
                .map_err(|error| format!("skill source {} revision {}: {error}", source.name, source.revision))?;
            if credential_repositories.insert(credential, &source.repository).is_some_and(|previous| previous != &source.repository) {
                return Err(format!(
                    "skill source {} credential {credential}: one credential cannot be narrowed to multiple source repositories",
                    source.name
                ));
            }
        }
    }
    Ok(())
}

fn default_skill_source_paths() -> Vec<String> {
    vec!["skills".to_string()]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillBundleInspection {
    pub(crate) sources: Vec<SkillSource>,
}

/// Validates the supply side of skill staging only: the manifest must pin an
/// arbitrary, well-formed set of sources and may attach a named credential to
/// each source.
/// There is deliberately no required-skill assertion here — what a given crew
/// receives is resolved at admission from CrewDefaults, Project, and dispatch
/// declarations, and checked against the candidate catalog before a roll.
/// A supply entry names a single path component, a repository without
/// embedded HTTP credentials, an optional single-component credential, and
/// distinct relative path roots without globs or traversal.
fn source_is_valid(source: &SkillSource) -> bool {
    !(source.name.is_empty()
        || source.name == "."
        || source.name == ".."
        || source.name.contains('/')
        || source.name.contains('\\')
        || source.name.chars().any(|character| matches!(character, '\r' | '\n'))
        || source.repository.is_empty()
        || (source.repository.contains("://")
            && Url::parse(&source.repository)
                .map_or(true, |url| url.password().is_some() || (matches!(url.scheme(), "http" | "https") && !url.username().is_empty())))
        || source.credential.as_ref().is_some_and(|credential| {
            credential.is_empty()
                || credential.contains('/')
                || credential.contains('\\')
                || credential.chars().any(|character| matches!(character, '\r' | '\n'))
        })
        || source.paths.is_empty()
        || source.paths.iter().collect::<BTreeSet<_>>().len() != source.paths.len()
        || source.paths.iter().any(|path| {
            path.is_empty()
                || path.starts_with('/')
                || path.ends_with('/')
                || path.contains('\\')
                || path.chars().any(|character| matches!(character, '\r' | '\n' | '*' | '?' | '[' | ']'))
                || path.split('/').any(|component| component.is_empty() || component == "." || component == "..")
        }))
}

pub(crate) fn inspect_skill_sources(source: &Path) -> Result<SkillBundleInspection, String> {
    let manifest_path = source.join(SKILL_BUNDLE_MANIFEST);
    let manifest = std::fs::read_to_string(&manifest_path)
        .map_err(|error| format!("read skill bundle manifest {}: {error}", manifest_path.display()))?;
    let manifest = serde_json::from_str::<SkillBundleManifest>(&manifest)
        .map_err(|error| format!("decode skill bundle manifest {}: {error}", manifest_path.display()))?;
    if manifest.schema_version != 5 || manifest.sources.is_empty() {
        return Err(format!("skill source manifest {} must use schema version 5 and pin at least one source", manifest_path.display()));
    }
    let names = manifest.sources.iter().map(|source| source.name.as_str()).collect::<BTreeSet<_>>();
    if names.len() != manifest.sources.len() || !manifest.sources.iter().all(source_is_valid) {
        return Err(format!("skill source manifest {} has invalid or duplicate source entries", manifest_path.display()));
    }
    if manifest.sources.iter().any(|source| source.revision.len() != 40 || !source.revision.bytes().all(|byte| byte.is_ascii_hexdigit())) {
        return Err(format!("skill source manifest {} must pin every source to a full commit SHA", manifest_path.display()));
    }
    Ok(SkillBundleInspection { sources: manifest.sources })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    pub(crate) fn git(cwd: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("spawn git");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).expect("utf-8").trim().to_string()
    }

    pub(crate) fn write(root: &Path, path: &str, contents: &[u8]) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directories");
        std::fs::write(path, contents).expect("file");
    }

    pub(crate) fn skill_md(name: &str, body: &str) -> Vec<u8> {
        format!("---\nname: {name}\ndescription: test skill\n---\n{body}\n").into_bytes()
    }

    pub(crate) fn commit(repo: &Path, message: &str) -> String {
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-q", "-m", message]);
        git(repo, &["rev-parse", "HEAD"])
    }

    /// A Git repository acting as a skill source, with its pinned revisions.
    pub(crate) struct Source {
        directory: tempfile::TempDir,
    }

    impl Source {
        pub(crate) fn new() -> Self {
            let directory = tempfile::tempdir().expect("source");
            git(directory.path(), &["init", "-q", "-b", "main"]);
            Self { directory }
        }

        pub(crate) fn path(&self) -> &Path {
            self.directory.path()
        }

        fn repository(&self) -> String {
            self.path().to_string_lossy().into_owned()
        }
    }

    /// The generation's supply manifest, authorizing each source at `revision`.
    pub(crate) fn supply(root: &Path, sources: &[(&str, &Source, &str, Option<&str>)]) -> PathBuf {
        let sources = sources
            .iter()
            .map(|(name, source, revision, credential)| {
                serde_json::json!({"name": name, "repository": source.repository(), "revision": revision, "credential": credential})
            })
            .collect::<Vec<_>>();
        let supply = root.join("supply");
        std::fs::create_dir_all(&supply).expect("supply");
        std::fs::write(supply.join(SKILL_BUNDLE_MANIFEST), serde_json::json!({"schema_version": 5, "sources": sources}).to_string())
            .expect("manifest");
        supply
    }

    pub(crate) fn selected(source_name: &str, source: &Source, revision: &str, name: &str) -> SkillCatalogEntry {
        SkillCatalogEntry {
            source: source_name.into(),
            repository: source.repository(),
            revision: revision.into(),
            name: name.into(),
            path: format!("skills/{name}"),
        }
    }

    pub(crate) fn trees(root: &Path) -> SkillTrees {
        SkillTrees::new(
            Arc::new(flotilla_vcs::vcs::FlotillaVcs::object_cache(Arc::new(flotilla_provider_runtime::ProcessCommandRunner))),
            root.join("cache"),
        )
    }

    /// Resolve `selection` against `supply` and build its archive, as delivery does.
    async fn archive(
        trees: &SkillTrees,
        supply: Option<&Path>,
        selection: &[SkillCatalogEntry],
        tokens: Option<&dyn SkillSourceTokens>,
    ) -> Result<SkillArchive, SkillTreeError> {
        trees.crew_archive(&SkillTree::resolve(supply, selection).await?, tokens).await
    }

    /// Unpack as the vessel agent's content cache does: a gzipped tar.
    fn unpack(archive: &SkillArchive) -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("unpack");
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive.bytes.as_slice()));
        tar.set_preserve_permissions(true);
        tar.unpack(directory.path()).expect("unpack archive");
        directory
    }

    pub(crate) fn files(root: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).expect("read") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    found.push(path.strip_prefix(root).expect("relative").to_string_lossy().into_owned());
                }
            }
        }
        found.sort();
        found
    }

    // #3663: the whole crew tree is bounded from tree metadata, before any
    // blob is read: the file past either bound is refused.
    #[test]
    fn the_tree_budget_refuses_the_file_past_either_bound() {
        let mut files = Budget { files: 2, bytes: 10 };
        files.admit(1).expect("first");
        files.admit(1).expect("second");
        files.admit(0).expect_err("third file");
        let mut bytes = Budget { files: 10, bytes: 10 };
        bytes.admit(10).expect("exactly the bound");
        bytes.admit(1).expect_err("one byte over");
    }

    // #3663: a crew's tree holds exactly its frozen selection across two
    // sources, with support files' bytes and executable bits, never another
    // skill the sources happen to carry.
    #[tokio::test]
    async fn the_tree_holds_exactly_the_frozen_selection_with_support_files() {
        let root = tempfile::tempdir().expect("root");
        let (first, second) = (Source::new(), Source::new());
        write(first.path(), "skills/alpha/SKILL.md", &skill_md("alpha", "alpha"));
        write(first.path(), "skills/alpha/bin/run", &[0, 159, 255, 10]);
        std::fs::set_permissions(first.path().join("skills/alpha/bin/run"), std::fs::Permissions::from_mode(0o755)).expect("mode");
        write(first.path(), "skills/unselected/SKILL.md", &skill_md("unselected", "not for this crew"));
        let first_pin = commit(first.path(), "first");
        write(second.path(), "skills/gamma/SKILL.md", &skill_md("gamma", "gamma"));
        let second_pin = commit(second.path(), "second");
        let supply = supply(root.path(), &[("first", &first, &first_pin, None), ("second", &second, &second_pin, None)]);
        let selection = [selected("first", &first, &first_pin, "alpha"), selected("second", &second, &second_pin, "gamma")];

        let archive = archive(&trees(root.path()), Some(&supply), &selection, None).await.expect("archive");
        let tree = unpack(&archive);
        assert_eq!(files(tree.path()), [SKILL_TREE_MANIFEST, "alpha/SKILL.md", "alpha/bin/run", "gamma/SKILL.md"]);
        assert_eq!(std::fs::read(tree.path().join("alpha/bin/run")).expect("binary"), [0, 159, 255, 10]);
        assert_ne!(std::fs::metadata(tree.path().join("alpha/bin/run")).expect("mode").permissions().mode() & 0o100, 0);
        assert_eq!(std::fs::metadata(tree.path().join("gamma/SKILL.md")).expect("mode").permissions().mode() & 0o111, 0);
    }

    // #3663, ADR 0052: a standing convoy keeps its admission pin after the
    // supply moves on; the current supply authorizes the source, the frozen
    // revision chooses the bytes.
    #[tokio::test]
    async fn an_old_pin_is_served_under_the_current_supply() {
        let root = tempfile::tempdir().expect("root");
        let source = Source::new();
        write(source.path(), "skills/alpha/SKILL.md", &skill_md("alpha", "old"));
        let old = commit(source.path(), "old");
        write(source.path(), "skills/alpha/SKILL.md", &skill_md("alpha", "new"));
        let new = commit(source.path(), "new");
        let supply = supply(root.path(), &[("first", &source, &new, None)]);

        let archive =
            archive(&trees(root.path()), Some(&supply), &[selected("first", &source, &old, "alpha")], None).await.expect("archive");
        let tree = unpack(&archive);
        assert_eq!(std::fs::read(tree.path().join("alpha/SKILL.md")).expect("skill"), skill_md("alpha", "old"));
    }

    // #3663: an empty selection is a tree holding only its manifest, so that
    // delivering it removes stale managed skills. It needs no supply.
    #[tokio::test]
    async fn an_empty_selection_is_a_manifest_only_tree() {
        let root = tempfile::tempdir().expect("root");
        let archive = archive(&trees(root.path()), None, &[], None).await.expect("archive");
        assert_eq!(files(unpack(&archive).path()), [SKILL_TREE_MANIFEST]);
    }

    // #3663: the tree policy admits only regular files under a SKILL.md whose
    // frontmatter names the frozen skill. Symlinks, submodules and a
    // mismatched name are permanent refusals; nothing is delivered.
    #[tokio::test]
    async fn the_tree_policy_permanently_refuses_links_submodules_and_misnamed_skills() {
        let root = tempfile::tempdir().expect("root");
        let source = Source::new();
        write(source.path(), "skills/linked/SKILL.md", &skill_md("linked", "linked"));
        std::os::unix::fs::symlink("../../outside", source.path().join("skills/linked/escape")).expect("symlink");
        write(source.path(), "skills/moduled/SKILL.md", &skill_md("moduled", "moduled"));
        write(source.path(), "skills/misnamed/SKILL.md", &skill_md("someone-else", "misnamed"));
        let first = commit(source.path(), "skills");
        git(source.path(), &["update-index", "--add", "--cacheinfo", &format!("160000,{first},skills/moduled/vendor")]);
        git(source.path(), &["commit", "-q", "-m", "submodule"]);
        let pin = git(source.path(), &["rev-parse", "HEAD"]);
        let supply = supply(root.path(), &[("first", &source, &pin, None)]);
        let trees = trees(root.path());

        for name in ["linked", "moduled", "misnamed"] {
            let error = archive(&trees, Some(&supply), &[selected("first", &source, &pin, name)], None).await.expect_err(name);
            assert!(error.permanent, "{name}: {error}");
        }
    }

    /// Counts mints; refuses once `refuse` is set.
    #[derive(Default)]
    struct Minter {
        mints: AtomicUsize,
        refuse: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl SkillSourceTokens for Minter {
        async fn mint(&self, _credential: &str, _repository: &str) -> Result<String, String> {
            self.mints.fetch_add(1, Ordering::SeqCst);
            if self.refuse.load(Ordering::SeqCst) {
                Err("installation no longer grants this repository".into())
            } else {
                Ok("token".into())
            }
        }
    }

    // #3663: a private source's narrowed token is minted at every staging,
    // cache hit or not, so a revoked credential refuses a cached tree.
    #[tokio::test]
    async fn a_private_source_is_authorized_at_every_staging_even_from_cache() {
        let root = tempfile::tempdir().expect("root");
        let source = Source::new();
        write(source.path(), "skills/alpha/SKILL.md", &skill_md("alpha", "alpha"));
        let pin = commit(source.path(), "pin");
        let supply = supply(root.path(), &[("private", &source, &pin, Some("reader"))]);
        let selection = [selected("private", &source, &pin, "alpha")];
        let (trees, minter) = (trees(root.path()), Minter::default());

        archive(&trees, Some(&supply), &selection, Some(&minter)).await.expect("first staging");
        // A cache hit uses no network: the remote is gone, yet the mint still runs.
        let moved = root.path().join("moved-away");
        std::fs::rename(source.path(), &moved).expect("take the remote away");
        archive(&trees, Some(&supply), &selection, Some(&minter)).await.expect("cached staging");
        std::fs::rename(&moved, source.path()).expect("restore the remote");
        assert_eq!(minter.mints.load(Ordering::SeqCst), 2);
        minter.refuse.store(true, Ordering::SeqCst);
        archive(&trees, Some(&supply), &selection, Some(&minter)).await.expect_err("revoked credential refuses the cached tree");
        archive(&trees, Some(&supply), &selection, None).await.expect_err("no credential store, no private tree");
    }

    // #3663 decision 4: crews aboard one vessel may pin different revisions of
    // one source; the probe resolves each crew on its own and reports the
    // vessel as a notice, not a failure.
    #[tokio::test]
    async fn the_vessel_probe_permits_mixed_revisions_and_reports_them() {
        let root = tempfile::tempdir().expect("root");
        let source = Source::new();
        write(source.path(), "skills/alpha/SKILL.md", &skill_md("alpha", "old"));
        let old = commit(source.path(), "old");
        write(source.path(), "skills/alpha/SKILL.md", &skill_md("alpha", "new"));
        let new = commit(source.path(), "new");
        let supply = supply(root.path(), &[("first", &source, &new, None)]);
        let trees = trees(root.path());
        let crews = |reviewer: &str| {
            BTreeMap::from([
                ("coder".to_string(), vec![selected("first", &source, &new, "alpha")]),
                ("reviewer".to_string(), vec![selected("first", &source, reviewer, "alpha")]),
            ])
        };

        let mixed = validate_frozen_vessel_skills(&trees, &supply, &crews(&old), &BTreeMap::new(), &BTreeMap::new()).await.expect("mixed");
        assert_eq!(mixed.len(), 1);
        let same = validate_frozen_vessel_skills(&trees, &supply, &crews(&new), &BTreeMap::new(), &BTreeMap::new()).await.expect("same");
        assert!(same.is_empty());
    }

    fn write_skill_sources(root: &Path) -> PathBuf {
        let skills = root.join("generation/skills");
        std::fs::create_dir_all(&skills).expect("create skill bundle");
        std::fs::write(
            skills.join(SKILL_BUNDLE_MANIFEST),
            r#"{"schema_version":5,"sources":[{"name":"mattpocock-skills","repository":"https://github.com/flotilla-org/mattpocock-skills.git","revision":"1111111111111111111111111111111111111111","credential":"github-skills-fork"},{"name":"rjw-skills","repository":"https://github.com/rjwittams/rjw-skills.git","revision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","paths":["plugins/rjw-sdlc/skills"]}]}"#,
        )
        .expect("write skill bundle manifest");
        skills
    }

    #[test]
    fn skill_manifest_rejects_duplicate_or_traversing_source_names() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let manifest_path = skills.join(SKILL_BUNDLE_MANIFEST);
        let manifest = std::fs::read_to_string(&manifest_path).expect("read fixture manifest");
        std::fs::write(&manifest_path, manifest.replace(r#""name":"rjw-skills""#, r#""name":"mattpocock-skills""#))
            .expect("write duplicate-name fixture manifest");
        let error = inspect_skill_sources(&skills).expect_err("duplicate source name must fail validation");
        assert!(error.contains("invalid or duplicate source entries"), "unexpected validation error: {error}");

        std::fs::write(&manifest_path, manifest.replace(r#""name":"rjw-skills""#, r#""name":"../rjw-skills""#))
            .expect("write traversing-name fixture manifest");
        let error = inspect_skill_sources(&skills).expect_err("path-traversing source name must fail validation");
        assert!(error.contains("invalid or duplicate source entries"), "unexpected validation error: {error}");

        std::fs::write(&manifest_path, manifest.replace("github-skills-fork", "../credential"))
            .expect("write traversing credential fixture manifest");
        let error = inspect_skill_sources(&skills).expect_err("path-traversing credential must fail validation");
        assert!(error.contains("invalid or duplicate source entries"), "unexpected validation error: {error}");
    }

    #[test]
    fn skill_manifest_validates_declared_paths_and_applies_the_default() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let manifest_path = skills.join(SKILL_BUNDLE_MANIFEST);
        let manifest = std::fs::read_to_string(&manifest_path).expect("read fixture manifest");
        let inspection = inspect_skill_sources(&skills).expect("inspect default and declared paths");
        assert_eq!(inspection.sources[0].paths, ["skills"]);
        assert_eq!(inspection.sources[1].paths, ["plugins/rjw-sdlc/skills"]);

        for invalid in [
            "",
            "../skills",
            "plugins/../skills",
            "/skills",
            "skills/",
            r"plugins\skills",
            "skills\nother",
            "skills/*",
            "skills/[ab]",
            "skills?",
        ] {
            let encoded = serde_json::to_string(invalid).expect("encode invalid fixture path");
            std::fs::write(&manifest_path, manifest.replace(r#"["plugins/rjw-sdlc/skills"]"#, &format!("[{encoded}]")))
                .expect("write invalid-path fixture manifest");
            let error = inspect_skill_sources(&skills).expect_err("invalid source path must fail validation");
            assert!(error.contains("invalid or duplicate source entries"), "unexpected validation error for {invalid:?}: {error}");
        }

        std::fs::write(&manifest_path, manifest.replace(r#"["plugins/rjw-sdlc/skills"]"#, r#"["skills","skills"]"#))
            .expect("write duplicate-path fixture manifest");
        let error = inspect_skill_sources(&skills).expect_err("duplicate source paths must fail validation");
        assert!(error.contains("invalid or duplicate source entries"), "unexpected validation error: {error}");
    }

    #[test]
    fn skill_manifest_accepts_any_source_count() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let manifest_path = skills.join(SKILL_BUNDLE_MANIFEST);
        std::fs::write(
            &manifest_path,
            r#"{"schema_version":5,"sources":[{"name":"only","repository":"https://example.com/only.git","revision":"1111111111111111111111111111111111111111"}]}"#,
        )
        .expect("write single-source manifest");
        let inspection = inspect_skill_sources(&skills).expect("single source must validate");
        assert_eq!(inspection.sources.len(), 1);

        std::fs::write(
            &manifest_path,
            r#"{"schema_version":5,"sources":[{"name":"one","repository":"https://example.com/one.git","revision":"1111111111111111111111111111111111111111"},{"name":"two","repository":"https://example.com/two.git","revision":"2222222222222222222222222222222222222222"},{"name":"three","repository":"https://example.com/three.git","revision":"3333333333333333333333333333333333333333"}]}"#,
        )
        .expect("write three-source manifest");
        let inspection = inspect_skill_sources(&skills).expect("three sources must validate");
        assert_eq!(inspection.sources.len(), 3);
    }

    #[test]
    fn skill_manifest_allows_credentials_on_arbitrary_sources() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let manifest_path = skills.join(SKILL_BUNDLE_MANIFEST);
        let manifest = std::fs::read_to_string(&manifest_path).expect("read fixture manifest");

        std::fs::write(
            &manifest_path,
            manifest.replace("https://github.com/flotilla-org/mattpocock-skills.git", "https://github.com/another-owner/private.git"),
        )
        .expect("write relocated credentialed source fixture manifest");
        let inspection = inspect_skill_sources(&skills).expect("credentialed source repository remains manifest data");
        assert_eq!(inspection.sources.len(), 2);
    }

    #[test]
    fn skill_manifest_accepts_ssh_usernames_but_rejects_http_userinfo_and_passwords() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let path = skills.join(SKILL_BUNDLE_MANIFEST);
        let original = std::fs::read_to_string(&path).expect("manifest");
        let repository = "https://github.com/rjwittams/rjw-skills.git";
        std::fs::write(&path, original.replace(repository, "ssh://git@github.com/rjwittams/rjw-skills.git")).expect("SSH manifest");
        inspect_skill_sources(&skills).expect("ordinary SSH username is allowed");
        for invalid in [
            "https://user:pass@github.com/rjwittams/rjw-skills.git",
            "https://user@github.com/rjwittams/rjw-skills.git",
            "ssh://git:pass@github.com/rjwittams/rjw-skills.git",
        ] {
            std::fs::write(&path, original.replace(repository, invalid)).expect("invalid manifest");
            inspect_skill_sources(&skills).expect_err("embedded secret or HTTP userinfo is rejected");
        }
    }

    #[test]
    fn skill_manifest_rejects_unpinned_revisions() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let manifest_path = skills.join(SKILL_BUNDLE_MANIFEST);
        let manifest = std::fs::read_to_string(&manifest_path).expect("read fixture manifest");

        for unpinned in ["main", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaz", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"] {
            std::fs::write(&manifest_path, manifest.replace("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", unpinned))
                .expect("write unpinned fixture manifest");
            let error = inspect_skill_sources(&skills).expect_err("a revision that is not a full SHA must fail validation");
            assert!(error.contains("full commit SHA"), "unexpected validation error for {unpinned}: {error}");
        }
    }

    #[test]
    fn skill_manifest_rejects_a_superseded_schema_version() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let manifest_path = skills.join(SKILL_BUNDLE_MANIFEST);
        let manifest = std::fs::read_to_string(&manifest_path).expect("read fixture manifest");
        std::fs::write(&manifest_path, manifest.replace(r#""schema_version":5"#, r#""schema_version":4"#))
            .expect("write superseded-schema fixture manifest");
        let error = inspect_skill_sources(&skills).expect_err("schema version 4 must fail validation");
        assert!(error.contains("schema version 5"), "unexpected validation error: {error}");

        std::fs::write(&manifest_path, r#"{"schema_version":5,"sources":[]}"#).expect("write empty-source fixture manifest");
        let error = inspect_skill_sources(&skills).expect_err("empty source set must fail validation");
        assert!(error.contains("at least one source"), "unexpected validation error: {error}");
    }

    // #2875: retaining a frozen revision never authorizes another repository,
    // an undeclared source, path traversal, or an unpinned ref. Diagnostics name
    // the source, revision, credential and concrete refusal before any mint.
    #[hegel::test]
    fn frozen_skill_resolution_preserves_source_authority(tc: hegel::TestCase) {
        let case = tc.draw(hegel::generators::integers::<u8>().min_value(0).max_value(5));
        let source = SkillSource {
            name: "private-skills".into(),
            repository: "https://github.com/example/private-skills.git".into(),
            revision: "2".repeat(40),
            credential: Some("private-skills".into()),
            paths: vec!["skills".into()],
        };
        let mut entry = crew_skill("private-source", "skills/private-folder");
        match case {
            0 => entry.repository = "other/private-skills".into(),
            1 => entry.path = "skills/../escape".into(),
            2 => entry.path = "skills-other/private-folder".into(),
            3 => entry.revision = "main".into(),
            4 => entry.source = "undeclared".into(),
            5 => entry.name = "../escape".into(),
            _ => unreachable!(),
        }
        let error = resolve_frozen_sources(&[source], std::slice::from_ref(&entry)).expect_err("untrusted frozen material refused");
        assert!(error.contains(&entry.source) && error.contains(&entry.revision) && error.contains("credential"), "{error}");
        assert!(
            error.contains("differs")
                || error.contains("invalid")
                || error.contains("outside")
                || error.contains("full commit SHA")
                || error.contains("not declared"),
            "{error}"
        );
    }

    fn crew_skill(name: &str, path: &str) -> flotilla_resources::SkillCatalogEntry {
        flotilla_resources::SkillCatalogEntry {
            source: "private-skills".into(),
            repository: "example/private-skills".into(),
            revision: "1".repeat(40),
            name: name.into(),
            path: path.into(),
        }
    }
}
