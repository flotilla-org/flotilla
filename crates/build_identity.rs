use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

// Generate diagnostic identity only for the final binaries. Libraries receive
// it through flotilla_daemon_api::build_info at startup, so workspace edits do not
// invalidate unrelated library crates.
// Protocol compatibility remains governed by the separate protocol fingerprint.

fn git_output(workspace_root: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .current_dir(workspace_root)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| output.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if path.is_file() {
        files.push(path.to_owned());
        return Ok(());
    }

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

fn hash_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// Canonicalize before sorting too: native separator ordering can differ.
fn canonical_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn canonical_files<T>(files: impl IntoIterator<Item = (String, T)>) -> Vec<(String, T)> {
    let mut files: Vec<_> = files.into_iter().map(|(name, value)| (canonical_path(&name), value)).collect();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

fn hash_file(mut hash: u64, path: &str, contents: &[u8]) -> u64 {
    hash = hash_bytes(hash, canonical_path(path).as_bytes());
    hash = hash_bytes(hash, &[0]);
    // Preserve lone CR bytes and arbitrary binary bytes; only CRLF is normalized.
    for (index, byte) in contents.iter().enumerate() {
        if *byte != b'\r' || contents.get(index + 1) != Some(&b'\n') {
            hash = hash_bytes(hash, &[*byte]);
        }
    }
    hash_bytes(hash, &[0xff])
}

fn source_fingerprint(workspace_root: &Path) -> Result<String, String> {
    // Deliberately fingerprint the whole compiled workspace rather than a
    // hand-maintained dependency subset: dirty inputs must change diagnostics,
    // while only the final executables are invalidated by this fingerprint.
    let crates_dir = workspace_root.join("crates");
    let mut inputs = vec![
        workspace_root.join("Cargo.lock"),
        workspace_root.join("Cargo.toml"),
        workspace_root.join("build.rs"),
        workspace_root.join("src"),
        workspace_root.join("assets"),
        crates_dir.join("build_identity.rs"),
    ];
    for entry in fs::read_dir(&crates_dir).map_err(|error| format!("cannot list {}: {error}", crates_dir.display()))? {
        let path = entry.map_err(|error| format!("cannot list {}: {error}", crates_dir.display()))?.path();
        if path.is_dir() {
            let manifest = path.join("Cargo.toml");
            if manifest.is_file() {
                inputs.push(manifest);
            }
            let source = path.join("src");
            if source.is_dir() {
                inputs.push(source);
            }
        }
    }
    let mut files = Vec::new();
    for input in &inputs {
        println!("cargo::rerun-if-changed={}", input.display());
        collect_files(input, &mut files).map_err(|error| format!("cannot collect {}: {error}", input.display()))?;
    }
    let files: Vec<_> = files
        .into_iter()
        .map(|path| {
            let relative = path.strip_prefix(workspace_root).unwrap_or(&path);
            let name = relative.to_str().ok_or_else(|| format!("non-UTF-8 source path: {}", relative.display()))?;
            Ok((name.to_owned(), path))
        })
        .collect::<Result<_, String>>()?;

    let mut hash = FNV_OFFSET_BASIS;
    for (relative, path) in canonical_files(files) {
        let contents = fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        hash = hash_file(hash, &relative, &contents);
    }
    Ok(format!("{hash:016x}"))
}

fn main() {
    println!("cargo::rerun-if-env-changed=FLOTILLA_BUILD_ID");

    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.as_path();
    if let Some(head) = git_output(workspace_root, &["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo::rerun-if-changed={head}");
    }
    if let Some(reference) = git_output(workspace_root, &["symbolic-ref", "-q", "HEAD"]) {
        if let Some(reference_path) = git_output(workspace_root, &["rev-parse", "--git-path", &reference]) {
            println!("cargo::rerun-if-changed={reference_path}");
        }
    }

    let build_id = std::env::var("FLOTILLA_BUILD_ID").ok().filter(|value| !value.is_empty()).unwrap_or_else(|| {
        let revision = git_output(workspace_root, &["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
        let fingerprint =
            source_fingerprint(workspace_root).unwrap_or_else(|error| panic!("cannot fingerprint executable sources: {error}"));
        format!("{revision}+{fingerprint}")
    });
    println!("cargo::rustc-env=FLOTILLA_BUILD_ID={build_id}");
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour: identical source trees have identical fingerprints across path
    // separators and checkout line endings, including empty and binary contents.
    #[test]
    fn checkout_platform_does_not_change_fingerprint() {
        let contents: &[&[u8]] = &[b"", b"\n", b"a\nb\n", b"a\nb", b"\r", b"\r\r", b"\xff\0\n"];
        for content in contents {
            let crlf: Vec<_> = content.iter().flat_map(|byte| if *byte == b'\n' { vec![b'\r', b'\n'] } else { vec![*byte] }).collect();
            let unix = [("src/a0.rs", *content), ("src/a/b.rs", b"other\n".as_slice())];
            let windows = [("src\\a0.rs", crlf.as_slice()), ("src\\a\\b.rs", b"other\r\n".as_slice())];
            let fingerprint = |files: &[(&str, &[u8])]| {
                let files = canonical_files(files.iter().map(|(name, bytes)| ((*name).to_owned(), *bytes)));
                files.iter().fold(FNV_OFFSET_BASIS, |hash, (path, bytes)| hash_file(hash, path, bytes))
            };
            assert_eq!(fingerprint(&unix), fingerprint(&windows));
        }
    }

    // Behaviour: meaningful changes still distinguish dirty source trees.
    #[test]
    fn source_changes_remain_distinct() {
        let original = hash_file(FNV_OFFSET_BASIS, "src/a.rs", b"a\n");
        assert_ne!(original, hash_file(FNV_OFFSET_BASIS, "src/b.rs", b"a\n"));
        assert_ne!(original, hash_file(FNV_OFFSET_BASIS, "src/a.rs", b"b\n"));
        assert_ne!(hash_file(FNV_OFFSET_BASIS, "a", b"\r"), hash_file(FNV_OFFSET_BASIS, "a", b""));
    }
}
