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

/// How an environment meets one destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReachMeans {
    /// The vessel's own network reaches it, subject to its egress policy.
    Native,
    /// Carried from the controlling host by an outward exposure under the
    /// destination's own name and port, so TLS stays end to end.
    Exposure,
}

impl fmt::Display for ReachMeans {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Native => "native",
            Self::Exposure => "exposure",
        })
    }
}

/// A destination and how the vessel's environment meets it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReachRoute {
    #[serde(flatten)]
    pub destination: ReachDestination,
    pub met: ReachMeans,
}

/// How a provider's vessels meet a destination that resolves, on the
/// controlling host, to a private address.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivateReach {
    /// The vessel shares the controlling host's network view.
    Native,
    /// The provider carries it from the controlling host by an exposure.
    Exposure,
    /// The provider cannot carry it (agentless SSH hosts await #3617).
    /// Placement refuses such a candidate, naming the destination.
    Unmet,
    /// The advertisement does not say: an earlier generation's record, or one
    /// rebuilt from a store that keeps only some capabilities. Placement
    /// refuses nothing on it; provisioning meets the set or fails naming the
    /// destination. Fail-open until the engine's stored Host provider row
    /// carries this capability (#4308).
    #[default]
    Undeclared,
}

impl PrivateReach {
    pub fn is_undeclared(&self) -> bool {
        *self == Self::Undeclared
    }

    /// How a private destination is met, or `None` when it cannot be.
    pub fn means(self) -> Option<ReachMeans> {
        match self {
            Self::Native => Some(ReachMeans::Native),
            Self::Exposure => Some(ReachMeans::Exposure),
            Self::Unmet | Self::Undeclared => None,
        }
    }
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
        let route = ReachRoute { destination: decoded, met: ReachMeans::Exposure };
        let text = serde_json::to_string(&route).unwrap();
        assert_eq!(text, r#"{"host":"forgejo.lab.example","port":443,"met":"exposure"}"#);
        assert_eq!(serde_json::from_str::<ReachRoute>(&text).unwrap(), route);
    }
}
