//! `[docker_vessels]` in `daemon.toml`: Docker vessels bounded together under one systemd slice.

use std::num::NonZeroU64;
#[cfg(any(unix, test))]
use std::path::Path;

#[cfg(any(unix, test))]
use flotilla_provider_runtime::{ChannelLabel, CommandRunner};
use serde::Deserialize;

/// Every Docker vessel starts under `slice` and may use up to the ceiling with
/// no swap; the slice's own `MemoryMax` bounds their total.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawDockerVessels")]
pub struct DockerVessels {
    slice: String,
    memory_ceiling_gib: NonZeroU64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDockerVessels {
    slice: String,
    memory_ceiling_gib: u64,
}

impl TryFrom<RawDockerVessels> for DockerVessels {
    type Error = String;
    fn try_from(raw: RawDockerVessels) -> Result<Self, String> {
        if slice_cgroup_path(&raw.slice).is_none() {
            return Err(format!("slice {:?} is not a plain systemd slice name ending in .slice", raw.slice));
        }
        let memory_ceiling_gib = NonZeroU64::new(raw.memory_ceiling_gib)
            .filter(|gib| gib.get() <= u64::MAX >> 30)
            .ok_or("memory_ceiling_gib must be a positive number of GiB")?;
        Ok(Self { slice: raw.slice, memory_ceiling_gib })
    }
}

impl DockerVessels {
    pub fn parse(table: toml::Table) -> Result<Self, String> {
        toml::Value::Table(table).try_into().map_err(|error| format!("[docker_vessels] in daemon.toml: {error}"))
    }

    /// The systemd slice every Docker vessel starts under.
    pub fn slice(&self) -> &str {
        &self.slice
    }

    /// One vessel's memory and memory-plus-swap limit.
    pub fn memory_ceiling_bytes(&self) -> u64 {
        self.memory_ceiling_gib.get() << 30
    }

