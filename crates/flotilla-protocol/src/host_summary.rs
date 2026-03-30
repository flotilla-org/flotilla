use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{EnvironmentInfo, HostName};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSummary {
    pub host_name: HostName,
    pub system: SystemInfo,
    #[serde(default)]
    pub inventory: ToolInventory,
    #[serde(default)]
    pub providers: Vec<HostProviderStatus>,
    #[serde(default)]
    pub environments: Vec<EnvironmentInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SystemInfo {
    #[serde(default)]
    pub home_dir: Option<PathBuf>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub cpu_count: Option<u16>,
    #[serde(default)]
    pub memory_total_mb: Option<u64>,
    #[serde(default)]
    pub environment: HostEnvironment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HostEnvironment {
    // Reserved for future host classification once we can distinguish these locally.
    BareMetal,
    Vm,
    Container,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolInventory {
    #[serde(default)]
    pub binaries: Vec<DiscoveryFact>,
    #[serde(default)]
    pub sockets: Vec<DiscoveryFact>,
    #[serde(default)]
    pub auth: Vec<DiscoveryFact>,
    #[serde(default)]
    pub env_vars: Vec<DiscoveryFact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryFact {
    pub name: String,
    #[serde(default)]
    pub detail: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostProviderStatus {
    pub category: String,
    /// Display name for the provider (e.g. "Docker").
    pub name: String,
    /// Implementation key used for provider lookup (e.g. "docker").
    #[serde(default)]
    pub implementation: String,
    pub healthy: bool,
}

/// Full snapshot of one host's state — system info, inventory, provider health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSnapshot {
    pub seq: u64,
    pub host_name: HostName,
    pub is_local: bool,
    pub connection_status: crate::PeerConnectionState,
    pub summary: HostSummary,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_helpers::assert_roundtrip, EnvironmentId, EnvironmentInfo, EnvironmentStatus, HostName, ImageId};

    #[test]
    fn host_summary_roundtrips_with_direct_and_provisioned_environments() {
        let summary = HostSummary {
            host_name: HostName::new("desktop"),
            system: SystemInfo {
                home_dir: Some(PathBuf::from("/home/dev")),
                os: Some("linux".into()),
                arch: Some("aarch64".into()),
                cpu_count: Some(8),
                memory_total_mb: None,
                environment: HostEnvironment::Container,
            },
            inventory: ToolInventory::default(),
            providers: vec![HostProviderStatus { category: "vcs".into(), name: "Git".into(), implementation: "git".into(), healthy: true }],
            environments: vec![
                EnvironmentInfo::Direct {
                    id: EnvironmentId::new("env-direct"),
                    display_name: Some("ssh-dev".into()),
                    status: EnvironmentStatus::Running,
                },
                EnvironmentInfo::Provisioned {
                    id: EnvironmentId::new("env-provisioned"),
                    display_name: None,
                    image: ImageId::new("ubuntu:24.04"),
                    status: EnvironmentStatus::Stopped,
                },
            ],
        };

        assert_roundtrip(&summary);
    }

    #[test]
    fn host_snapshot_roundtrips() {
        let snapshot = HostSnapshot {
            seq: 1,
            host_name: HostName::new("desktop"),
            is_local: true,
            connection_status: crate::PeerConnectionState::Connected,
            summary: HostSummary {
                host_name: HostName::new("desktop"),
                system: SystemInfo {
                    home_dir: Some(PathBuf::from("/home/dev")),
                    os: Some("linux".into()),
                    arch: Some("aarch64".into()),
                    cpu_count: Some(8),
                    memory_total_mb: Some(16384),
                    environment: HostEnvironment::Unknown,
                },
                inventory: ToolInventory::default(),
                providers: vec![],
                environments: vec![],
            },
        };
        assert_roundtrip(&snapshot);
    }

    #[test]
    fn system_info_defaults_environment_when_missing() {
        let system: SystemInfo = serde_json::from_str(r#"{"os":"linux"}"#).expect("system info should deserialize");
        assert_eq!(system.environment, HostEnvironment::Unknown);
    }
}
