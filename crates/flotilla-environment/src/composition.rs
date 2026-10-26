//! Guest layer composition (#3874): the one declaration of what a guest layer
//! contains, how each member is verified, where it is published and what
//! environment the layer contributes. Runtime lookup, publication, delivery
//! and the Cleat roll read it generically; the release validator reads the
//! same file. See `stdlib/guest-layers/README.md`.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::LazyLock,
};

use serde::{Deserialize, Serialize};

/// Roles whose callers need that specific file. Every other member is handled
/// only through the generic member list.
pub mod role {
    /// Bootstrapped separately into each vessel; never published in the layer.
    pub const AGENT: &str = "agent";
    pub const CLI: &str = "cli";
    pub const CLEAT: &str = "cleat";
    pub const CLEAT_LIBRARY: &str = "cleat-library";
}

/// The generation-built tools layer this release requires.
pub fn flotilla_tools() -> &'static Composition {
    static TOOLS: LazyLock<Composition> = LazyLock::new(|| {
        Composition::parse(include_str!("../../../stdlib/guest-layers/flotilla-tools.json")).expect("stdlib flotilla-tools declaration")
    });
    &TOOLS
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Composition {
    pub schema_version: u32,
    pub name: String,
    pub members: Vec<Member>,
    #[serde(default)]
    pub environment: LayerEnvironment,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub role: String,
    /// A generation-relative regular file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// A generation-relative directory; every file under it belongs to this member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    /// Where the member is published in the layer. Absent: verified, not published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<FileFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loads: Option<Loads>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<RequiredEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileFormat {
    /// A native executable or library for the guest platform.
    Elf,
    /// A compiled terminfo entry.
    Terminfo,
}

