use std::{fs, path::Path};

use flotilla_protocol::{EnvironmentId, HostEnvironment, HostName, HostProviderStatus, HostSummary, NodeId, NodeInfo, SystemInfo};
use sysinfo::System;

use crate::ports::Environments;
use flotilla_provider_runtime::EnvVars;

/// Resident bytes of this daemon process, sampled for the host heartbeat.
/// Failure is unknown, never zero; refresh only this PID rather than the fleet.
pub fn daemon_rss_bytes() -> Option<u64> {
    let pid = sysinfo::get_current_pid().ok()?;
    let mut system = System::new();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        sysinfo::ProcessRefreshKind::nothing().with_memory(),
    );
    system.process(pid).map(|process| process.memory()).filter(|bytes| *bytes > 0)
}

pub async fn build_local_host_summary(
    node_id: &NodeId,
    host_name: &HostName,
    environment_id: EnvironmentId,
    environment_manager: &dyn Environments,
    providers: Vec<HostProviderStatus>,
    env: &dyn EnvVars,
) -> HostSummary {
    let inventory = environment_manager.local_inventory();
    let environments = environment_manager.host_summary_environments().await;

    HostSummary {
        environment_id,
        host_name: Some(host_name.clone()),
        node: NodeInfo::new(node_id.clone(), host_name.to_string()),
        system: collect_system_info(env),
        inventory,
        providers,
        environments,
    }
}

pub fn collect_system_info(env: &dyn EnvVars) -> SystemInfo {
    SystemInfo {
        home_dir: env.get("HOME").map(Into::into),
        os: Some(env.host_os().to_string()),
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
