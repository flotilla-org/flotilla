//! Publish a digest-named tree beside a destination and flip the destination's
//! link to it in one `rename(2)` (#3663, `docs/implementation-choices/skill-tree-delivery.md`).
//!
//! Readers resolving the destination see the complete old tree or the complete
//! new one. Nothing here knows what the tree holds: the caller supplies a
//! canonical archive, its sha256 and the name of a file every complete tree
//! contains. Until drydock#84 lands, every environment uses the runner store;
//! the agent's streamed digest-checked unpack replaces its transfer steps.
//!
//! ```text
//! <parent>/.flotilla-<name>/<digest>/              complete, never modified
//! <parent>/.flotilla-<name>/<digest>.partial-<id>/ an attempt's unpack
//! <parent>/<name> -> .flotilla-<name>/<digest>     what readers resolve
//! <parent>/<name>.prev -> .flotilla-<name>/<old>   retention record only
//! ```

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_provider_runtime::{ChannelLabel, CommandRunner};
use sha2::Digest;

/// Minutes an entry must be untouched before a later publication reaps it.
/// Every attempt touches its tree, so an in-flight attempt is never reaped
/// unless it has stalled this long, and its final verification catches that.
pub const REAP_AFTER_MINUTES: u32 = 60;
const ATTEMPTS: u32 = 3;

/// A canonical archive of one tree, named by the sha256 of its bytes.
#[derive(Debug, Clone)]
pub struct TreeArchive {
    pub digest: String,
    pub bytes: Arc<Vec<u8>>,
    /// A file every complete tree contains; its presence marks completion.
    pub marker: String,
}

/// One regular file of a tree, by its `/`-separated path within the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveFile {
    pub path: String,
    pub executable: bool,
    pub contents: Vec<u8>,
}

impl TreeArchive {
    /// The canonical uncompressed POSIX tar of `files`: entries in component
    /// order with their parent directories, only directories and regular
    /// files, modes 0700 or 0600, uid/gid 0, mtime 0 and no owner names. The
    /// bytes, and so the digest, depend only on the paths, modes and contents.
    pub fn build(files: Vec<ArchiveFile>, marker: &str) -> Result<Self, String> {
        let mut entries = std::collections::BTreeMap::<Vec<String>, Option<ArchiveFile>>::new();
        for file in files {
            let components = file.path.split('/').map(str::to_string).collect::<Vec<_>>();
            if components.iter().any(|component| component.is_empty() || component == "." || component == "..") {
                return Err(format!("archive path {:?} is not a relative path of normal components", file.path));
            }
            for depth in 1..components.len() {
                entries.entry(components[..depth].to_vec()).or_insert(None);
            }
            if entries.insert(components, Some(file.clone())).is_some() {
                return Err(format!("archive path {:?} is duplicated or also a directory", file.path));
            }
        }
        if !entries.contains_key(&vec![marker.to_string()]) {
            return Err(format!("archive lacks its completion marker {marker}"));
        }
        let mut bytes = Vec::new();
        for (components, file) in &entries {
            let name = components.join("/");
            match file {
                Some(file) => {
                    // Keys are in component order, so any entry below this
                    // path is the very next one.
                    let below = entries.range::<Vec<String>, _>((std::ops::Bound::Excluded(components), std::ops::Bound::Unbounded)).next();
                    if below.is_some_and(|(other, _)| other.starts_with(components)) {
                        return Err(format!("archive path {name:?} is both a file and a directory"));
                    }
                    let mode = if file.executable { 0o700 } else { 0o600 };
                    bytes.extend(tar_header(&name, mode, file.contents.len() as u64, b'0')?);
                    bytes.extend(&file.contents);
                    bytes.resize(bytes.len().div_ceil(512) * 512, 0);
                }
                None => bytes.extend(tar_header(&format!("{name}/"), 0o700, 0, b'5')?),
            }
        }
        bytes.resize(bytes.len() + 1024, 0);
        let digest = sha2::Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Self { digest, bytes: Arc::new(bytes), marker: marker.to_string() })
    }
}

