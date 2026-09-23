use std::{
    fmt::Write,
    fs,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

fn collect_files(root: &Path, path: &Path, files: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    for entry in fs::read_dir(path).map_err(|error| format!("cannot list {}: {error}", path.display()))? {
        let entry = entry.map_err(|error| format!("cannot read entry in {}: {error}", path.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, files)?;
        } else if path.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| format!("cannot make {} relative to {}: {error}", path.display(), root.display()))?;
            let relative = relative
                .components()
                .map(|component| component.as_os_str().to_str().ok_or_else(|| format!("non-UTF-8 protocol path: {}", relative.display())))
                .collect::<Result<Vec<_>, _>>()?
                .join("/");
            files.push((relative, path));
        }
    }
    Ok(())
}

pub fn fingerprint_protocol_source(source_root: &Path) -> Result<String, String> {
    let mut files = Vec::new();
    collect_files(source_root, source_root, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut hasher = Sha256::new();
    for (relative, path) in files {
        let contents = fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        hasher.update((relative.len() as u64).to_be_bytes());
        hasher.update(relative.as_bytes());
        hasher.update((contents.len() as u64).to_be_bytes());
        hasher.update(contents);
    }

    let mut fingerprint = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(fingerprint, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(fingerprint)
}
