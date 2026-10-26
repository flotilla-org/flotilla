//! Provider-owned generation delivery. Mount/copy stable real directories,
//! preserving relative links. Never mount a resolved generation or mask the
//! image's sibling harnesses. Publish a complete tree before flipping current.
//! What is published, and where, comes from the layer's composition
//! declaration ([`crate::composition`]); nothing here names a tool.
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

use crate::composition::{Composition, Source};

/// A complete verified payload: its declaration and the tree its members'
/// generation paths are relative to. No image or Docker dependency is involved.
#[derive(Clone, Copy)]
pub struct Payload<'a> {
    pub composition: &'a Composition,
    pub root: &'a Path,
}

impl<'a> From<&'a crate::guest_tools::GuestTools> for Payload<'a> {
    fn from(tools: &'a crate::guest_tools::GuestTools) -> Self {
        Self { composition: tools.composition(), root: tools.root() }
    }
}

/// Cache generation publication by source file identity. One publisher is
/// shared by the daemon's tool factory across all vessels and reconciles.
#[derive(Default)]
pub struct Publisher {
    published: std::sync::Mutex<Option<(String, PathBuf)>>,
}
impl Publisher {
    pub fn publish(&self, root: &Path, source: Payload<'_>) -> io::Result<PathBuf> {
        self.publish_with(root, source, |input, output, hash, expected| copy_asset(input, output, hash, expected))
    }

    fn publish_with(
        &self,
        root: &Path,
        source: Payload<'_>,
        transfer: impl FnMut(&mut fs::File, &mut fs::File, &mut Sha256, u64) -> io::Result<()>,
    ) -> io::Result<PathBuf> {
        use std::os::unix::fs::MetadataExt;
        let mut cached = self.published.lock().map_err(|_| io::Error::other("tool publisher lock poisoned"))?;
        let mut identity = Sha256::new();
        identity.update(root.as_os_str().as_encoded_bytes());
        for (name, path) in source_files(source)? {
            let metadata = fs::metadata(&path)?;
            for path in [&name, &path] {
                identity.update((path.as_os_str().as_encoded_bytes().len() as u64).to_le_bytes());
                identity.update(path.as_os_str().as_encoded_bytes());
            }
            for value in [
                metadata.dev(),
                metadata.ino(),
                metadata.len(),
                u64::from(metadata.mode()),
                metadata.mtime() as u64,
                metadata.mtime_nsec() as u64,
                metadata.ctime() as u64,
                metadata.ctime_nsec() as u64,
            ] {
                identity.update(value.to_le_bytes());
            }
        }
        let identity = format!("{:x}", identity.finalize());
        if let Some((previous, target)) = cached.as_ref() {
            if previous == &identity {
                let _lock = publication_lock(root)?;
                if published_links_intact(root, target, source.composition) {
                    clear_staging(root)?;
                    return Ok(target.clone());
                }
            }
        }
        let target = publish_with(root, source, transfer)?;
        *cached = Some((identity, target.clone()));
        Ok(target)
    }
}

/// The relative target of a stable entry point at `link` (root-relative) for
/// the published path `path` inside the current generation.
fn current_target(link: &Path, path: &Path) -> PathBuf {
    let depth = link.parent().map_or(0, |parent| parent.components().count());
    let mut target = PathBuf::new();
    for _ in 0..depth {
        target.push("..");
    }
    target.join("tools/current").join(path)
}

/// Every stable entry point of a published generation: one link per published
/// file, and one per top-level entry of each published tree.
fn entry_points(target: &Path, composition: &Composition) -> io::Result<Vec<PathBuf>> {
    let mut links = Vec::new();
    for (member, install) in composition.published() {
        match &member.source {
            Source::File(_) => links.push(PathBuf::from(install)),
            Source::Tree(_) => {
                for entry in fs::read_dir(target.join(install))? {
                    links.push(Path::new(install).join(entry?.file_name()));
                }
            }
        }
    }
    links.sort();
    Ok(links)
}