fn tar_header(name: &str, mode: u32, size: u64, kind: u8) -> Result<[u8; 512], String> {
    fn octal(field: &mut [u8], value: u64) {
        let text = format!("{value:0width$o}", width = field.len() - 1);
        field[..text.len()].copy_from_slice(text.as_bytes());
    }
    let mut header = [0u8; 512];
    // ustar splits long names at a `/` into a 155-byte prefix and a 100-byte name.
    let (prefix, base) = if name.len() <= 100 {
        ("", name)
    } else {
        let split = name
            .char_indices()
            .filter(|(index, character)| *character == '/' && *index <= 155 && name.len() - index - 1 <= 100 && index + 1 < name.len())
            .map(|(index, _)| index)
            .next()
            .ok_or_else(|| format!("archive path {name:?} is too long for a ustar header"))?;
        (&name[..split], &name[split + 1..])
    };
    header[..base.len()].copy_from_slice(base.as_bytes());
    octal(&mut header[100..108], u64::from(mode));
    octal(&mut header[108..116], 0);
    octal(&mut header[116..124], 0);
    octal(&mut header[124..136], size);
    octal(&mut header[136..148], 0);
    header[156] = kind;
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    octal(&mut header[329..337], 0);
    octal(&mut header[337..345], 0);
    header[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    header[148..156].fill(b' ');
    let checksum = header.iter().map(|byte| u64::from(*byte)).sum::<u64>();
    let text = format!("{checksum:06o}\0 ");
    header[148..156].copy_from_slice(text.as_bytes());
    Ok(header)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishError {
    /// Retrying cannot help: the guest lacks a tool, or the destination is
    /// neither a link nor a directory.
    pub permanent: bool,
    pub message: String,
}

impl PublishError {
    pub fn transient(message: impl Into<String>) -> Self {
        Self { permanent: false, message: message.into() }
    }

    fn permanent(message: impl Into<String>) -> Self {
        Self { permanent: true, message: message.into() }
    }
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Missing,
    Directory,
    Link(String),
    Other,
}

/// The guest file operations publication needs, each one atomic on its own.
/// `rename` has `rename(2)` semantics: it replaces a link or file, but fails
/// onto a non-empty directory.
#[async_trait]
pub trait TreeStore: Send + Sync {
    /// The entry itself, without following a link.
    async fn entry(&self, path: &Path) -> Result<Entry, PublishError>;
    /// Whether the path resolves, following links.
    async fn exists(&self, path: &Path) -> Result<bool, PublishError>;
    async fn create_private_dir(&self, path: &Path) -> Result<(), PublishError>;
    async fn upload(&self, bytes: &[u8], path: &Path) -> Result<(), PublishError>;
    async fn sha256(&self, path: &Path) -> Result<String, PublishError>;
    async fn extract(&self, archive: &Path, into: &Path) -> Result<(), PublishError>;
    async fn rename(&self, from: &Path, to: &Path) -> Result<(), PublishError>;
    async fn symlink(&self, target: &str, at: &Path) -> Result<(), PublishError>;
    async fn touch(&self, path: &Path) -> Result<(), PublishError>;
    /// Names of the directory's entries untouched for more than `minutes`.
    async fn stale_entries(&self, directory: &Path, minutes: u32) -> Result<Vec<String>, PublishError>;
    async fn remove(&self, path: &Path) -> Result<(), PublishError>;
}

struct Layout {
    destination: PathBuf,
    store: PathBuf,
    previous: PathBuf,
    store_name: String,
}

impl Layout {
    fn new(destination: &Path) -> Result<Self, PublishError> {
        let parent = destination.parent().ok_or_else(|| PublishError::permanent("publication destination has no parent"))?;
        let name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| PublishError::permanent("publication destination needs a UTF-8 name"))?;
        let store_name = format!(".flotilla-{name}");
        Ok(Self {
            destination: destination.to_path_buf(),
            store: parent.join(&store_name),
            previous: parent.join(format!("{name}.prev")),
            store_name,
        })
    }

    fn target(&self, digest: &str) -> String {
        format!("{}/{digest}", self.store_name)
    }

    fn digest_of<'a>(&self, target: &'a str) -> Option<&'a str> {
        target.strip_prefix(&self.store_name)?.strip_prefix('/')
    }
}

/// Whether the destination already resolves to this archive's complete tree.
pub async fn is_published(store: &dyn TreeStore, destination: &Path, archive: &TreeArchive) -> Result<bool, PublishError> {
    resolves_to(store, &Layout::new(destination)?, archive).await
}

/// The completion record: a link naming the archive's tree, which resolves.
async fn resolves_to(store: &dyn TreeStore, layout: &Layout, archive: &TreeArchive) -> Result<bool, PublishError> {
    Ok(store.entry(&layout.destination).await? == Entry::Link(layout.target(&archive.digest))
        && store.exists(&layout.destination.join(&archive.marker)).await?)
}

