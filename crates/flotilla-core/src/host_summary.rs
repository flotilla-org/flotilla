use std::{collections::BTreeSet, fs, path::Path};

use flotilla_protocol::{HostEnvironment, HostName, HostProviderStatus, HostSummary, SystemInfo};
use sysinfo::System;

use crate::{
    convert::inventory_from_bag,
    model::provider_names_from_registry,
    providers::{
        discovery::{EnvVars, EnvironmentBag},
        registry::ProviderRegistry,
    },
};

pub fn build_local_host_summary(
    host_name: &HostName,
    host_bag: &EnvironmentBag,
    providers: Vec<HostProviderStatus>,
    env: &dyn EnvVars,
) -> HostSummary {
    HostSummary {
        host_name: host_name.clone(),
        system: collect_system_info(env),
        inventory: inventory_from_bag(host_bag),
        providers,
        environments: vec![],
    }
}

pub fn provider_statuses_from_registries<'a>(registries: impl IntoIterator<Item = &'a ProviderRegistry>) -> Vec<HostProviderStatus> {
    let mut seen = BTreeSet::new();
    let mut statuses = Vec::new();

    for registry in registries {
        for (category, names) in provider_names_from_registry(registry) {
            for name in names {
                if seen.insert((category.clone(), name.clone())) {
                    // Static batch: `healthy` means the provider is registered locally at startup.
                    statuses.push(HostProviderStatus { category: category.clone(), name, healthy: true });
                }
            }
        }
    }

    statuses.sort_by(|a, b| a.category.cmp(&b.category).then_with(|| a.name.cmp(&b.name)));
    statuses
}

pub fn collect_system_info(env: &dyn EnvVars) -> SystemInfo {
    SystemInfo {
        home_dir: env.get("HOME").map(Into::into),
        os: Some(std::env::consts::OS.to_string()),
        arch: Some(std::env::consts::ARCH.to_string()),
        cpu_count: std::thread::available_parallelism().ok().and_then(|n| u16::try_from(n.get()).ok()),
        memory_total_mb: total_memory_mb(),
        environment: detect_host_environment(),
    }
}

pub fn classify_host_environment_from_markers(dockerenv_present: bool, cgroup: Option<&str>) -> HostEnvironment {
    if dockerenv_present {
        return HostEnvironment::Container;
    }

    let Some(cgroup) = cgroup else {
        return HostEnvironment::Unknown;
    };

    if ["docker", "containerd", "kubepods", "podman", "libpod"].iter().any(|marker| cgroup.contains(marker)) {
        HostEnvironment::Container
    } else {
        HostEnvironment::Unknown
    }
}

fn detect_host_environment() -> HostEnvironment {
    let dockerenv_present = Path::new("/.dockerenv").exists();
    let cgroup = fs::read_to_string("/proc/1/cgroup").ok();
    classify_host_environment_from_markers(dockerenv_present, cgroup.as_deref())
}

fn total_memory_mb() -> Option<u64> {
    let mut system = System::new();
    system.refresh_memory();
    let total_bytes = system.total_memory();
    (total_bytes > 0).then_some(total_bytes / (1024 * 1024))
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::HostEnvironment;

    use super::*;

    #[test]
    fn classify_host_environment_unknown_without_markers() {
        let env = classify_host_environment_from_markers(false, None);
        assert_eq!(env, HostEnvironment::Unknown);
    }
}