fn published_links_intact(root: &Path, target: &Path, composition: &Composition) -> bool {
    let matches = |path: PathBuf, expected: &Path| fs::read_link(path).is_ok_and(|actual| actual == expected);
    if !target.is_dir() || !matches(root.join("tools/current"), Path::new(target.file_name().expect("generation"))) {
        return false;
    }
    let Ok(links) = entry_points(target, composition) else { return false };
    links.iter().all(|link| matches(root.join(link), &current_target(link, link)))
}

fn source_files(source: Payload<'_>) -> io::Result<Vec<(PathBuf, PathBuf)>> {
    let mut files = Vec::new();
    for (member, install) in source.composition.published() {
        match &member.source {
            Source::File(file) => files.push((PathBuf::from(install), source.root.join(file))),
            Source::Tree(tree) => {
                collect(&source.root.join(tree), Path::new(install), &mut files)?;
                for required in &member.requires {
                    let name = Path::new(install).join(&required.path);
                    if !files.iter().any(|(published, _)| published == &name) {
                        return Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("{} lacks required entry {}", member.role, required.path),
                        ));
                    }
                }
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Stage a complete immutable generation before publishing it atomically.
/// A provider-root filesystem lock serializes publication and pruning across
/// daemon restarts and concurrent provisioning requests.
pub fn publish(root: &Path, source: Payload<'_>) -> io::Result<PathBuf> {
    publish_with(root, source, |input, output, hash, expected| copy_asset(input, output, hash, expected))
}

fn publish_with(
    root: &Path,
    source: Payload<'_>,
    mut transfer: impl FnMut(&mut fs::File, &mut fs::File, &mut Sha256, u64) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let _lock = publication_lock(root)?;
    clear_staging(root)?;
    let files = source_files(source)?;
    fs::create_dir_all(root.join("tools"))?;
    let staging = root.join("tools").join(format!(".staging-{}", uuid::Uuid::new_v4()));
    // Stream each immutable staged snapshot into the framed digest. Memory is
    // bounded by one buffer rather than the size of the entire release.
    let result = (|| -> io::Result<PathBuf> {
        let mut hash = Sha256::new();
        for (name, path) in &files {
            let destination = staging.join(name);
            fs::create_dir_all(destination.parent().expect("asset parent"))?;
            let mut source = fs::File::open(path)?;
            let metadata = source.metadata()?;
            let mut output = fs::File::create(&destination)?;
            let name = name.to_string_lossy();
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            use std::os::unix::fs::PermissionsExt;
            hash.update(metadata.permissions().mode().to_le_bytes());
            hash.update(metadata.len().to_le_bytes());
            transfer(&mut source, &mut output, &mut hash, metadata.len())?;
            fs::set_permissions(&destination, metadata.permissions())?;
        }
        let generation = format!("{:x}", hash.finalize());
        let target = root.join("tools").join(generation);
        if target.exists() {
            fs::remove_dir_all(&staging)?;
        } else {
            fs::rename(&staging, &target)?;
        }
        Ok(target)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    let target = result?;
    let generation = target.file_name().expect("generation name");
    // Mount these real directories separately, leaving image-owned harnesses
    // under /opt/flotilla visible. Relative links resolve on both host and guest.
    let exposed = source.composition.exposed_directories();
    for directory in &exposed {
        fs::create_dir_all(root.join(directory))?;
    }
    let links = entry_points(&target, source.composition)?;
    for path in &links {
        let parent = root.join(path.parent().unwrap_or(Path::new("")));
        link(&parent, path.file_name().expect("entry point name"), &current_target(path, path))?;
    }
    link(&root.join("tools"), "current", Path::new(generation))?;
    // Remove retired entry points only after current is published; active
    // readers still resolve old entries until the atomic generation flip. A
    // retired member's directory may no longer be exposed, so walk the root.
    let links = links.into_iter().collect::<BTreeSet<_>>();
    remove_retired_links(root, Path::new(""), &links)?;
    Ok(target)
}

/// Remove every entry point into `tools/current` under `relative` that the
/// current declaration does not publish. Never follows links.
fn remove_retired_links(root: &Path, relative: &Path, current: &BTreeSet<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let path = relative.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() && path != Path::new("tools") {
            remove_retired_links(root, &path, current)?;
        } else if kind.is_symlink()
            && !current.contains(&path)
            && fs::read_link(entry.path())?.starts_with(current_target(&path, Path::new("")))
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn copy_asset(mut source: impl io::Read, mut output: impl io::Write, hash: &mut Sha256, expected: u64) -> io::Result<()> {
    let mut buffer = [0_u8; 64 * 1024];
    let mut copied = 0_u64;
    loop {
        let length = source.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        output.write_all(&buffer[..length])?;
        hash.update(&buffer[..length]);
        copied += length as u64;
    }
    if copied != expected {
        return Err(io::Error::other("tools asset changed length during publication"));
    }
    Ok(())
}

// Only call while holding the publication lock: no live publisher can own
// these intermediates, including after a daemon restart.
fn clear_staging(root: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(root.join("tools")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(".staging-") && entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn publication_lock(root: &Path) -> io::Result<fs::File> {
    fs::create_dir_all(root)?;
    let file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(root.join(".lock"))?;
    file.lock()?;
    Ok(file)
}

fn link(root: &Path, name: impl AsRef<std::ffi::OsStr>, destination: &Path) -> io::Result<()> {
    let name = name.as_ref();
    let temporary = root.join(format!(".link-{}", uuid::Uuid::new_v4()));
    std::os::unix::fs::symlink(destination, &temporary)?;
    if let Err(error) = fs::rename(&temporary, root.join(name)) {
        let _ = fs::remove_file(temporary);
        return Err(error);
    }
    Ok(())
}

fn collect(source: &Path, relative: &Path, files: &mut Vec<(PathBuf, PathBuf)>) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = relative.join(entry.file_name());
        if kind.is_dir() {
            collect(&entry.path(), &name, files)?;
        } else if kind.is_file() {
            files.push((name, entry.path()));
        } else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "tools assets must be regular files"));
        }
    }
    Ok(())
}

/// Prune only generations proven unused by the provider. Failed or incomplete
/// observations must return false. In particular an old cleat retaining sessions
/// retains its executable and library generation. `current` is always protected.
pub fn prune(root: &Path, unused: impl FnMut(&Path) -> bool) -> io::Result<()> {
    let _lock = publication_lock(root)?;
    prune_locked(root, unused)
}

fn prune_locked(root: &Path, mut unused: impl FnMut(&Path) -> bool) -> io::Result<()> {
    clear_staging(root)?;
    let current = match fs::read_link(root.join("tools/current")) {
        Ok(current) => current,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in fs::read_dir(root.join("tools"))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if Path::new(&entry.file_name()) != current && unused(&entry.path()) {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

/// Local Docker shares the host PID namespace's kernel file identities even
/// when paths differ across mounts. Keep every generation with an executable,
/// mapped library, or open descriptor in a process of the provider's host UID.
/// Docker runs vessels with that UID; unrelated root/system processes must not
/// prevent collection by a non-root provider. Incomplete visibility
/// refuses pruning, preserving sessions rather than guessing from build SHAs.
#[cfg(target_os = "linux")]
pub fn prune_local(root: &Path) -> io::Result<()> {
    // Local Docker's host_user() runs vessel tools with this same real UID.
    // SAFETY: getuid is a side-effect-free process identity query.
    let owner = unsafe { libc::getuid() };
    prune_local_for(root, Path::new("/proc"), owner)
}

#[cfg(target_os = "linux")]
fn prune_local_for(root: &Path, processes: &Path, owner: u32) -> io::Result<()> {
    use std::{collections::HashSet, os::unix::fs::MetadataExt};
    // Observe and collect under the publication lock, so the protected current
    // generation cannot change between the process snapshot and deletion.
    let _lock = publication_lock(root)?;
    let mut used = HashSet::new();
    for entry in fs::read_dir(processes)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let process = entry.path();
        match fs::metadata(&process) {
            Ok(metadata) if metadata.uid() != owner => continue,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Ok(()),
        }
        let inspect = (|| -> io::Result<()> {
            let metadata = fs::metadata(process.join("exe"))?;
            used.insert((metadata.dev(), metadata.ino()));
            let maps = fs::read_to_string(process.join("maps"))?;
            for line in maps.lines() {
                let fields: Vec<_> = line.split_whitespace().take(5).collect();
                if fields.len() != 5 {
                    return Err(io::Error::other("invalid process maps"));
                }
                let Some((major, minor)) = fields[3].split_once(':') else {
                    return Err(io::Error::other("invalid maps device"));
                };
                if let (Ok(major), Ok(minor), Ok(inode)) =
                    (u32::from_str_radix(major, 16), u32::from_str_radix(minor, 16), fields[4].parse::<u64>())
                {
                    used.insert((libc::makedev(major, minor), inode));
                } else {
                    return Err(io::Error::other("invalid maps identity"));
                }
            }
            for descriptor in fs::read_dir(process.join("fd"))? {
                match fs::metadata(descriptor?.path()) {
                    Ok(metadata) => {
                        used.insert((metadata.dev(), metadata.ino()));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        })();
        if inspect.is_err() {
            // Exited processes and kernel threads lack exe. If maps still has
            // content, inability to inspect it means a live unknown user.
            match fs::read_to_string(process.join("maps")) {
                Ok(maps) if maps.is_empty() => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                _ => return Ok(()),
            }
        }
    }
    prune_locked(root, |generation| {
        let mut files = Vec::new();
        if collect(generation, Path::new(""), &mut files).is_err() {
            return false;
        }
        files.iter().all(|(_, path)| fs::metadata(path).is_ok_and(|metadata| !used.contains(&(metadata.dev(), metadata.ino()))))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source tree laid out by the shipped declaration's generation paths.
    fn write_source(source: &Path, contents: &[u8]) {
        for file in
            ["bin/flotilla-vessel", "bin/cleat", "lib/libghostty-vt.so.0", "share/terminfo/x/xterm-ghostty", "share/terminfo/LICENSE"]
        {
            fs::create_dir_all(source.join(file).parent().unwrap()).unwrap();
            fs::write(source.join(file), contents).unwrap();
        }
    }

    struct Assets {
        root: PathBuf,
        flotilla: PathBuf,
        cleat: PathBuf,
        terminfo: PathBuf,
    }
    impl Assets {
        fn new(source: &Path) -> Self {
            Self {
                root: source.into(),
                flotilla: source.join("bin/flotilla-vessel"),
                cleat: source.join("bin/cleat"),
                terminfo: source.join("share/terminfo"),
            }
        }
        fn payload(&self) -> Payload<'_> {
            Payload { composition: crate::composition::flotilla_tools(), root: &self.root }
        }
    }
    // Scenario: repeated preparation across vessels reuses the published hash
    // without transferring binary bytes. Equal-size replacement invalidates it;
    // removing a stable entry point still causes publication to repair it.
    #[test]
    fn publisher_caches_file_identity_and_repairs_missing_links() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("provider");
        let source = temp.path().join("source");
        write_source(&source, b"old");
        let assets = Assets::new(&source);
        let publisher = Publisher::default();
        let transfers = std::cell::Cell::new(0);
        let transfer = |input: &mut fs::File, output: &mut fs::File, hash: &mut Sha256, expected| {
            transfers.set(transfers.get() + 1);
            copy_asset(input, output, hash, expected)
        };
        let first = publisher.publish_with(&root, assets.payload(), transfer).unwrap();
        assert_eq!(transfers.get(), 5);
        let abandoned = root.join("tools/.staging-interrupted");
        fs::create_dir_all(&abandoned).unwrap();
        fs::write(abandoned.join("partial"), b"interrupted").unwrap();
        assert_eq!(publisher.publish_with(&root, assets.payload(), transfer).unwrap(), first);
        assert!(!abandoned.exists(), "cached publication still reclaims abandoned staging under the root lock");
        assert_eq!(transfers.get(), 5, "unchanged preparation never rereads binary content");
        let replacement = source.join("replacement");
        fs::write(&replacement, b"new").unwrap();
        fs::rename(replacement, &assets.flotilla).unwrap();
        let updated = publisher.publish_with(&root, assets.payload(), transfer).unwrap();
        assert_ne!(first, updated);
        assert_eq!(fs::read(root.join("bin/flotilla")).unwrap(), b"new");
        assert_eq!(transfers.get(), 10);
        fs::remove_file(root.join("bin/flotilla")).unwrap();
        assert_eq!(publisher.publish_with(&root, assets.payload(), transfer).unwrap(), updated);
        assert_eq!(fs::read(root.join("bin/flotilla")).unwrap(), b"new");
        assert_eq!(transfers.get(), 15);
    }

    // #3874 acceptance: a tool and a companion tree added only to the
    // declaration are published through the same entry-point rules, and a
    // later declaration without them retires their entry points.
    #[test]
    fn declared_additions_publish_without_publisher_changes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("provider");
        let source = temp.path().join("source");
        write_source(&source, b"tools");
        fs::create_dir_all(source.join("share/extra/data")).unwrap();
        fs::write(source.join("bin/extra"), b"extra").unwrap();
        fs::write(source.join("share/extra/data/manifest"), b"companion").unwrap();
        let mut extended = serde_json::to_value(crate::composition::flotilla_tools()).unwrap();
        let members = extended["members"].as_array_mut().unwrap();
        members.push(serde_json::json!({"role": "extra", "file": "bin/extra", "install": "bin/extra"}));
        members.push(serde_json::json!({"role": "extra-data", "tree": "share/extra", "install": "share/extra", "requires": [{"path": "data/manifest"}]}));
        let extended = crate::composition::Composition::parse(&extended.to_string()).unwrap();
        let publisher = Publisher::default();
        publisher.publish(&root, Payload { composition: &extended, root: &source }).unwrap();
        assert_eq!(fs::read_link(root.join("bin/extra")).unwrap(), Path::new("../tools/current/bin/extra"));
        assert_eq!(fs::read(root.join("bin/extra")).unwrap(), b"extra");
        assert_eq!(fs::read_link(root.join("share/extra/data")).unwrap(), Path::new("../../tools/current/share/extra/data"));
        assert_eq!(fs::read(root.join("share/extra/data/manifest")).unwrap(), b"companion");
        assert_eq!(fs::read(root.join("bin/flotilla")).unwrap(), b"tools");
        fs::remove_file(source.join("share/extra/data/manifest")).unwrap();
        let error = publish(&root, Payload { composition: &extended, root: &source }).unwrap_err();
        assert!(error.to_string().contains("extra-data lacks required entry data/manifest"), "{error}");
        publisher.publish(&root, Assets::new(&source).payload()).unwrap();
        assert!(fs::symlink_metadata(root.join("bin/extra")).is_err(), "retired entry point removed");
        assert!(fs::symlink_metadata(root.join("share/extra/data")).is_err(), "retired companion removed");
        assert_eq!(fs::read(root.join("bin/cleat")).unwrap(), b"tools");
    }

    // Mode-only rolls must publish the requested executable permissions.
    // Abandoned crash intermediates are collected by publication and pruning.
    #[test]
    fn mode_changes_publish_new_generations_and_crash_staging_is_collected() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("provider");
        let source = temp.path().join("source");
        write_source(&source, b"unchanged");
        let assets = Assets::new(&source);
        fs::set_permissions(&assets.cleat, fs::Permissions::from_mode(0o644)).unwrap();
        let previous = publish(&root, assets.payload()).unwrap();
        let crashed = root.join("tools/.staging-crashed");
        fs::create_dir_all(&crashed).unwrap();
        fs::write(crashed.join("partial"), b"partial").unwrap();
        fs::set_permissions(&assets.cleat, fs::Permissions::from_mode(0o755)).unwrap();
        let current = publish(&root, assets.payload()).unwrap();
        assert_ne!(previous, current);
        assert_eq!(fs::metadata(root.join("bin/cleat")).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(!crashed.exists());
        fs::create_dir_all(&crashed).unwrap();
        prune(&root, |_| false).unwrap();
        assert!(!crashed.exists());
        assert!(previous.is_dir());
        assert!(current.is_dir());
    }

    // Wrap real file readers at the transfer seam to deterministically model
    // shrink/growth after metadata, plus an I/O failure after a staged copy.
    // None may flip current or leave behind partial staging directories.
    #[test]
    fn streaming_failures_preserve_current_and_remove_staging() {
        use io::Read;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("provider");
        let source = temp.path().join("source");
        write_source(&source, b"initial");
        let assets = Assets::new(&source);
        let current = publish(&root, assets.payload()).unwrap();
        for failure in ["shrink", "grow", "io"] {
            fs::write(&assets.cleat, failure).unwrap();
            let mut count = 0;
            let error = publish_with(&root, assets.payload(), |input, output, hash, expected| {
                count += 1;
                if count != 2 {
                    return copy_asset(input, output, hash, expected);
                }
                match failure {
                    "shrink" => copy_asset(input.take(expected - 1), output, hash, expected),
                    "grow" => copy_asset(input.chain(io::Cursor::new(b"extra")), output, hash, expected),
                    _ => {
                        copy_asset(input, output, hash, expected)?;
                        Err(io::Error::other("injected transfer failure"))
                    }
                }
            })
            .unwrap_err();
            assert_eq!(count, 2, "failure must occur after staging begins");
            assert!(error.to_string().contains(if failure == "io" { "injected transfer failure" } else { "changed length" }));
            assert_eq!(fs::read_link(root.join("tools/current")).unwrap(), current.strip_prefix(root.join("tools")).unwrap());
            assert_eq!(fs::read(root.join("bin/cleat")).unwrap(), b"initial");
            let entries: Vec<_> = fs::read_dir(root.join("tools")).unwrap().map(|entry| entry.unwrap().file_name()).collect();
            assert_eq!(entries.len(), 2, "only current and its complete generation may remain: {entries:?}");
        }
    }

    // An empty/failed first installation is safe to prune. Later publication
    // removes retired terminfo entries without disturbing the current database.
    #[test]
    fn empty_publication_and_retired_terminfo_entries_are_cleaned_up() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("provider");
        prune(&root, |_| panic!("no generation was published")).unwrap();
        let source = temp.path().join("source");
        write_source(&source, b"contents");
        let assets = Assets::new(&source);
        publish(&root, assets.payload()).unwrap();
        fs::write(source.join("share/terminfo/retired"), b"contents").unwrap();
        publish(&root, assets.payload()).unwrap();
        assert!(root.join("share/terminfo/retired").is_file());
        fs::remove_file(source.join("share/terminfo/retired")).unwrap();
        publish(&root, assets.payload()).unwrap();
        assert!(fs::symlink_metadata(root.join("share/terminfo/retired")).is_err());
        assert!(root.join("share/terminfo/LICENSE").is_file());
        assert_eq!(fs::read(root.join("share/terminfo/x/xterm-ghostty")).unwrap(), b"contents");
    }

    // Generated rolls span duplicate publications, missing assets, changing
    // bytes, active leases and lease release. Every step preserves the current
    // complete generation and pruning conserves all leased generations.
    #[hegel::test]
    fn generated_rolls_preserve_current_and_leased_generations(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        write_source(&source, b"initial");
        let root = temp.path().join("provider");
        let assets = Assets::new(&source);
        let mut current = publish(&root, assets.payload()).unwrap();
        let mut leases = vec![current.clone()];
        let length = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
        for index in 0..length {
            match tc.draw(gs::integers::<u8>().min_value(0).max_value(3)) {
                0 => assert_eq!(publish(&root, assets.payload()).unwrap(), current),
                1 => {
                    fs::write(&assets.cleat, format!("build-{index}")).unwrap();
                    current = publish(&root, assets.payload()).unwrap();
                    leases.push(current.clone());
                }
                2 => {
                    fs::remove_file(assets.terminfo.join("x/xterm-ghostty")).unwrap();
                    assert!(publish(&root, assets.payload()).is_err());
                    fs::write(assets.terminfo.join("x/xterm-ghostty"), b"initial").unwrap();
                }
                _ => {
                    leases.retain(|lease| lease == &current);
                }
            }
            prune(&root, |generation| !leases.iter().any(|lease| lease == generation)).unwrap();
            assert!(leases.iter().all(|lease| lease.exists()));
            assert_eq!(fs::read_link(root.join("tools/current")).unwrap(), current.strip_prefix(root.join("tools")).unwrap());
            assert_eq!(fs::read(root.join("bin/cleat")).unwrap(), fs::read(&assets.cleat).unwrap());
            assert!(root.join("share/terminfo/x/xterm-ghostty").is_file());
        }
    }

    // Concurrent provisioning requests must converge on a complete reusable
    // generation, with no partial staging directories exposed as current.
    #[test]
    fn concurrent_publishers_reuse_one_complete_generation() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        write_source(&source, b"candidate");
        let root = temp.path().join("provider");
        let assets = Assets::new(&source);
        let barrier = std::sync::Barrier::new(4);
        let generations = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        publish(&root, assets.payload()).unwrap()
                    })
                })
                .collect();
            threads.into_iter().map(|thread| thread.join().unwrap()).collect::<Vec<_>>()
        });
        assert!(generations.iter().all(|generation| generation == &generations[0]));
        assert_eq!(fs::read_dir(root.join("tools")).unwrap().count(), 2);
        assert_eq!(fs::read(root.join("bin/cleat")).unwrap(), b"candidate");
        assert_eq!(fs::read(root.join("share/terminfo/x/xterm-ghostty")).unwrap(), b"candidate");
    }

    // Process boundary: a synthetic /proc tree supplies kernel observations.
    // Real file identities protect an old generation even when mapped paths
    // differ; unknown owned processes defer collection, unrelated UIDs do not.
    #[cfg(target_os = "linux")]
    #[test]
    fn local_collection_tracks_file_identities_and_refuses_incomplete_owned_observations() {
        use std::os::unix::fs::MetadataExt;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        write_source(&source, b"old");
        let root = temp.path().join("provider");
        let assets = Assets::new(&source);
        let old = publish(&root, assets.payload()).unwrap();
        fs::write(&assets.cleat, b"new").unwrap();
        let new = publish(&root, assets.payload()).unwrap();
        let processes = temp.path().join("proc");
        let process = processes.join("123");
        fs::create_dir_all(process.join("fd")).unwrap();
        let owner = fs::metadata(&process).unwrap().uid();
        std::os::unix::fs::symlink(new.join("bin/cleat"), process.join("exe")).unwrap();
        let library = fs::metadata(old.join("lib/libghostty-vt.so.0")).unwrap();
        fs::write(
            process.join("maps"),
            format!(
                "0-1 r--p 0 {:x}:{:x} {} /different/container/path\n",
                libc::major(library.dev()),
                libc::minor(library.dev()),
                library.ino()
            ),
        )
        .unwrap();
        prune_local_for(&root, &processes, owner).unwrap();
        assert!(old.exists());
        fs::write(process.join("maps"), "").unwrap();
        std::os::unix::fs::symlink(old.join("bin/flotilla"), process.join("fd/9")).unwrap();
        prune_local_for(&root, &processes, owner).unwrap();
        assert!(old.exists());
        fs::remove_file(process.join("fd/9")).unwrap();
        fs::write(process.join("maps"), "invalid nonempty observation").unwrap();
        prune_local_for(&root, &processes, owner).unwrap();
        assert!(old.exists());
        prune_local_for(&root, &processes, owner.wrapping_add(1)).unwrap();
        assert!(!old.exists());
        assert!(new.exists());
    }

    // A roll exposes a complete new generation through the stable parent while
    // leased generations survive pruning; failures leave the current link intact.
    #[test]
    fn roll_is_atomic_and_pruning_respects_live_users() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        write_source(&source, b"old");
        let root = temp.path().join("provider");
        let assets = Assets::new(&source);
        let old = publish(&root, assets.payload()).unwrap();
        assert_eq!(publish(&root, assets.payload()).unwrap(), old);
        fs::write(&assets.cleat, b"new").unwrap();
        let new = publish(&root, assets.payload()).unwrap();
        assert_ne!(old, new);
        assert_eq!(fs::read(root.join("bin/cleat")).unwrap(), b"new");
        assert_eq!(fs::read(root.join("share/terminfo/x/xterm-ghostty")).unwrap(), b"old");
        prune(&root, |_| false).unwrap();
        assert!(old.exists());
        fs::remove_file(assets.terminfo.join("x/xterm-ghostty")).unwrap();
        assert!(publish(&root, assets.payload()).is_err());
        assert_eq!(fs::read_link(root.join("tools/current")).unwrap(), new.strip_prefix(root.join("tools")).unwrap());
        prune(&root, |_| true).unwrap();
        assert!(!old.exists());
        assert!(new.exists());
    }
}