/// Make `destination` resolve to the archive's tree. Every attempt writes only
/// uniquely named entries until its final rename, and every writer of one
/// selection publishes the same digest, so concurrent or abandoned attempts
/// converge. Transient failures restart from the beginning with a new attempt.
pub async fn publish(store: &dyn TreeStore, destination: &Path, archive: &TreeArchive) -> Result<(), PublishError> {
    let layout = Layout::new(destination)?;
    let mut attempt = 1;
    loop {
        match publish_once(store, &layout, archive).await {
            Err(error) if !error.permanent && attempt < ATTEMPTS => {
                tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
                attempt += 1;
            }
            result => return result,
        }
    }
}

async fn publish_once(store: &dyn TreeStore, layout: &Layout, archive: &TreeArchive) -> Result<(), PublishError> {
    let target = layout.target(&archive.digest);
    // An ambiguous earlier activation is reconciled by identity alone.
    if resolves_to(store, layout, archive).await? {
        return Ok(());
    }
    let current = store.entry(&layout.destination).await?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let tree = layout.store.join(&archive.digest);
    store.create_private_dir(&layout.store).await?;
    if store.exists(&tree.join(&archive.marker)).await? {
        store.touch(&tree).await?;
    } else {
        let partial = layout.store.join(format!("{}.partial-{id}", archive.digest));
        let upload = layout.store.join(format!("{}.partial-{id}.tar", archive.digest));
        let unpacked = async {
            store.upload(&archive.bytes, &upload).await?;
            // Check the received bytes before anything is extracted from them.
            if store.sha256(&upload).await? != archive.digest {
                return Err(PublishError::transient(format!("received archive does not match digest {}", archive.digest)));
            }
            store.create_private_dir(&partial).await?;
            store.extract(&upload, &partial).await?;
            if !store.exists(&partial.join(&archive.marker)).await? {
                return Err(PublishError::transient(format!("extracted tree {} lacks {}", archive.digest, archive.marker)));
            }
            // Fails onto a competing writer's complete tree of the same
            // digest; the retry then finds that tree and uses it.
            store.rename(&partial, &tree).await
        }
        .await;
        // This attempt's own leftovers; the reap removes any these miss.
        for leftover in [&upload, &partial] {
            if unpacked.is_err() || leftover == &upload {
                let _ = store.remove(leftover).await;
            }
        }
        unpacked?;
    }
    // Reap before the flip, so that nothing this attempt does follows its
    // completion check. The new tree is protected by having just been
    // touched or created; the current and previous trees by their links.
    // Reaping is housekeeping: its failure never fails a publication.
    if let Err(error) = reap(store, layout).await {
        tracing::warn!(store = %layout.store.display(), %error, "reaping published trees failed; continuing");
    }
    match &current {
        // Recorded before the flip: a crash between the two only protects
        // the still-current tree from reaping.
        Entry::Link(old) if *old != target => {
            let link = layout.store.join(format!("prev-{id}"));
            store.symlink(old, &link).await?;
            store.rename(&link, &layout.previous).await?;
        }
        // The previous stager's real directory. The destination is absent
        // until the link below exists: the one stated exception, once per
        // vessel that predates this layout.
        Entry::Directory => store.rename(&layout.destination, &layout.store.join(format!("legacy-{id}"))).await?,
        Entry::Other => return Err(PublishError::permanent(format!("{} is neither a link nor a directory", layout.destination.display()))),
        Entry::Missing | Entry::Link(_) => {}
    }
    let link = layout.store.join(format!("link-{id}"));
    store.symlink(&target, &link).await?;
    store.rename(&link, &layout.destination).await?;
    // The completion record: a matching link whose tree still resolves. A
    // concurrent reaper can win between the touch and the flip.
    if !resolves_to(store, layout, archive).await? {
        return Err(PublishError::transient(format!("{} does not resolve to tree {}", layout.destination.display(), archive.digest)));
    }
    Ok(())
}

/// Remove everything in the store that neither link names and that nobody has
/// touched within [`REAP_AFTER_MINUTES`]: older trees, abandoned partials,
/// leaked temporary links and legacy directories. Nothing is removed unless
/// both links were read; a failed removal does not stop the others.
async fn reap(store: &dyn TreeStore, layout: &Layout) -> Result<(), PublishError> {
    let mut protected = BTreeSet::new();
    for link in [&layout.destination, &layout.previous] {
        if let Entry::Link(target) = store.entry(link).await? {
            protected.extend(layout.digest_of(&target).map(str::to_string));
        }
    }
    let mut failed = None;
    for name in store.stale_entries(&layout.store, REAP_AFTER_MINUTES).await? {
        if !protected.contains(&name) {
            if let Err(error) = store.remove(&layout.store.join(name)).await {
                failed = Some(error);
            }
        }
    }
    failed.map_or(Ok(()), Err)
}

