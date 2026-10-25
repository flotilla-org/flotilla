//! Resolve a complete guest build from the installed, verified generation.
//! Controller-native binaries never participate in this lookup.
//! [Workload and invalidation choices](../../../docs/implementation-choices/guest-tools.md).
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use drydock_protocol::Platform;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const PREFIX: &str = "share/flotilla/guest-tools";
const TREE: &str = "share/flotilla/guest-tools/linux-amd64";
const NATIVE: &[&str] = &["bin/drydock-agent", "bin/flotilla-vessel", "bin/cleat", "lib/libghostty-vt.so.0"];

#[derive(Deserialize)]
struct Catalog {
    schema_version: u32,
    sources: BTreeMap<String, String>,
    cohort: String,
    platforms: BTreeMap<String, Entry>,
}
#[derive(Deserialize)]
struct Entry {
    root: String,
    files: Vec<File>,
}
#[derive(Deserialize)]
struct File {
    path: String,
    sha256: String,
    size_bytes: u64,
}

/// All paths have been validated together before any connection or guest push.
#[derive(Clone)]
pub struct GuestTools {
    root: PathBuf,
}
impl GuestTools {
    pub fn installed(platform: &Platform) -> Result<Self, String> {
        if platform != &linux_amd64() {
            return Err(format!("guest platform {platform}: NoPlatformBuild: only linux/amd64 is admitted"));
        }
        // Installed generations are immutable; a fleet roll restarts the daemon.
        // Verify the complete payload once, retaining only its paths per process.
        static INSTALLED: std::sync::OnceLock<Result<GuestTools, String>> = std::sync::OnceLock::new();
        INSTALLED
            .get_or_init(|| {
                let executable = std::env::current_exe().map_err(|e| e.to_string())?;
                let generation = executable.parent().and_then(Path::parent).ok_or("controller executable has no generation root")?;
                Self::select(generation, &Platform::current(), platform)
            })
            .clone()
    }
    /// The controller is admitted separately; only the recorded guest selects bytes.
    pub fn select(generation: &Path, controller: &Platform, guest: &Platform) -> Result<Self, String> {
        if controller != &linux_amd64() && controller != &Platform::new("darwin", "arm64") {
            return Err(format!("guest platform {guest}: unsupported controller {controller}"));
        }
        Self::resolve(generation, guest)
    }
    pub fn resolve(generation: &Path, platform: &Platform) -> Result<Self, String> {
        let refuse = |message: String| format!("guest platform {platform}: {message}");
        if platform != &linux_amd64() {
            return Err(refuse("NoPlatformBuild: only linux/amd64 is admitted".into()));
        }
        let read = |path: &Path| fs::read(path).map_err(|e| refuse(format!("missing or unreadable artifact {}: {e}", path.display())));
        let catalog_path = generation.join(PREFIX).join("catalog.json");
        if !catalog_path.is_file() {
            return Err(refuse("NoPlatformBuild: installed generation has no compatible guest catalog".into()));
        }
        let catalog: Catalog = serde_json::from_slice(&read(&catalog_path)?).map_err(|e| refuse(format!("invalid catalog: {e}")))?;
        let manifest: serde_json::Value =
            serde_json::from_slice(&read(&generation.join("manifest.json"))?).map_err(|e| refuse(e.to_string()))?;
        let outer: serde_json::Value =
            serde_json::from_slice(&read(&generation.join(".generation.json"))?).map_err(|e| refuse(e.to_string()))?;
        let sources = serde_json::to_value(&catalog.sources).map_err(|e| refuse(e.to_string()))?;
        let cohort = outer["source_run"]["api_run_id"].as_u64().map(|id| id.to_string());
        if catalog.schema_version != 1
            || catalog.sources.len() != 4
            || manifest["sources"] != sources
            || outer["sources"] != sources
            || cohort.as_deref() != Some(catalog.cohort.as_str())
            || manifest["guest_tools_cohort"] != catalog.cohort
        {
            return Err(refuse("inconsistent generation cohort or source pins".into()));
        }
        if catalog.platforms.len() != 1 {
            return Err(refuse("unsupported catalog matrix".into()));
        }
        let entry = catalog.platforms.get("linux/amd64").ok_or_else(|| refuse("NoPlatformBuild: catalog lacks linux/amd64".into()))?;
        if entry.root != TREE {
            return Err(refuse("incompatible guest library layout".into()));
        }
        if entry.files.len() > 16384 {
            return Err(refuse("guest catalog exceeds artifact limit".into()));
        }
        let root = generation.join(&entry.root);
        let mut declared = BTreeSet::new();
        for file in &entry.files {
            let path = Path::new(&file.path);
            if path.components().any(|part| !matches!(part, std::path::Component::Normal(_)))
                || (!NATIVE.contains(&file.path.as_str()) && !file.path.starts_with("share/terminfo/"))
                || !declared.insert(file.path.clone())
            {
                return Err(refuse(format!("unsafe or duplicate artifact {}", file.path)));
            }
            let path = root.join(path);
            if !path.canonicalize().map_err(|e| refuse(e.to_string()))?.starts_with(root.canonicalize().map_err(|e| refuse(e.to_string()))?)
            {
                return Err(refuse(format!("artifact escapes guest tree: {}", file.path)));
            }
            let metadata = fs::metadata(&path).map_err(|e| refuse(format!("invalid artifact {}: {e}", file.path)))?;
            if !metadata.is_file() || metadata.len() > 128 * 1024 * 1024 {
                return Err(refuse(format!("invalid artifact type or size: {}", file.path)));
            }
            let content = read(&path)?;
            if content.len() as u64 != file.size_bytes || format!("{:x}", Sha256::digest(&content)) != file.sha256 {
                return Err(refuse(format!("digest/size mismatch: {}", file.path)));
            }
            if NATIVE.contains(&file.path.as_str()) {
                validate_native(&content, &file.path).map_err(refuse)?;
                use std::os::unix::fs::PermissionsExt;
                if fs::metadata(&path).map_err(|e| refuse(e.to_string()))?.permissions().mode() & 0o111 == 0 {
                    return Err(refuse(format!("native artifact is not executable: {}", file.path)));
                }
            }
            if file.path == "share/terminfo/x/xterm-ghostty" && !content.starts_with(&[0x1a, 1]) && !content.starts_with(&[0x1e, 2]) {
                return Err(refuse("invalid compiled terminfo".into()));
            }
        }
        for name in NATIVE.iter().copied().chain(["share/terminfo/x/xterm-ghostty", "share/terminfo/LICENSE"]) {
            if !declared.contains(name) {
                return Err(refuse(format!("missing artifact {name}")));
            }
        }
        fn inventory(root: &Path, relative: &Path, files: &mut BTreeSet<String>) -> Result<(), String> {
            if relative.components().count() > 16 || files.len() > 16384 {
                return Err("guest tree exceeds inventory limit".into());
            }
            for entry in fs::read_dir(root.join(relative)).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let path = relative.join(entry.file_name());
                let kind = entry.file_type().map_err(|e| e.to_string())?;
                if kind.is_dir() {
                    inventory(root, &path, files)?;
                } else if kind.is_file() || (kind.is_symlink() && entry.path().is_file()) {
                    files.insert(path.to_string_lossy().into_owned());
                } else {
                    return Err(format!("unsupported guest artifact {}", path.display()));
                }
            }
            Ok(())
        }
        let mut actual = BTreeSet::new();
        inventory(&root, Path::new(""), &mut actual).map_err(refuse)?;
        if actual != declared {
            return Err(refuse("unindexed or missing guest artifacts".into()));
        }
        Ok(Self { root })
    }
    pub fn agent(&self) -> PathBuf {
        self.root.join("bin/drydock-agent")
    }
    pub fn flotilla(&self) -> PathBuf {
        self.root.join("bin/flotilla-vessel")
    }
    pub fn cleat(&self) -> PathBuf {
        self.root.join("bin/cleat")
    }
    pub fn library(&self) -> PathBuf {
        self.root.join("lib/libghostty-vt.so.0")
    }
    pub fn terminfo(&self) -> PathBuf {
        self.root.join("share/terminfo")
    }
}
pub fn linux_amd64() -> Platform {
    Platform::new("linux", "amd64")
}

