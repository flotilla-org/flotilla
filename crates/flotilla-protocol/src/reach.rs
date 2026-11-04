//! What a vessel must reach, and how its environment meets each destination
//! (#4299). The reach set is a requirement of the work, like its toolchains:
//! composed from what the vessel is given and what the fleet and project
//! declare. The composed set is what a later egress allowlist is built from.
use serde::{Deserialize, Serialize};
use std::fmt;

/// One destination a vessel must reach: a host name (or address) and port.
/// Every declaration decodes through [`ReachDestination::new`], so a template,
/// project or fleet entry is validated and its host lowercased wherever it is
/// written.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "DeclaredDestination")]
pub struct ReachDestination {
    pub host: String,
    pub port: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclaredDestination {
    host: String,
    port: u16,
}

impl TryFrom<DeclaredDestination> for ReachDestination {
    type Error = String;

    fn try_from(declared: DeclaredDestination) -> Result<Self, String> {
        Self::new(declared.host, declared.port)
    }
}

impl ReachDestination {
    /// A destination with its host lowercased; refused when the host is empty,
    /// holds whitespace or a separator, or the port is zero.
    pub fn new(host: impl Into<String>, port: u16) -> Result<Self, String> {
        let destination = Self { host: host.into().to_ascii_lowercase(), port };
        destination.validate()?;
        Ok(destination)
    }

    pub fn validate(&self) -> Result<(), String> {
        let host = self.host.strip_prefix('[').and_then(|host| host.strip_suffix(']')).unwrap_or(&self.host);
        if host.is_empty() || host.chars().any(|c| c.is_whitespace() || matches!(c, '/' | '@' | '?' | '#')) {
            return Err(format!("reach destination host `{}` is not a host name or address", self.host));
        }
        if self.port == 0 {
            return Err(format!("reach destination {} needs a nonzero port", self.host));
        }
        Ok(())
    }
}

impl fmt::Display for ReachDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') && !self.host.starts_with('[') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

/// A way a provider instance's vessels can reach a destination beyond the
/// guest's own egress, declared per instance by its module (#4299,
/// flotilla-org/drydock#262), preferred first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReachWay {
    /// The vessel shares the controlling host's network: whatever the host
    /// reaches, the vessel reaches.
    HostNetwork,
    /// The controlling host carries the connection by an outward exposure
    /// under the destination's own name and port, so TLS stays end to end.
    OutwardExposure,
}

/// How a vessel meets one destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReachMeans {
    /// The guest's own egress, subject to its egress policy.
    Egress,
    /// The controlling host's network, which the vessel shares.
    HostNetwork,
    /// An outward exposure carried from the controlling host.
    OutwardExposure,
}

/// A destination and how the vessel's environment meets it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReachRoute {
    #[serde(flatten)]
    pub destination: ReachDestination,
    pub met: ReachMeans,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destinations_refuse_hosts_that_are_not_names() {
        assert_eq!(ReachDestination::new("Forgejo.Lab.Example", 443).unwrap().host, "forgejo.lab.example");
        for (host, port) in [("", 443), ("a b", 443), ("user@host", 22), ("host/path", 443), ("host", 0)] {
            assert!(ReachDestination::new(host, port).is_err(), "{host}:{port}");
        }
        assert_eq!(ReachDestination::new("::1", 8443).unwrap().to_string(), "[::1]:8443");
        let decoded: ReachDestination = serde_json::from_str(r#"{"host":"Forgejo.Lab.Example","port":443}"#).unwrap();
        assert_eq!(decoded.host, "forgejo.lab.example");
        for refused in [r#"{"host":"","port":443}"#, r#"{"host":"host","port":0}"#, r#"{"host":"host","port":443,"extra":1}"#] {
            assert!(serde_json::from_str::<ReachDestination>(refused).is_err(), "{refused}");
        }
        let route = ReachRoute { destination: decoded, met: ReachMeans::OutwardExposure };
        let text = serde_json::to_string(&route).unwrap();
        assert_eq!(text, r#"{"host":"forgejo.lab.example","port":443,"met":"outward_exposure"}"#);
        assert_eq!(serde_json::from_str::<ReachRoute>(&text).unwrap(), route);
        assert_eq!(
            serde_json::to_string(&[ReachWay::OutwardExposure, ReachWay::HostNetwork]).unwrap(),
            r#"["outward_exposure","host_network"]"#
        );
    }
}