/// Publication through an environment's command runner, using only tools in
/// GNU coreutils and BusyBox: `readlink`, `test`, `mkdir`, `sha256sum`,
/// `tar -o`, `mv -T`, `ln -s`, `touch`, `find -mmin` and `rm`.
pub struct RunnerTreeStore {
    runner: Arc<dyn CommandRunner>,
}

impl RunnerTreeStore {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    async fn output(&self, cmd: &str, args: &[&str]) -> Result<flotilla_provider_runtime::CommandOutput, PublishError> {
        let output = self
            .runner
            .run_output(cmd, args, Path::new("/"), &ChannelLabel::Default)
            .await
            .map_err(|error| PublishError::transient(format!("run {cmd}: {error}")))?;
        if output.exit_code == Some(127) {
            return Err(PublishError::permanent(format!("the guest lacks `{cmd}`, which skill publication requires")));
        }
        Ok(output)
    }

    async fn run(&self, cmd: &str, args: &[&str]) -> Result<String, PublishError> {
        let output = self.output(cmd, args).await?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(PublishError::transient(format!("{cmd} failed: {}", output.stderr.trim())))
        }
    }

    async fn test(&self, flag: &str, path: &Path) -> Result<bool, PublishError> {
        let output = self.output("test", &[flag, &path.to_string_lossy()]).await?;
        match output.exit_code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(PublishError::transient(format!("test {flag} {}: {}", path.display(), output.stderr.trim()))),
        }
    }
}

#[async_trait]
impl TreeStore for RunnerTreeStore {
    async fn entry(&self, path: &Path) -> Result<Entry, PublishError> {
        let text = path.to_string_lossy();
        let link = self.output("readlink", &["--", &text]).await?;
        if link.success() {
            return Ok(Entry::Link(link.stdout.trim_end_matches('\n').to_string()));
        }
        if self.test("-d", path).await? {
            Ok(Entry::Directory)
        } else if self.test("-e", path).await? {
            Ok(Entry::Other)
        } else {
            Ok(Entry::Missing)
        }
    }

    async fn exists(&self, path: &Path) -> Result<bool, PublishError> {
        self.test("-e", path).await
    }

    async fn create_private_dir(&self, path: &Path) -> Result<(), PublishError> {
        self.run("mkdir", &["-p", "-m", "700", "--", &path.to_string_lossy()]).await.map(drop)
    }

    async fn upload(&self, bytes: &[u8], path: &Path) -> Result<(), PublishError> {
        let source = tempfile::NamedTempFile::new().map_err(|error| PublishError::transient(format!("stage archive: {error}")))?;
        tokio::fs::write(source.path(), bytes).await.map_err(|error| PublishError::transient(format!("stage archive: {error}")))?;
        self.runner
            .write_file_from(source.path(), path)
            .await
            .map_err(|error| PublishError::transient(format!("upload archive to {}: {error}", path.display())))
    }

    async fn sha256(&self, path: &Path) -> Result<String, PublishError> {
        let output = self.run("sha256sum", &["--", &path.to_string_lossy()]).await?;
        output.split_whitespace().next().map(str::to_string).ok_or_else(|| PublishError::transient("sha256sum printed no digest"))
    }

    async fn extract(&self, archive: &Path, into: &Path) -> Result<(), PublishError> {
        self.run("tar", &["-xof", &archive.to_string_lossy(), "-C", &into.to_string_lossy()]).await.map(drop)
    }

    async fn rename(&self, from: &Path, to: &Path) -> Result<(), PublishError> {
        self.run("mv", &["-Tf", "--", &from.to_string_lossy(), &to.to_string_lossy()]).await.map(drop)
    }

    async fn symlink(&self, target: &str, at: &Path) -> Result<(), PublishError> {
        self.run("ln", &["-s", "--", target, &at.to_string_lossy()]).await.map(drop)
    }

    async fn touch(&self, path: &Path) -> Result<(), PublishError> {
        self.run("touch", &["-c", "--", &path.to_string_lossy()]).await.map(drop)
    }

    async fn stale_entries(&self, directory: &Path, minutes: u32) -> Result<Vec<String>, PublishError> {
        let age = format!("+{minutes}");
        let output = self.run("find", &[&directory.to_string_lossy(), "-mindepth", "1", "-maxdepth", "1", "-mmin", &age]).await?;
        Ok(output.lines().filter_map(|line| Path::new(line).file_name()?.to_str().map(str::to_string)).collect())
    }

    async fn remove(&self, path: &Path) -> Result<(), PublishError> {
        self.run("rm", &["-rf", "--", &path.to_string_lossy()]).await.map(drop)
    }
}

#[cfg(test)]
#[path = "tree_publication_tests.rs"]
mod tests;