    /// Docker's systemd driver creates a missing slice silently and unbounded,
    /// so the slice must already be loaded with a finite `MemoryMax` that
    /// leaves room for at least one vessel at the ceiling.
    #[cfg(any(unix, test))]
    pub(super) async fn verify(&self, runner: &dyn CommandRunner, cgroup_driver: &str) -> Result<(), String> {
        let slice = &self.slice;
        if cgroup_driver != "systemd" {
            return Err(format!("Docker vessel slice {slice} requires Docker's systemd cgroup driver, not {cgroup_driver:?}"));
        }
        let shown = runner
            .run("systemctl", &["show", slice, "--property=LoadState", "--property=MemoryMax"], Path::new("/"), &ChannelLabel::Default)
            .await
            .map_err(|error| format!("Docker vessel slice {slice} could not be checked: {error}"))?;
        let property = |key: &str| shown.lines().find_map(|line| line.trim().strip_prefix(key)?.strip_prefix('='));
        if property("LoadState") != Some("loaded") {
            return Err(format!("Docker vessel slice {slice} is not installed on this host"));
        }
        // Some systemd versions print an unset limit as u64::MAX rather than `infinity`.
        let Some(max) = property("MemoryMax").and_then(|max| max.parse::<u64>().ok()).filter(|max| *max != u64::MAX) else {
            return Err(format!("Docker vessel slice {slice} has no MemoryMax"));
        };
        if self.memory_ceiling_bytes() > max {
            return Err(format!(
                "Docker vessel memory ceiling {} bytes exceeds slice {slice}'s MemoryMax of {max} bytes",
                self.memory_ceiling_bytes()
            ));
        }
        Ok(())
    }
}

/// The cgroup v2 directory of a systemd slice, whose dashes nest it in its
/// parents; `None` for anything that is not a plain slice name. Escapes such
/// as `\x2d` are valid unit names but are refused here.
pub(super) fn slice_cgroup_path(slice: &str) -> Option<String> {
    let stem = slice.strip_suffix(".slice")?;
    if slice.len() > 255
        || stem.is_empty()
        || stem.starts_with('-')
        || stem.ends_with('-')
        || stem.contains("--")
        || !stem.bytes().all(|byte| byte.is_ascii_alphanumeric() || b":_.-".contains(&byte))
    {
        return None;
    }
    let mut path: Vec<_> = stem.match_indices('-').map(|(index, _)| format!("{}.slice", &stem[..index])).collect();
    path.push(slice.to_string());
    Some(path.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::generators as gs;

    // A slice name built from one to four non-empty segments of [A-Za-z0-9:_.]
    // joined by `-` is accepted and nests under each dash-prefix slice, while
    // each corruption (leading, trailing or doubled dash, a `/` or a backslash, a
    // missing suffix) is refused. Each case also pads the name to exactly 255
    // bytes, which is accepted, and 256 bytes, which is refused.
    #[hegel::test]
    fn slice_names_follow_the_systemd_slice_grammar(tc: hegel::TestCase) {
        let alphabet: Vec<char> = ('a'..='z').chain('A'..='Z').chain('0'..='9').chain([':', '_', '.']).collect();
        let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
        let segments: Vec<String> = (0..count)
            .map(|_| {
                let length = tc.draw(gs::integers::<usize>().min_value(1).max_value(40));
                (0..length).map(|_| alphabet[tc.draw(gs::integers::<usize>().min_value(0).max_value(alphabet.len() - 1))]).collect()
            })
            .collect();
        let stem = segments.join("-");
        let expected: Vec<String> = (1..=count).map(|n| format!("{}.slice", segments[..n].join("-"))).collect();
        assert_eq!(slice_cgroup_path(&format!("{stem}.slice")), Some(expected.join("/")));
        let head = &segments[0];
        for corrupted in [
            format!("-{stem}.slice"),
            format!("{stem}-.slice"),
            format!("{head}--{head}.slice"),
            format!("{stem}/{head}.slice"),
            format!("{stem}\\x2d{head}.slice"),
            stem.clone(),
            ".slice".into(),
        ] {
            assert_eq!(slice_cgroup_path(&corrupted), None, "{corrupted}");
        }
        for (length, accepted) in [(255, true), (256, false)] {
            let padded = format!("{stem}-{}.slice", "x".repeat(length - stem.len() - ".slice".len() - 1));
            assert_eq!(padded.len(), length);
            assert_eq!(slice_cgroup_path(&padded).is_some(), accepted, "{length}-byte slice name");
        }
    }

    // The table needs a valid slice and a positive GiB ceiling, and refuses
    // missing or unknown keys. Every shape is tried at ceilings of 0, 1, the
    // largest byte-representable value and one past it, and at a drawn ceiling.
    #[hegel::test]
    fn table_needs_a_slice_and_positive_ceiling(tc: hegel::TestCase) {
        let drawn = tc.draw(gs::integers::<u64>().min_value(1).max_value(u64::MAX >> 30));
        let ceilings = [(0, false), (1, true), (drawn, true), (u64::MAX >> 30, true), ((u64::MAX >> 30) + 1, false)];
        for (gib, gib_valid) in ceilings {
            for (slice, slice_valid) in [("flotilla-crews.slice", true), ("flotilla-crews", false)] {
                for shape in 0..4 {
                    let body = match shape {
                        0 => format!("slice = {slice:?}\nmemory_ceiling_gib = {gib}\n"),
                        1 => format!("slice = {slice:?}\n"),
                        2 => format!("memory_ceiling_gib = {gib}\n"),
                        _ => format!("slice = {slice:?}\nmemory_ceiling_gib = {gib}\nmax_vessels = 4\n"),
                    };
                    let parsed = DockerVessels::parse(toml::from_str(&body).expect("well-formed TOML"));
                    if shape == 0 && slice_valid && gib_valid {
                        let vessels = parsed.expect("valid [docker_vessels]");
                        assert_eq!((vessels.slice(), vessels.memory_ceiling_bytes()), (slice, gib << 30));
                    } else {
                        assert!(parsed.is_err(), "{body} should be refused");
                    }
                }
            }
        }
    }
}