/// Libraries a native member loads, found through a loader search path that is
/// relative to the member's own install directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Loads {
    pub libraries: Vec<String>,
    pub runpath: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredEntry {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<FileFormat>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerEnvironment {
    /// Install-relative directories prepended to `PATH`, in order.
    #[serde(default)]
    pub path: Vec<String>,
    /// Values joined with `:`; relative entries are install paths.
    #[serde(default)]
    pub variables: BTreeMap<String, Vec<String>>,
}

/// One member's source location in the generation tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source<'a> {
    File(&'a str),
    Tree(&'a str),
}

impl Member {
    pub fn source(&self) -> Source<'_> {
        match (&self.file, &self.tree) {
            (Some(file), _) => Source::File(file),
            (None, Some(tree)) => Source::Tree(tree),
            (None, None) => unreachable!("validated members name a file or a tree"),
        }
    }
}

impl Composition {
    pub fn parse(text: &str) -> Result<Self, String> {
        let composition: Self = serde_json::from_str(text).map_err(|error| format!("invalid guest layer declaration: {error}"))?;
        composition.validate()?;
        Ok(composition)
    }

    /// Static rules: unique roles and paths, normalised relative paths, one
    /// source per member, and every loader dependency resolving to a published
    /// member in the published layout.
    pub fn validate(&self) -> Result<(), String> {
        let refuse = |message: String| Err(format!("guest layer `{}`: {message}", self.name));
        if self.schema_version != 1 {
            return refuse(format!("unsupported declaration schema {}", self.schema_version));
        }
        if self.name.is_empty() || self.members.is_empty() {
            return refuse("a layer has a name and at least one member".into());
        }
        let mut roles = BTreeSet::new();
        let mut sources: Vec<(&str, bool)> = Vec::new();
        let mut installs: Vec<&str> = Vec::new();
        for member in &self.members {
            let role = &member.role;
            if role.is_empty() || !roles.insert(role.as_str()) {
                return refuse(format!("empty or duplicate role `{role}`"));
            }
            let (source, tree) = match (&member.file, &member.tree) {
                (Some(file), None) => (file.as_str(), false),
                (None, Some(tree)) => (tree.as_str(), true),
                _ => return refuse(format!("member `{role}` names exactly one of `file` or `tree`")),
            };
            for path in std::iter::once(source).chain(member.install.as_deref()) {
                if !normalised(path) {
                    return refuse(format!("member `{role}` path `{path}` must be relative and normalised"));
                }
            }
            if sources.iter().any(|(other, other_tree)| overlaps(source, other, tree || *other_tree)) {
                return refuse(format!("member `{role}` source `{source}` overlaps another member"));
            }
            sources.push((source, tree));
            if let Some(install) = &member.install {
                if install == "tools" || install.starts_with("tools/") {
                    return refuse(format!("member `{role}` installs under the reserved `tools` directory"));
                }
                if installs.iter().any(|other| overlaps(install, other, true)) {
                    return refuse(format!("member `{role}` install `{install}` overlaps another member"));
                }
                installs.push(install);
            }
            if tree && (member.format.is_some() || member.loads.is_some()) {
                return refuse(format!("tree member `{role}` declares formats per required entry"));
            }
            if !tree && !member.requires.is_empty() {
                return refuse(format!("file member `{role}` cannot require entries"));
            }
            for entry in &member.requires {
                if !normalised(&entry.path) || entry.format == Some(FileFormat::Elf) {
                    return refuse(format!("member `{role}` required entry `{}` must be a normalised non-native path", entry.path));
                }
            }
            if let Some(loads) = &member.loads {
                if member.format != Some(FileFormat::Elf) {
                    return refuse(format!("member `{role}` loads libraries but is not native"));
                }
                let install = member
                    .install
                    .as_deref()
                    .ok_or_else(|| format!("guest layer `{}`: member `{role}` loads libraries but is not published", self.name))?;
                if loads.libraries.is_empty() {
                    return refuse(format!("member `{role}` declares an empty library list"));
                }
                for library in &loads.libraries {
                    let resolved = resolve_library(install, &loads.runpath, library).ok_or_else(|| {
                        format!("guest layer `{}`: member `{role}` runpath `{}` is not origin-relative", self.name, loads.runpath)
                    })?;
                    let provided = self.members.iter().any(|other| {
                        other.file.is_some() && other.format == Some(FileFormat::Elf) && other.install.as_deref() == Some(resolved.as_str())
                    });
                    if !provided {
                        return refuse(format!("member `{role}` loads `{library}`, but no published native member installs `{resolved}`"));
                    }
                }
            }
        }
        for entry in self.environment.path.iter().chain(self.environment.variables.values().flatten()) {
            if !entry.starts_with('/') && !normalised(entry) {
                return refuse(format!("environment entry `{entry}` must be absolute or a normalised install path"));
            }
        }
        for name in self.environment.variables.keys() {
            if name == "PATH" || name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
                return refuse(format!("environment variable `{name}` is not a settable name"));
            }
        }
        Ok(())
    }

    pub fn member(&self, role: &str) -> Option<&Member> {
        self.members.iter().find(|member| member.role == role)
    }

    /// Published members with their install paths, in declaration order.
    pub fn published(&self) -> impl Iterator<Item = (&Member, &str)> {
        self.members.iter().filter_map(|member| member.install.as_deref().map(|install| (member, install)))
    }

    /// The real directories that hold a published layer's stable entry points:
    /// each published file's parent and each published tree.
    pub fn exposed_directories(&self) -> BTreeSet<PathBuf> {
        self.published()
            .map(|(member, install)| match member.source() {
                Source::File(_) => Path::new(install).parent().map(Path::to_path_buf).unwrap_or_default(),
                Source::Tree(_) => PathBuf::from(install),
            })
            .collect()
    }

    /// `PATH` directories under `prefix`, in order.
    pub fn path_entries(&self, prefix: &Path) -> Vec<PathBuf> {
        self.environment.path.iter().map(|entry| prefix.join(entry)).collect()
    }

    /// The layer's variables with install paths under `prefix`.
    pub fn variables(&self, prefix: &Path) -> Vec<(String, String)> {
        self.environment
            .variables
            .iter()
            .map(|(name, entries)| {
                let value = entries
                    .iter()
                    .map(|entry| if entry.starts_with('/') { entry.clone() } else { prefix.join(entry).to_string_lossy().into_owned() })
                    .collect::<Vec<_>>()
                    .join(":");
                (name.clone(), value)
            })
            .collect()
    }
}

fn normalised(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && Path::new(path).components().all(|part| matches!(part, Component::Normal(_)))
        && !path.ends_with('/')
}

/// Whether two relative paths are equal or one contains the other, when either
/// can contain files (`containing`); otherwise only equality collides.
fn overlaps(left: &str, right: &str, containing: bool) -> bool {
    left == right || (containing && (Path::new(left).starts_with(right) || Path::new(right).starts_with(left)))
}

/// The install path a library resolves to from `install` through an
/// origin-relative runpath entry.
pub(crate) fn resolve_library(install: &str, runpath: &str, library: &str) -> Option<String> {
    let relative = runpath.strip_prefix("$ORIGIN")?;
    let mut resolved: Vec<&str> = Path::new(install).parent()?.iter().filter_map(|part| part.to_str()).collect();
    for part in relative.split('/').filter(|part| !part.is_empty() && *part != ".") {
        if part == ".." {
            resolved.pop()?;
        } else {
            resolved.push(part);
        }
    }
    resolved.push(library);
    Some(resolved.join("/"))
}

/// The ELF machine number a guest platform's natives carry.
pub fn elf_machine(architecture: &str) -> Option<u16> {
    match architecture {
        "amd64" => Some(62),
        "arm64" => Some(183),
        _ => None,
    }
}

