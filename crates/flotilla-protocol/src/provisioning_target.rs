use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{EnvironmentId, HostName};

/// Where a checkout (or other provisioning action) should be created.
///
/// Display syntax:
/// - `@host` — bare host
/// - `+provider@host` — fresh container on host using the named provider
/// - `=env_id@host` — reuse an existing running environment
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProvisioningTarget {
    /// A bare host with no environment layer.
    Host { host: HostName },
    /// Provision a fresh container on the given host using the named provider.
    NewEnvironment { host: HostName, provider: String },
    /// Reuse an existing running environment on the given host.
    ExistingEnvironment { host: HostName, env_id: EnvironmentId },
}

impl ProvisioningTarget {
    /// The host on which this target lives.
    pub fn host(&self) -> &HostName {
        match self {
            Self::Host { host } => host,
            Self::NewEnvironment { host, .. } => host,
            Self::ExistingEnvironment { host, .. } => host,
        }
    }
}

impl fmt::Display for ProvisioningTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host { host } => write!(f, "@{host}"),
            Self::NewEnvironment { host, provider } => write!(f, "+{provider}@{host}"),
            Self::ExistingEnvironment { host, env_id } => write!(f, "={env_id}@{host}"),
        }
    }
}

impl FromStr for ProvisioningTarget {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        fn non_blank(component: &str) -> Option<&str> {
            let trimmed = component.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }

