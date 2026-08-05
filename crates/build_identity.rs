use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

// Git revision alone does not identify binaries built from dirty or otherwise
// divergent source trees. Both halves of the socket protocol use this shared
// helper so their advertised generation covers the actual compiled inputs too.

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

fn source_fingerprint(workspace_root: &Path) -> Result<String, String> {
    // Deliberately fingerprint the whole compiled workspace rather than a
    // hand-maintained dependency subset: over-invalidation is safe under the
    // no-compat policy, while omitting an indirect wire input is not.
    let crates_dir = workspace_root.join("crates");
    let mut inputs = vec![
        workspace_root.join("Cargo.lock"),
        workspace_root.join("Cargo.toml"),
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
    files.sort();

    let mut hash = FNV_OFFSET_BASIS;
    for path in files {
        let relative = path.strip_prefix(workspace_root).unwrap_or(&path);
        hash = hash_bytes(hash, relative.as_os_str().as_encoded_bytes());
        hash = hash_bytes(hash, &[0]);
        let contents = fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        hash = hash_bytes(hash, &contents);
        hash = hash_bytes(hash, &[0xff]);
    }
    Ok(format!("{hash:016x}"))
}

fn main() {
    println!("cargo::rerun-if-env-changed=FLOTILLA_BUILD_ID");

    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().and_then(Path::parent).expect("crate is nested directly beneath workspace crates/");
    if let Some(head) = git_output(workspace_root, &["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo::rerun-if-changed={head}");
    }
    if let Some(reference) = git_output(workspace_root, &["symbolic-ref", "-q", "HEAD"]) {
        if let Some(reference_path) = git_output(workspace_root, &["rev-parse", "--git-path", &reference]) {
            println!("cargo::rerun-if-changed={reference_path}");
        }
    }

    let build_id = std::env::var("FLOTILLA_BUILD_ID")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            let revision = git_output(workspace_root, &["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
            let fingerprint = source_fingerprint(workspace_root).unwrap_or_else(|error| panic!("cannot fingerprint wire sources: {error}"));
            format!("{revision}+{fingerprint}")
        });
    println!("cargo::rustc-env=FLOTILLA_BUILD_ID={build_id}");
}