/// Verify a native member: an ELF64 little-endian object for `machine` and,
/// when it loads libraries, those libraries and the origin-relative runpath in
/// its dynamic section. Guest bytes are inspected, never executed.
pub fn validate_elf(bytes: &[u8], artifact: &str, machine: u16, loads: Option<&Loads>) -> Result<(), String> {
    if bytes.len() < 64 || !bytes.starts_with(b"\x7fELF\x02\x01") || bytes[18..20] != machine.to_le_bytes() {
        return Err(format!("invalid ELF64 little-endian machine {machine} artifact {artifact}"));
    }
    let Some(loads) = loads else { return Ok(()) };
    let invalid = || format!("incompatible native loader layout: {artifact}");
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
    let mut segments = Vec::new();
    let mut dynamic = None;
    for index in 0..count {
        let start = table + stride * index;
        let segment = (number(start + 8, 8)?, number(start + 16, 8)?, number(start + 32, 8)?);
        match number(start, 4)? {
            1 => segments.push(segment),
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
        segments.into_iter().find(|(_, address, size)| strings >= *address && strings - address < *size).ok_or_else(invalid)?;
    let base = offset.checked_add(strings - address).ok_or_else(invalid)?;
    let mut needed = BTreeSet::new();
    let mut relative = false;
    for (tag, value) in tags {
        if !matches!(tag, 1 | 15 | 29) {
            continue;
        }
        let start = usize::try_from(base.checked_add(value).ok_or_else(invalid)?).map_err(|_| invalid())?;
        let tail = bytes.get(start..).ok_or_else(invalid)?;
        let string = &tail[..tail.iter().position(|b| *b == 0).ok_or_else(invalid)?];
        if tag == 1 {
            needed.insert(string.to_vec());
        } else if string.split(|b| *b == b':').any(|part| part == loads.runpath.as_bytes()) {
            relative = true;
        }
    }
    if !relative || !loads.libraries.iter().all(|library| needed.contains(library.as_bytes())) {
        return Err(invalid());
    }
    Ok(())
}

/// A compiled terminfo entry: legacy or extended-number magic.
pub fn validate_terminfo(bytes: &[u8], artifact: &str) -> Result<(), String> {
    if bytes.starts_with(&[0x1a, 1]) || bytes.starts_with(&[0x1e, 2]) {
        Ok(())
    } else {
        Err(format!("invalid compiled terminfo {artifact}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The shipped declaration carries every role a caller looks up, with the
    // published layout and environment the vessel paths are built from.
    #[test]
    fn stdlib_tools_declaration_names_every_typed_role() {
        let tools = flotilla_tools();
        for typed in [role::AGENT, role::CLI, role::CLEAT, role::CLEAT_LIBRARY] {
            assert!(matches!(tools.member(typed).map(Member::source), Some(Source::File(_))), "{typed}");
        }
        assert!(tools.member(role::AGENT).is_some_and(|agent| agent.install.is_none()), "the agent is bootstrapped, not published");
        assert_eq!(
            tools.exposed_directories().into_iter().collect::<Vec<_>>(),
            [PathBuf::from("bin"), PathBuf::from("lib"), PathBuf::from("share/terminfo")]
        );
        assert_eq!(tools.path_entries(Path::new("/p")), [PathBuf::from("/p/bin")]);
        assert_eq!(tools.variables(Path::new("/p")), [("TERMINFO_DIRS".to_string(), "/p/share/terminfo:/usr/share/terminfo".to_string())]);
    }

    // Static declaration rules refuse each broken composition with its reason.
    #[test]
    fn invalid_declarations_are_refused_with_their_reason() {
        let base = serde_json::to_value(flotilla_tools()).expect("declaration");
        type Mutation = fn(&mut serde_json::Value);
        let cases: [(&str, Mutation); 7] = [
            ("duplicate role", |value| value["members"][1]["role"] = "agent".into()),
            ("exactly one of", |value| value["members"][0]["tree"] = "share".into()),
            ("normalised", |value| value["members"][1]["install"] = "../bin/flotilla".into()),
            ("overlaps", |value| value["members"][4]["install"] = "bin".into()),
            ("no published native member installs `lib/libghostty-vt.so.0`", |value| {
                value["members"][3]["install"] = "lib/other.so".into()
            }),
            ("not origin-relative", |value| value["members"][2]["loads"]["runpath"] = "/usr/lib".into()),
            ("reserved `tools`", |value| value["members"][1]["install"] = "tools/flotilla".into()),
        ];
        for (reason, mutate) in cases {
            let mut value = base.clone();
            mutate(&mut value);
            let error = Composition::parse(&value.to_string()).expect_err(reason);
            assert!(error.contains(reason), "{reason}: {error}");
        }
    }

    #[test]
    fn origin_relative_runpaths_resolve_against_the_install_directory() {
        assert_eq!(resolve_library("bin/cleat", "$ORIGIN/../lib", "x.so").as_deref(), Some("lib/x.so"));
        assert_eq!(resolve_library("a/b/tool", "$ORIGIN", "x.so").as_deref(), Some("a/b/x.so"));
        assert_eq!(resolve_library("tool", "$ORIGIN/../lib", "x.so"), None);
        assert_eq!(resolve_library("bin/cleat", "/lib", "x.so"), None);
    }
}
