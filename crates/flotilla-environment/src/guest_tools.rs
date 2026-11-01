//! Resolve a complete guest build from the installed, verified generation.
//! Controller-native binaries never participate in this lookup. What the build
//! must contain comes from this release's composition declaration
//! ([`crate::composition`]), never from the payload being checked.
//! [Workload and invalidation choices](../../../docs/implementation-choices/guest-tools.md).
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use crate::platform::Platform;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::composition::{self, Composition, FileFormat, Source};

const PREFIX: &str = "share/flotilla/guest-tools";
const TREE: &str = "share/flotilla/guest-tools/linux-amd64";

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

/// All members have been validated together before any connection or guest push.
#[derive(Clone)]
pub struct GuestTools {
    root: PathBuf,
    composition: &'static Composition,
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
        Self::resolve_with(generation, platform, composition::flotilla_tools())
    }
    /// Verify `generation`'s guest build against `composition`.
    pub fn resolve_with(generation: &Path, platform: &Platform, composition: &'static Composition) -> Result<Self, String> {
        let refuse = |message: String| format!("guest platform {platform}: {message}");
        if platform != &linux_amd64() {
            return Err(refuse("NoPlatformBuild: only linux/amd64 is admitted".into()));
        }
        let machine = composition::elf_machine(&platform.architecture).ok_or_else(|| refuse("NoPlatformBuild: no native format".into()))?;
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
            if path.components().any(|part| !matches!(part, std::path::Component::Normal(_))) || !declared.insert(file.path.clone()) {
                return Err(refuse(format!("unsafe or duplicate artifact {}", file.path)));
            }
            let member = owner(composition, &file.path).ok_or_else(|| refuse(format!("undeclared artifact {}", file.path)))?;
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
            match (&member.source, member.format) {
                (Source::File(_), Some(FileFormat::Elf)) => {
                    composition::validate_elf(&content, &file.path, machine, member.loads.as_ref()).map_err(refuse)?;
                    use std::os::unix::fs::PermissionsExt;
                    if metadata.permissions().mode() & 0o111 == 0 {
                        return Err(refuse(format!("native artifact is not executable: {}", file.path)));
                    }
                }
                (Source::File(_), Some(FileFormat::Terminfo)) => composition::validate_terminfo(&content, &file.path).map_err(refuse)?,
                (Source::Tree(tree), _) => {
                    let entry = file.path.strip_prefix(tree).map(|rest| rest.trim_start_matches('/'));
                    if member
                        .requires
                        .iter()
                        .any(|required| Some(required.path.as_str()) == entry && required.format == Some(FileFormat::Terminfo))
                    {
                        composition::validate_terminfo(&content, &file.path).map_err(refuse)?;
                    }
                }
                (Source::File(_), None) => {}
            }
        }
        for member in &composition.members {
            let required: Vec<String> = match &member.source {
                Source::File(file) => vec![file.clone()],
                Source::Tree(tree) => member.requires.iter().map(|entry| format!("{tree}/{}", entry.path)).collect(),
            };
            if let Some(missing) = required.into_iter().find(|name| !declared.contains(name)) {
                return Err(refuse(format!("missing {} artifact {missing}", member.role)));
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
        Ok(Self { root, composition })
    }
    /// The verified file a typed caller needs, by its declared role.
    pub fn file(&self, role: &str) -> PathBuf {
        match self.composition.member(role).map(|member| &member.source) {
            Some(Source::File(file)) => self.root.join(file),
            _ => unreachable!("the composition declares file role `{role}`"),
        }
    }
    /// Every verified file member, by role.
    pub fn files(&self) -> std::collections::BTreeMap<String, PathBuf> {
        self.composition
            .members
            .iter()
            .filter_map(|member| match &member.source {
                Source::File(file) => Some((member.role.clone(), self.root.join(file))),
                Source::Tree(_) => None,
            })
            .collect()
    }
    pub fn composition(&self) -> &'static Composition {
        self.composition
    }
    /// The verified tree, laid out by the members' generation paths.
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn agent(&self) -> PathBuf {
        self.file(composition::role::AGENT)
    }
}

