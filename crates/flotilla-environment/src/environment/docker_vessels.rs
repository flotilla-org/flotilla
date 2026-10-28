//! `[docker_vessels]` in `daemon.toml`: Docker vessels bounded together under one systemd slice.

use std::{num::NonZeroU64, path::Path};

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
            return Err(format!("slice {:?} is not a systemd slice name ending in .slice", raw.slice));
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

    pub fn slice(&self) -> &str {
        &self.slice
    }

    pub fn memory_ceiling_bytes(&self) -> u64 {
        self.memory_ceiling_gib.get() << 30
    }

    /// Docker's systemd driver creates a missing slice silently and unbounded,
    /// so the slice must already be loaded with a finite `MemoryMax`.
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
        if !property("MemoryMax").is_some_and(|max| max.parse::<u64>().is_ok()) {
            return Err(format!("Docker vessel slice {slice} has no MemoryMax"));
        }
        Ok(())
    }
}

/// The cgroup v2 directory of a systemd slice, whose dashes nest it in its
/// parents; `None` for anything that is not a plain slice name.
pub(super) fn slice_cgroup_path(slice: &str) -> Option<String> {
    let stem = slice.strip_suffix(".slice")?;
    if stem.is_empty()
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

    // The table needs a systemd slice name and a positive GiB ceiling, and
    // refuses missing or unknown keys. The generator covers valid and malformed
    // slice names, ceilings at 0, 1 and the largest byte-representable value and
    // beyond, and dropped or extra keys.
    #[hegel::test]
    fn accepts_only_a_slice_and_positive_ceiling(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let slices = [
            ("flotilla-crews.slice", true),
            ("crews.slice", true),
            ("a:b_c.d.slice", true),
            ("flotilla-crews", false),
            (".slice", false),
            ("-crews.slice", false),
            ("crews-.slice", false),
            ("flotilla--crews.slice", false),
            ("flotilla/crews.slice", false),
            ("crews.service", false),
        ];
        let (slice, slice_valid) = slices[tc.draw(gs::integers::<usize>().min_value(0).max_value(slices.len() - 1))];
        let ceilings = [(0, false), (1, true), (20, true), (u64::MAX >> 30, true), ((u64::MAX >> 30) + 1, false)];
        let (gib, gib_valid) = ceilings[tc.draw(gs::integers::<usize>().min_value(0).max_value(ceilings.len() - 1))];
        let shape = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        let body = match shape {
            0 => format!("slice = {slice:?}\nmemory_ceiling_gib = {gib}\n"),
            1 => format!("slice = {slice:?}\n"),
            2 => format!("memory_ceiling_gib = {gib}\n"),
            _ => format!("slice = {slice:?}\nmemory_ceiling_gib = {gib}\nmax_vessels = 4\n"),
        };
        let table: toml::Table = toml::from_str(&body).expect("well-formed TOML");
        let parsed = DockerVessels::parse(table);
        if shape == 0 && slice_valid && gib_valid {
            let vessels = parsed.expect("valid [docker_vessels]");
            assert_eq!((vessels.slice(), vessels.memory_ceiling_bytes()), (slice, gib << 30));
        } else {
            assert!(parsed.is_err(), "{body} should be refused");
        }
    }
}