pub fn validate_native(bytes: &[u8], artifact: &str) -> Result<(), String> {
    if bytes.len() < 64 || !bytes.starts_with(b"\x7fELF\x02\x01") || bytes[18..20] != [62, 0] {
        return Err(format!("invalid ELF64 little-endian amd64 artifact {artifact}"));
    }
    if artifact != "bin/cleat" {
        return Ok(());
    }
    let invalid = || format!("incompatible cleat native loader layout: {artifact}");
    let number = |offset: usize, size: usize| -> Result<u64, String> {
        let part = bytes.get(offset..offset.checked_add(size).ok_or_else(invalid)?).ok_or_else(invalid)?;
        Ok(part.iter().enumerate().fold(0u64, |n, (i, b)| n | (u64::from(*b) << (8 * i))))
    };
    let table = usize::try_from(number(32, 8)?).map_err(|_| invalid())?;
    let stride = number(54, 2)? as usize;
    let count = number(56, 2)? as usize;
    if stride < 56 || table.checked_add(stride.checked_mul(count).ok_or_else(invalid)?).is_none_or(|end| end > bytes.len()) {
        return Err(invalid());
    }
    let mut loads = Vec::new();
    let mut dynamic = None;
    for index in 0..count {
        let start = table + stride * index;
        let segment = (number(start + 8, 8)?, number(start + 16, 8)?, number(start + 32, 8)?);
        match number(start, 4)? {
            1 => loads.push(segment),
            2 => dynamic = Some(segment),
            _ => {}
        }
    }
    let (offset, _, size) = dynamic.ok_or_else(invalid)?;
    let end = offset.checked_add(size).ok_or_else(invalid)?;
    if end > bytes.len() as u64 || size % 16 != 0 {
        return Err(invalid());
    }
    let mut tags = Vec::new();
    for start in (offset as usize..end as usize).step_by(16) {
        let tag = number(start, 8)?;
        if tag == 0 {
            break;
        }
        tags.push((tag, number(start + 8, 8)?));
    }
    let strings = tags.iter().find_map(|(tag, value)| (*tag == 5).then_some(*value)).ok_or_else(invalid)?;
    let (offset, address, _) =
        loads.into_iter().find(|(_, address, size)| strings >= *address && strings - address < *size).ok_or_else(invalid)?;
    let base = offset.checked_add(strings - address).ok_or_else(invalid)?;
    let mut library = false;
    let mut relative = false;
    for (tag, value) in tags {
        if !matches!(tag, 1 | 15 | 29) {
            continue;
        }
        let start = usize::try_from(base.checked_add(value).ok_or_else(invalid)?).map_err(|_| invalid())?;
        let tail = bytes.get(start..).ok_or_else(invalid)?;
        let string = &tail[..tail.iter().position(|b| *b == 0).ok_or_else(invalid)?];
        if tag == 1 && string == b"libghostty-vt.so.0" {
            library = true;
        }
        if matches!(tag, 15 | 29) && string.split(|b| *b == b':').any(|part| part == b"$ORIGIN/../lib") {
            relative = true;
        }
    }
    if !library || !relative {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../ci/fleet-candidates/fixtures/guest_tools.rs"));

    // Both admitted controller platforms resolve the same Linux guest cohort;
    // native controller bytes, missing members and mixed identities never satisfy it.
    // The finite generator covers both controllers and every named refusal boundary.
    #[hegel::test]
    fn controller_identity_cannot_replace_guest_build(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let controller = tc.draw(gs::integers::<usize>().min_value(0).max_value(1));
        let fault = tc.draw(gs::integers::<usize>().min_value(0).max_value(10));
        let temporary = tempfile::tempdir().expect("generation");
        let root = temporary.path();
        let files = write_guest_fixture(root);
        let mut manifest: serde_json::Value = serde_json::from_slice(&files["manifest.json"]).expect("manifest");
        manifest["platform"] = ["linux-x86_64-gnu2.36", "darwin-aarch64"][controller].into();
        fs::write(root.join("manifest.json"), serde_json::to_vec(&manifest).expect("manifest")).expect("manifest");
        let native = root.join(TREE).join("bin/cleat");
        match fault {
            0 => {}
            1 => fs::remove_file(root.join(TREE).join("bin/drydock-agent")).expect("remove member"),
            2 => fs::write(&native, b"controller native bytes").expect("tamper"),
            3 => {
                manifest["sources"]["cleat"] = "0".repeat(40).into();
                fs::write(root.join("manifest.json"), serde_json::to_vec(&manifest).expect("manifest")).expect("manifest");
            }
            4 => fs::remove_file(root.join(PREFIX).join("catalog.json")).expect("old generation"),
            5 => {
                let mut outer: serde_json::Value = serde_json::from_slice(&files[".generation.json"]).expect("outer");
                outer["source_run"]["api_run_id"] = 43.into();
                fs::write(root.join(".generation.json"), serde_json::to_vec(&outer).expect("outer")).expect("outer");
            }
            6 => fs::remove_file(root.join(TREE).join("lib/libghostty-vt.so.0")).expect("library"),
            7 => fs::remove_file(root.join(TREE).join("share/terminfo/x/xterm-ghostty-alias")).expect("alias"),
            8 | 9 => {
                let mut bytes = fs::read(&native).expect("native fixture");
                if fault == 8 {
                    bytes[18] = 183;
                } else {
                    let start = bytes.windows(b"$ORIGIN/../lib".len()).position(|part| part == b"$ORIGIN/../lib").expect("rpath");
                    bytes[start] = b'/';
                }
                fs::write(&native, &bytes).expect("native mutation");
                let mut catalog: serde_json::Value = serde_json::from_slice(&files[&format!("{PREFIX}/catalog.json")]).expect("catalog");
                let record = catalog["platforms"]["linux/amd64"]["files"]
                    .as_array_mut()
                    .expect("records")
                    .iter_mut()
                    .find(|record| record["path"] == "bin/cleat")
                    .expect("cleat record");
                record["sha256"] = format!("{:x}", Sha256::digest(&bytes)).into();
                fs::write(root.join(PREFIX).join("catalog.json"), serde_json::to_vec(&catalog).expect("catalog"))
                    .expect("catalog mutation");
            }
            10 => fs::write(root.join(TREE).join("share/terminfo/unindexed"), b"unverified").expect("extra artifact"),
            _ => unreachable!(),
        }
        let controller = [linux_amd64(), Platform::new("darwin", "arm64")][controller].clone();
        let result = GuestTools::select(root, &controller, &linux_amd64());
        if fault == 0 {
            let tools = result.expect("matching guest build");
            assert_eq!(fs::read(tools.agent()).expect("agent"), files[&format!("{TREE}/bin/drydock-agent")]);
            assert_eq!(fs::read(tools.flotilla()).expect("CLI"), files[&format!("{TREE}/bin/flotilla-vessel")]);
        } else {
            assert!(result.err().expect("invalid guest build must refuse").contains("guest platform linux/amd64"));
        }
        assert!(GuestTools::resolve(root, &Platform::new("linux", "arm64")).err().expect("unsupported target").contains("NoPlatformBuild"));
    }
}