/// The member whose file or tree contains `path`.
fn owner<'a>(composition: &'a Composition, path: &str) -> Option<&'a composition::Member> {
    composition.members.iter().find(|member| match &member.source {
        Source::File(file) => file == path,
        Source::Tree(tree) => Path::new(path).starts_with(tree) && path != tree,
    })
}

pub fn linux_amd64() -> Platform {
    Platform::new("linux", "amd64")
}

/// Verify bytes read for `role` against this release's declaration, for
/// callers that read a member themselves (the agent bootstrap).
pub fn validate_native(bytes: &[u8], role: &str, platform: &Platform) -> Result<(), String> {
    let member = composition::flotilla_tools().member(role).ok_or_else(|| format!("no guest member `{role}`"))?;
    let machine = composition::elf_machine(&platform.architecture).ok_or_else(|| format!("guest platform {platform}: no native format"))?;
    let name = match &member.source {
        Source::File(file) | Source::Tree(file) => file,
    };
    composition::validate_elf(bytes, name, machine, member.loads.as_ref())
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
        let fault = tc.draw(gs::integers::<usize>().min_value(0).max_value(13));
        let temporary = tempfile::tempdir().expect("generation");
        let root = temporary.path();
        let files = write_guest_fixture(root);
        let mut manifest: serde_json::Value = serde_json::from_slice(&files["manifest.json"]).expect("manifest");
        manifest["platform"] = ["linux-x86_64-gnu2.36", "darwin-aarch64"][controller].into();
        fs::write(root.join("manifest.json"), serde_json::to_vec(&manifest).expect("manifest")).expect("manifest");
        let native = root.join(TREE).join("bin/cleat");
        let rewrite_catalog = |path: &str, bytes: &[u8]| {
            let mut catalog: serde_json::Value =
                serde_json::from_slice(&fs::read(root.join(PREFIX).join("catalog.json")).expect("catalog")).expect("catalog");
            let record = catalog["platforms"]["linux/amd64"]["files"]
                .as_array_mut()
                .expect("records")
                .iter_mut()
                .find(|record| record["path"] == path)
                .expect("record");
            record["sha256"] = format!("{:x}", Sha256::digest(bytes)).into();
            record["size_bytes"] = bytes.len().into();
            fs::write(root.join(PREFIX).join("catalog.json"), serde_json::to_vec(&catalog).expect("catalog")).expect("catalog mutation");
        };
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
            8 | 9 | 12 | 13 => {
                let mut bytes = fs::read(&native).expect("native fixture");
                let field =
                    |bytes: &[u8], at: usize, size: usize| bytes[at..at + size].iter().rev().fold(0, |n, b| n << 8 | usize::from(*b));
                match fault {
                    8 => bytes[18] = 183,
                    9 => {
                        let start = bytes.windows(b"$ORIGIN/../lib".len()).position(|part| part == b"$ORIGIN/../lib").expect("rpath");
                        bytes[start] = b'/';
                    }
                    // Program headers past the end of the file.
                    12 => bytes.truncate(field(&bytes, 32, 8) + 8),
                    // A dynamic section that is not a whole number of entries.
                    _ => {
                        let (table, stride) = (field(&bytes, 32, 8), field(&bytes, 54, 2));
                        let dynamic =
                            (0..field(&bytes, 56, 2)).map(|i| table + stride * i).find(|&at| field(&bytes, at, 4) == 2).expect("dynamic");
                        bytes[dynamic + 32] ^= 8;
                    }
                }
                fs::write(&native, &bytes).expect("native mutation");
                rewrite_catalog("bin/cleat", &bytes);
            }
            10 => fs::write(root.join(TREE).join("share/terminfo/unindexed"), b"unverified").expect("extra artifact"),
            11 => {
                // An undeclared member indexed by the catalog cannot validate itself.
                fs::write(root.join(TREE).join("bin/extra"), b"extra").expect("extra member");
                let mut catalog: serde_json::Value =
                    serde_json::from_slice(&fs::read(root.join(PREFIX).join("catalog.json")).expect("catalog")).expect("catalog");
                catalog["platforms"]["linux/amd64"]["files"].as_array_mut().expect("records").push(serde_json::json!({
                    "path": "bin/extra", "sha256": format!("{:x}", Sha256::digest(b"extra")), "size_bytes": 5
                }));
                fs::write(root.join(PREFIX).join("catalog.json"), serde_json::to_vec(&catalog).expect("catalog")).expect("catalog");
            }
            _ => unreachable!(),
        }
        let controller = [linux_amd64(), Platform::new("darwin", "arm64")][controller].clone();
        let result = GuestTools::select(root, &controller, &linux_amd64());
        if fault == 0 {
            let tools = result.expect("matching guest build");
            assert_eq!(fs::read(tools.agent()).expect("agent"), files[&format!("{TREE}/bin/drydock-agent")]);
            assert_eq!(fs::read(tools.file(composition::role::CLI)).expect("CLI"), files[&format!("{TREE}/bin/flotilla-vessel")]);
        } else {
            assert!(result.err().expect("invalid guest build must refuse").contains("guest platform linux/amd64"));
        }
        assert!(GuestTools::resolve(root, &Platform::new("linux", "arm64")).err().expect("unsupported target").contains("NoPlatformBuild"));
    }

    // #3874 acceptance: a declared additional tool and companion tree validate
    // through the declaration alone; the shipped declaration refuses them, and
    // dropping a declared companion entry is refused by name.
    #[test]
    fn an_added_tool_and_companion_validate_through_the_declaration_alone() {
        let temporary = tempfile::tempdir().expect("generation");
        let root = temporary.path();
        write_guest_fixture(root);
        let mut catalog: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join(PREFIX).join("catalog.json")).expect("catalog")).expect("catalog");
        for (path, bytes) in [("bin/extra", &b"#!/bin/sh\n"[..]), ("share/extra/data/manifest", b"companion")] {
            fs::create_dir_all(root.join(TREE).join(path).parent().expect("parent")).expect("parent");
            fs::write(root.join(TREE).join(path), bytes).expect("extra file");
            catalog["platforms"]["linux/amd64"]["files"].as_array_mut().expect("records").push(serde_json::json!({
                "path": path, "sha256": format!("{:x}", Sha256::digest(bytes)), "size_bytes": bytes.len()
            }));
        }
        fs::write(root.join(PREFIX).join("catalog.json"), serde_json::to_vec(&catalog).expect("catalog")).expect("catalog");
        let mut extended = serde_json::to_value(composition::flotilla_tools()).expect("declaration");
        let members = extended["members"].as_array_mut().expect("members");
        members.push(serde_json::json!({"role": "extra", "file": "bin/extra", "install": "bin/extra"}));
        members.push(serde_json::json!({"role": "extra-data", "tree": "share/extra", "install": "share/extra", "requires": [{"path": "data/manifest"}]}));
        let extended: &'static Composition = Box::leak(Box::new(Composition::parse(&extended.to_string()).expect("extended declaration")));
        let tools = GuestTools::resolve_with(root, &linux_amd64(), extended).expect("extended build validates");
        assert_eq!(fs::read(tools.file("extra")).expect("extra"), b"#!/bin/sh\n");
        assert!(GuestTools::resolve(root, &linux_amd64()).err().expect("shipped declaration").contains("undeclared artifact bin/extra"));
        fs::remove_file(root.join(TREE).join("share/extra/data/manifest")).expect("drop companion");
        let mut catalog: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join(PREFIX).join("catalog.json")).expect("catalog")).expect("catalog");
        catalog["platforms"]["linux/amd64"]["files"]
            .as_array_mut()
            .expect("records")
            .retain(|record| record["path"] != "share/extra/data/manifest");
        fs::write(root.join(PREFIX).join("catalog.json"), serde_json::to_vec(&catalog).expect("catalog")).expect("catalog");
        let error = GuestTools::resolve_with(root, &linux_amd64(), extended).err().expect("missing companion");
        assert!(error.contains("missing extra-data artifact share/extra/data/manifest"), "{error}");
    }
}
