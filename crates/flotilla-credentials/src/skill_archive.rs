//! The canonical archive of one crew's skill tree (#3663), named by the sha256 of
//! its bytes. The vessel agent's content cache stores it by that digest and unpacks
//! it as a gzipped tar; equal trees give equal bytes, so a repeat is a cache hit.

use std::{collections::BTreeMap, io::Write, ops::Bound, path::Path};

use flate2::{write::GzEncoder, Compression, GzBuilder};
use sha2::{Digest, Sha256};

/// One regular file of a tree, by its `/`-separated path within the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveFile {
    pub path: String,
    pub executable: bool,
    pub contents: Vec<u8>,
}

/// A canonical `tar.gz` of one tree, and the sha256 of those bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillArchive {
    pub sha256: String,
    pub bytes: Vec<u8>,
}

impl SkillArchive {
    /// Entries in component order, each file after its parent directories; only
    /// directories and regular files, modes 0700 or 0600, owner 0, mtime 0 and no
    /// owner names, in a gzip stream with no name or time. The bytes, and so the
    /// digest, depend only on the paths, modes and contents, never on input order.
    pub fn build(files: Vec<ArchiveFile>) -> Result<Self, String> {
        let mut entries = BTreeMap::<Vec<String>, Option<ArchiveFile>>::new();
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
        let gzip = GzBuilder::new().mtime(0).write(Vec::new(), Compression::default());
        let mut tar = tar::Builder::new(gzip);
        let failed = |error: std::io::Error| format!("build skill archive: {error}");
        for (components, file) in &entries {
            let name = components.join("/");
            let mut header = tar::Header::new_gnu();
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            match file {
                Some(file) => {
                    // Keys are in component order, so any entry below this path is the next one.
                    let below = entries.range::<Vec<String>, _>((Bound::Excluded(components), Bound::Unbounded)).next();
                    if below.is_some_and(|(other, _)| other.starts_with(components)) {
                        return Err(format!("archive path {name:?} is both a file and a directory"));
                    }
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_mode(if file.executable { 0o700 } else { 0o600 });
                    header.set_size(file.contents.len() as u64);
                    tar.append_data(&mut header, Path::new(&name), file.contents.as_slice()).map_err(failed)?;
                }
                None => {
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_mode(0o700);
                    header.set_size(0);
                    tar.append_data(&mut header, Path::new(&name), std::io::empty()).map_err(failed)?;
                }
            }
        }
        let gzip: GzEncoder<Vec<u8>> = tar.into_inner().map_err(failed)?;
        let bytes = gzip.finish().map_err(failed)?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        Ok(Self { sha256, bytes })
    }

    /// The archive as a private temporary file on this host, for the agent's push;
    /// removed when the returned path is dropped.
    pub fn to_temp_file(&self) -> Result<tempfile::TempPath, String> {
        let mut file = tempfile::NamedTempFile::new().map_err(|error| format!("stage skill archive: {error}"))?;
        file.write_all(&self.bytes).map_err(|error| format!("stage skill archive: {error}"))?;
        Ok(file.into_temp_path())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn unpacked(archive: &SkillArchive) -> Vec<(String, u32, Vec<u8>)> {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive.bytes.as_slice()));
        tar.entries()
            .expect("entries")
            .map(|entry| {
                let mut entry = entry.expect("entry");
                let mut contents = Vec::new();
                entry.read_to_end(&mut contents).expect("contents");
                (entry.path().expect("path").display().to_string(), entry.header().mode().expect("mode"), contents)
            })
            .collect()
    }

    // Behaviour: the digest names the tree, not how it was listed. Any order of the
    // same files gives the same digest; changing one file's bytes or its executable
    // bit gives another. The bytes unpack to exactly those files, long paths included.
    #[hegel::test(test_cases = 200)]
    fn the_digest_is_the_trees_alone(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
        let mut files = (0..count)
            .map(|index| {
                let depth = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
                let directories =
                    (0..depth).map(|level| format!("d{level}-{}", "x".repeat(tc.draw(gs::integers::<usize>().max_value(60)))));
                let path = directories.chain([format!("f{index}")]).collect::<Vec<_>>().join("/");
                let contents = tc.draw(gs::vecs(gs::integers::<u8>()).max_size(64));
                ArchiveFile { path, executable: tc.draw(gs::booleans()), contents }
            })
            .collect::<Vec<_>>();
        let archive = SkillArchive::build(files.clone()).expect("archive");
        let entries = unpacked(&archive);
        for file in &files {
            let mode = if file.executable { 0o700 } else { 0o600 };
            assert!(entries.contains(&(file.path.clone(), mode, file.contents.clone())), "{} unpacks as built", file.path);
        }
        let mut shuffled = files.clone();
        let rotation = tc.draw(gs::integers::<usize>().max_value(count - 1));
        shuffled.rotate_left(rotation);
        shuffled.reverse();
        assert_eq!(SkillArchive::build(shuffled).expect("archive").sha256, archive.sha256, "input order");
        let changed = tc.draw(gs::integers::<usize>().max_value(count - 1));
        files[changed].executable = !files[changed].executable;
        assert_ne!(SkillArchive::build(files.clone()).expect("archive").sha256, archive.sha256, "mode");
        files[changed].executable = !files[changed].executable;
        files[changed].contents.push(0);
        assert_ne!(SkillArchive::build(files).expect("archive").sha256, archive.sha256, "content");
    }
}
