//! Provider-owned generation delivery. Mount/copy stable real directories,
//! preserving relative links. Never mount a resolved generation or mask the
//! image's sibling harnesses. Publish a complete tree before flipping current.
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Complete candidate assets. No image or Docker dependency is involved.
pub struct ToolsSource<'a> {
    pub flotilla: &'a Path,
    pub cleat: &'a Path,
    pub library: &'a Path,
    pub terminfo: &'a Path,
}

/// Stage a complete immutable generation before publishing it atomically.
/// A provider-root filesystem lock serializes publication and pruning across
/// daemon restarts and concurrent provisioning requests.
pub fn publish(root: &Path, source: &ToolsSource<'_>) -> io::Result<PathBuf> {
    let _lock = publication_lock(root)?;
    let mut files = vec![
        (PathBuf::from("bin/flotilla"), source.flotilla.to_path_buf()),
        (PathBuf::from("bin/cleat"), source.cleat.to_path_buf()),
        (PathBuf::from("lib/libghostty-vt.so.0"), source.library.to_path_buf()),
    ];
    collect(source.terminfo, Path::new("share/terminfo"), &mut files)?;
    if !files.iter().any(|(name, _)| name == Path::new("share/terminfo/x/xterm-ghostty")) {
        return Err(io::Error::new(io::ErrorKind::NotFound, "cleat release lacks xterm-ghostty terminfo"));
    }
    files.sort();
    fs::create_dir_all(root.join("tools"))?;
    let staging = root.join("tools").join(format!(".staging-{}", uuid::Uuid::new_v4()));
    // Stream each immutable staged snapshot into the framed digest. Memory is
    // bounded by one buffer rather than the size of the entire release.
    let result = (|| -> io::Result<PathBuf> {
        use io::{Read, Write};
        let mut hash = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        for (name, path) in &files {
            let destination = staging.join(name);
            fs::create_dir_all(destination.parent().expect("asset parent"))?;
            let mut source = fs::File::open(path)?;
            let metadata = source.metadata()?;
            let mut output = fs::File::create(&destination)?;
            let name = name.to_string_lossy();
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            hash.update(metadata.len().to_le_bytes());
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
            if copied != metadata.len() {
                return Err(io::Error::other("tools asset changed length during publication"));
            }
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
    for directory in ["bin", "lib", "share/terminfo"] {
        fs::create_dir_all(root.join(directory))?;
    }
    for name in ["flotilla", "cleat"] {
        link(&root.join("bin"), name, &Path::new("../tools/current/bin").join(name))?;
    }
    link(&root.join("lib"), "libghostty-vt.so.0", Path::new("../tools/current/lib/libghostty-vt.so.0"))?;
    for entry in fs::read_dir(target.join("share/terminfo"))? {
        let entry = entry?;
        let name = entry.file_name();
        link(&root.join("share/terminfo"), &name, &Path::new("../../tools/current/share/terminfo").join(&name))?;
    }
    link(&root.join("tools"), "current", Path::new(generation))?;
    // Remove retired top-level links only after current is published; active
    // readers still resolve old entries until the atomic generation flip.
    for entry in fs::read_dir(root.join("share/terminfo"))? {
        let entry = entry?;
        if !target.join("share/terminfo").join(entry.file_name()).exists() && entry.file_type()?.is_symlink() {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(target)
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
    // An empty/failed first installation is safe to prune. Later publication
    // removes retired terminfo entries without disturbing the current database.
    #[test]
    fn empty_publication_and_retired_terminfo_entries_are_cleaned_up() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("provider");
        prune(&root, |_| panic!("no generation was published")).unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("terminfo/x")).unwrap();
        for file in ["flotilla", "cleat", "library", "terminfo/x/xterm-ghostty", "terminfo/LICENSE"] {
            fs::write(source.join(file), b"contents").unwrap();
        }
        let assets = ToolsSource {
            flotilla: &source.join("flotilla"),
            cleat: &source.join("cleat"),
            library: &source.join("library"),
            terminfo: &source.join("terminfo"),
        };
        publish(&root, &assets).unwrap();
        assert!(root.join("share/terminfo/LICENSE").is_file());
        fs::remove_file(source.join("terminfo/LICENSE")).unwrap();
        publish(&root, &assets).unwrap();
        assert!(fs::symlink_metadata(root.join("share/terminfo/LICENSE")).is_err());
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
        fs::create_dir_all(source.join("terminfo/x")).unwrap();
        for file in ["flotilla", "cleat", "lib", "terminfo/x/xterm-ghostty"] {
            fs::write(source.join(file), b"initial").unwrap();
        }
        let root = temp.path().join("provider");
        let assets = ToolsSource {
            flotilla: &source.join("flotilla"),
            cleat: &source.join("cleat"),
            library: &source.join("lib"),
            terminfo: &source.join("terminfo"),
        };
        let mut current = publish(&root, &assets).unwrap();
        let mut leases = vec![current.clone()];
        let length = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
        for index in 0..length {
            match tc.draw(gs::integers::<u8>().min_value(0).max_value(3)) {
                0 => assert_eq!(publish(&root, &assets).unwrap(), current),
                1 => {
                    fs::write(assets.cleat, format!("build-{index}")).unwrap();
                    current = publish(&root, &assets).unwrap();
                    leases.push(current.clone());
                }
                2 => {
                    fs::remove_file(assets.terminfo.join("x/xterm-ghostty")).unwrap();
                    assert!(publish(&root, &assets).is_err());
                    fs::write(assets.terminfo.join("x/xterm-ghostty"), b"initial").unwrap();
                }
                _ => {
                    leases.retain(|lease| lease == &current);
                }
            }
            prune(&root, |generation| !leases.iter().any(|lease| lease == generation)).unwrap();
            assert!(leases.iter().all(|lease| lease.exists()));
            assert_eq!(fs::read_link(root.join("tools/current")).unwrap(), current.strip_prefix(root.join("tools")).unwrap());
            assert_eq!(fs::read(root.join("bin/cleat")).unwrap(), fs::read(assets.cleat).unwrap());
            assert!(root.join("share/terminfo/x/xterm-ghostty").is_file());
        }
    }

    // Concurrent provisioning requests must converge on a complete reusable
    // generation, with no partial staging directories exposed as current.
    #[test]
    fn concurrent_publishers_reuse_one_complete_generation() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("terminfo/x")).unwrap();
        for file in ["flotilla", "cleat", "lib", "terminfo/x/xterm-ghostty"] {
            fs::write(source.join(file), b"candidate").unwrap();
        }
        let root = temp.path().join("provider");
        let assets = ToolsSource {
            flotilla: &source.join("flotilla"),
            cleat: &source.join("cleat"),
            library: &source.join("lib"),
            terminfo: &source.join("terminfo"),
        };
        let barrier = std::sync::Barrier::new(4);
        let generations = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        publish(&root, &assets).unwrap()
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
        fs::create_dir_all(source.join("terminfo/x")).unwrap();
        for file in ["flotilla", "cleat", "lib", "terminfo/x/xterm-ghostty"] {
            fs::write(source.join(file), b"old").unwrap();
        }
        let root = temp.path().join("provider");
        let assets = ToolsSource {
            flotilla: &source.join("flotilla"),
            cleat: &source.join("cleat"),
            library: &source.join("lib"),
            terminfo: &source.join("terminfo"),
        };
        let old = publish(&root, &assets).unwrap();
        fs::write(assets.cleat, b"new").unwrap();
        let new = publish(&root, &assets).unwrap();
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
        fs::create_dir_all(source.join("terminfo/x")).unwrap();
        for file in ["flotilla", "cleat", "lib", "terminfo/x/xterm-ghostty"] {
            fs::write(source.join(file), b"old").unwrap();
        }
        let root = temp.path().join("provider");
        let assets = ToolsSource {
            flotilla: &source.join("flotilla"),
            cleat: &source.join("cleat"),
            library: &source.join("lib"),
            terminfo: &source.join("terminfo"),
        };
        let old = publish(&root, &assets).unwrap();
        assert_eq!(publish(&root, &assets).unwrap(), old);
        fs::write(assets.cleat, b"new").unwrap();
        let new = publish(&root, &assets).unwrap();
        assert_ne!(old, new);
        assert_eq!(fs::read(root.join("bin/cleat")).unwrap(), b"new");
        assert_eq!(fs::read(root.join("share/terminfo/x/xterm-ghostty")).unwrap(), b"old");
        prune(&root, |_| false).unwrap();
        assert!(old.exists());
        fs::remove_file(assets.terminfo.join("x/xterm-ghostty")).unwrap();
        assert!(publish(&root, &assets).is_err());
        assert_eq!(fs::read_link(root.join("tools/current")).unwrap(), new.strip_prefix(root.join("tools")).unwrap());
        prune(&root, |_| true).unwrap();
        assert!(!old.exists());
        assert!(new.exists());
    }
}