        if let Some(rest) = s.strip_prefix('@') {
            // `@host`
            let host = non_blank(rest).ok_or_else(|| format!("invalid ProvisioningTarget: host name cannot be empty in '{s}'"))?;
            Ok(Self::Host { host: HostName::new(host) })
        } else if let Some(rest) = s.strip_prefix('+') {
            // `+provider@host`
            if let Some((provider, host)) = rest.split_once('@') {
                let provider =
                    non_blank(provider).ok_or_else(|| format!("invalid ProvisioningTarget: provider cannot be empty in '{s}'"))?;
                let host = non_blank(host).ok_or_else(|| format!("invalid ProvisioningTarget: host cannot be empty in '{s}'"))?;
                Ok(Self::NewEnvironment { host: HostName::new(host), provider: provider.to_string() })
            } else {
                Err(format!("invalid ProvisioningTarget: expected '+provider@host', got '{s}'"))
            }
        } else if let Some(rest) = s.strip_prefix('=') {
            // `=env_id@host`
            if let Some((env_id, host)) = rest.split_once('@') {
                let env_id = non_blank(env_id).ok_or_else(|| format!("invalid ProvisioningTarget: env_id cannot be empty in '{s}'"))?;
                let host = non_blank(host).ok_or_else(|| format!("invalid ProvisioningTarget: host cannot be empty in '{s}'"))?;
                Ok(Self::ExistingEnvironment { host: HostName::new(host), env_id: EnvironmentId::new(env_id) })
            } else {
                Err(format!("invalid ProvisioningTarget: expected '=env_id@host', got '{s}'"))
            }
        } else {
            Err(format!("invalid ProvisioningTarget: expected '@host', '+provider@host', or '=env_id@host', got '{s}'"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::assert_roundtrip;

    // --- Display ---

    #[test]
    fn display_host() {
        let t = ProvisioningTarget::Host { host: HostName::new("myhost") };
        assert_eq!(t.to_string(), "@myhost");
    }

    #[test]
    fn display_new_environment() {
        let t = ProvisioningTarget::NewEnvironment { host: HostName::new("myhost"), provider: "docker".to_string() };
        assert_eq!(t.to_string(), "+docker@myhost");
    }

    #[test]
    fn display_existing_environment() {
        let t = ProvisioningTarget::ExistingEnvironment { host: HostName::new("myhost"), env_id: EnvironmentId::new("env-abc") };
        assert_eq!(t.to_string(), "=env-abc@myhost");
    }

    // --- FromStr ---

    #[test]
    fn parse_host() {
        let t: ProvisioningTarget = "@myhost".parse().expect("parse @myhost");
        assert_eq!(t, ProvisioningTarget::Host { host: HostName::new("myhost") });
    }

    #[test]
    fn parse_new_environment() {
        let t: ProvisioningTarget = "+docker@myhost".parse().expect("parse +docker@myhost");
        assert_eq!(t, ProvisioningTarget::NewEnvironment { host: HostName::new("myhost"), provider: "docker".to_string() });
    }

    #[test]
    fn parse_existing_environment() {
        let t: ProvisioningTarget = "=env-abc@myhost".parse().expect("parse =env-abc@myhost");
        assert_eq!(t, ProvisioningTarget::ExistingEnvironment { host: HostName::new("myhost"), env_id: EnvironmentId::new("env-abc") });
    }

    // --- Roundtrip (Display -> FromStr) ---

    #[test]
    fn roundtrip_host() {
        let t = ProvisioningTarget::Host { host: HostName::new("box") };
        let s = t.to_string();
        let back: ProvisioningTarget = s.parse().expect("roundtrip host");
        assert_eq!(t, back);
    }

    #[test]
    fn roundtrip_new_environment() {
        let t = ProvisioningTarget::NewEnvironment { host: HostName::new("cloud"), provider: "podman".to_string() };
        let s = t.to_string();
        let back: ProvisioningTarget = s.parse().expect("roundtrip new_environment");
        assert_eq!(t, back);
    }

    #[test]
    fn roundtrip_existing_environment() {
        let t = ProvisioningTarget::ExistingEnvironment { host: HostName::new("cloud"), env_id: EnvironmentId::new("env-xyz-123") };
        let s = t.to_string();
        let back: ProvisioningTarget = s.parse().expect("roundtrip existing_environment");
        assert_eq!(t, back);
    }

    // --- Serde roundtrip ---

    #[test]
    fn serde_roundtrip_host() {
        let t = ProvisioningTarget::Host { host: HostName::new("box") };
        assert_roundtrip(&t);
    }

    #[test]
    fn serde_roundtrip_new_environment() {
        let t = ProvisioningTarget::NewEnvironment { host: HostName::new("cloud"), provider: "docker".to_string() };
        assert_roundtrip(&t);
    }

    #[test]
    fn serde_roundtrip_existing_environment() {
        let t = ProvisioningTarget::ExistingEnvironment { host: HostName::new("cloud"), env_id: EnvironmentId::new("env-42") };
        assert_roundtrip(&t);
    }

    // --- host() accessor ---

    #[test]
    fn host_accessor_host_variant() {
        let t = ProvisioningTarget::Host { host: HostName::new("myhost") };
        assert_eq!(t.host(), &HostName::new("myhost"));
    }

    #[test]
    fn host_accessor_new_environment() {
        let t = ProvisioningTarget::NewEnvironment { host: HostName::new("myhost"), provider: "docker".to_string() };
        assert_eq!(t.host(), &HostName::new("myhost"));
    }

    #[test]
    fn host_accessor_existing_environment() {
        let t = ProvisioningTarget::ExistingEnvironment { host: HostName::new("myhost"), env_id: EnvironmentId::new("env-1") };
        assert_eq!(t.host(), &HostName::new("myhost"));
    }

    // --- Error cases ---

    #[test]
    fn parse_error_no_prefix() {
        let result: Result<ProvisioningTarget, _> = "myhost".parse();
        assert!(result.is_err());
    }

    #[test]
    fn parse_error_at_only() {
        let result: Result<ProvisioningTarget, _> = "@".parse();
        assert!(result.is_err());
    }

    #[test]
    fn parse_error_new_env_missing_at() {
        let result: Result<ProvisioningTarget, _> = "+docker".parse();
        assert!(result.is_err());
    }

    #[test]
    fn parse_error_new_env_empty_provider() {
        let result: Result<ProvisioningTarget, _> = "+@myhost".parse();
        assert!(result.is_err());
    }

    #[test]
    fn parse_error_new_env_empty_host() {
        let result: Result<ProvisioningTarget, _> = "+docker@".parse();
        assert!(result.is_err());
    }

    #[test]
    fn parse_error_existing_env_missing_at() {
        let result: Result<ProvisioningTarget, _> = "=env-id".parse();
        assert!(result.is_err());
    }

    #[test]
    fn parse_error_existing_env_empty_id() {
        let result: Result<ProvisioningTarget, _> = "=@myhost".parse();
        assert!(result.is_err());
    }

    #[test]
    fn parse_error_existing_env_empty_host() {
        let result: Result<ProvisioningTarget, _> = "=env-id@".parse();
        assert!(result.is_err());
    }

    // --- Whitespace rejection ---

    #[test]
    fn parse_error_whitespace_only_host() {
        assert!("@ ".parse::<ProvisioningTarget>().is_err());
        assert!("@  \t".parse::<ProvisioningTarget>().is_err());
    }

    #[test]
    fn parse_error_whitespace_only_provider() {
        assert!("+ @host".parse::<ProvisioningTarget>().is_err());
    }

    #[test]
    fn parse_error_whitespace_only_env_id() {
        assert!("= @host".parse::<ProvisioningTarget>().is_err());
    }

    #[test]
    fn parse_error_whitespace_only_host_in_new_env() {
        assert!("+docker@ ".parse::<ProvisioningTarget>().is_err());
    }
}
