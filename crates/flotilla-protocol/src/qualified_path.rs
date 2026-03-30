use std::{fmt, path::PathBuf, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::EnvironmentId;

/// Unique identifier for a host in the daemon mesh.
///
/// Transparent newtype around String — same pattern as `EnvironmentId`.
/// During migration, `HostName` values map directly to `HostId`.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostId(String);

impl HostId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Qualifier that distinguishes whether a path belongs to a host or a sandbox environment.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum PathQualifier {
    Host(HostId),
    Environment(EnvironmentId),
}

/// A filesystem path qualified by either a host or an environment.
///
/// Replaces `HostPath` with a more general model that supports both
/// host-local and sandboxed-environment paths.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct QualifiedPath {
    pub qualifier: PathQualifier,
    pub path: PathBuf,
}

impl QualifiedPath {
    /// Create a host-qualified path.
    pub fn host(host: HostId, path: impl Into<PathBuf>) -> Self {
        Self { qualifier: PathQualifier::Host(host), path: path.into() }
    }

    /// Create an environment-qualified path.
    pub fn environment(env: EnvironmentId, path: impl Into<PathBuf>) -> Self {
        Self { qualifier: PathQualifier::Environment(env), path: path.into() }
    }

    /// Convert from a legacy HostPath by mapping HostName to HostId.
    /// During migration, HostName string is used directly as HostId.
    pub fn from_host_path(host: &crate::HostName, path: impl Into<PathBuf>) -> Self {
        Self::host(HostId::new(host.as_str()), path)
    }

    /// Returns the `HostId` if this is a host-qualified path, `None` otherwise.
    pub fn host_id(&self) -> Option<&HostId> {
        match &self.qualifier {
            PathQualifier::Host(id) => Some(id),
            PathQualifier::Environment(_) => None,
        }
    }

    /// Returns the `EnvironmentId` if this is an environment-qualified path, `None` otherwise.
    pub fn environment_id(&self) -> Option<&EnvironmentId> {
        match &self.qualifier {
            PathQualifier::Host(_) => None,
            PathQualifier::Environment(id) => Some(id),
        }
    }
}

impl fmt::Display for QualifiedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.qualifier {
            PathQualifier::Host(id) => write!(f, "host:{}:{}", id, self.path.display()),
            PathQualifier::Environment(id) => write!(f, "env:{}:{}", id, self.path.display()),
        }
    }
}

impl FromStr for QualifiedPath {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Format: "host:<id>:<path>" or "env:<id>:<path>"
        // Constraint: <id> must not contain colons (splits on first two colons).
        let (prefix, rest) =
            s.split_once(':').ok_or_else(|| format!("invalid QualifiedPath: expected 'host:id:path' or 'env:id:path', got '{s}'"))?;
        let (id, path) =
            rest.split_once(':').ok_or_else(|| format!("invalid QualifiedPath: expected 'host:id:path' or 'env:id:path', got '{s}'"))?;
        match prefix {
            "host" => Ok(Self::host(HostId::new(id), PathBuf::from(path))),
            "env" => Ok(Self::environment(EnvironmentId::new(id), PathBuf::from(path))),
            other => Err(format!("invalid QualifiedPath prefix: expected 'host' or 'env', got '{other}'")),
        }
    }
}

/// Serde helpers for `IndexMap<QualifiedPath, V>` — serializes keys as `"host:id:path"` or
/// `"env:id:path"` strings so they work as JSON object keys.
pub mod qualified_path_map {
    use std::{fmt, marker::PhantomData};

    use indexmap::IndexMap;
    use serde::{
        de::{self, Deserializer, MapAccess, Visitor},
        ser::{SerializeMap, Serializer},
        Deserialize, Serialize,
    };

    use super::QualifiedPath;

    pub fn serialize<V, S>(map: &IndexMap<QualifiedPath, V>, serializer: S) -> Result<S::Ok, S::Error>
    where
        V: Serialize,
        S: Serializer,
    {
        let mut m = serializer.serialize_map(Some(map.len()))?;
        for (k, v) in map {
            m.serialize_entry(&k.to_string(), v)?;
        }
        m.end()
    }

    pub fn deserialize<'de, V, D>(deserializer: D) -> Result<IndexMap<QualifiedPath, V>, D::Error>
    where
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        struct MapVisitor<V>(PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for MapVisitor<V> {
            type Value = IndexMap<QualifiedPath, V>;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a map with QualifiedPath string keys")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
                let mut map = IndexMap::with_capacity(access.size_hint().unwrap_or(0));
                while let Some((key_str, value)) = access.next_entry::<String, V>()? {
                    let key: QualifiedPath = key_str.parse().map_err(de::Error::custom)?;
                    map.insert(key, value);
                }
                Ok(map)
            }
        }

        deserializer.deserialize_map(MapVisitor(PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use super::*;
    use crate::test_helpers::assert_roundtrip;

    // HostId tests

    #[test]
    fn host_id_display() {
        let h = HostId::new("desktop");
        assert_eq!(h.as_str(), "desktop");
        assert_eq!(format!("{h}"), "desktop");
    }

    #[test]
    fn host_id_equality() {
        let a = HostId::new("desktop");
        let b = HostId::new("desktop");
        let c = HostId::new("laptop");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn host_id_serde_roundtrip() {
        let h = HostId::new("cloud-vm");
        let json = serde_json::to_string(&h).expect("serialize");
        assert_eq!(json, "\"cloud-vm\"");
        let back: HostId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(h, back);
    }

    // QualifiedPath tests — host variant

    #[test]
    fn qualified_path_host_display() {
        let qp = QualifiedPath::host(HostId::new("desktop"), "/Users/dev/project");
        assert_eq!(format!("{qp}"), "host:desktop:/Users/dev/project");
    }

    #[test]
    fn qualified_path_host_accessors() {
        let qp = QualifiedPath::host(HostId::new("desktop"), "/home/dev");
        assert_eq!(qp.host_id(), Some(&HostId::new("desktop")));
        assert_eq!(qp.environment_id(), None);
    }

    // QualifiedPath tests — environment variant

    #[test]
    fn qualified_path_environment_display() {
        let qp = QualifiedPath::environment(EnvironmentId::new("sandbox-1"), "/workspace/repo");
        assert_eq!(format!("{qp}"), "env:sandbox-1:/workspace/repo");
    }

    #[test]
    fn qualified_path_environment_accessors() {
        let qp = QualifiedPath::environment(EnvironmentId::new("sandbox-1"), "/workspace");
        assert_eq!(qp.environment_id(), Some(&EnvironmentId::new("sandbox-1")));
        assert_eq!(qp.host_id(), None);
    }

    // QualifiedPath equality

    #[test]
    fn qualified_path_equality_same() {
        let a = QualifiedPath::host(HostId::new("laptop"), "/home/dev/repo");
        let b = QualifiedPath::host(HostId::new("laptop"), "/home/dev/repo");
        assert_eq!(a, b);
    }

    #[test]
    fn qualified_path_equality_different_qualifier() {
        let host = QualifiedPath::host(HostId::new("laptop"), "/home/dev/repo");
        let env = QualifiedPath::environment(EnvironmentId::new("laptop"), "/home/dev/repo");
        assert_ne!(host, env);
    }

    #[test]
    fn qualified_path_equality_different_hosts() {
        let a = QualifiedPath::host(HostId::new("laptop"), "/home/dev/repo");
        let b = QualifiedPath::host(HostId::new("desktop"), "/home/dev/repo");
        assert_ne!(a, b);
    }

    // QualifiedPath serde

    #[test]
    fn qualified_path_serde_roundtrip_host() {
        let qp = QualifiedPath::host(HostId::new("cloud"), "/opt/repos/app");
        assert_roundtrip(&qp);
    }

    #[test]
    fn qualified_path_serde_roundtrip_environment() {
        let qp = QualifiedPath::environment(EnvironmentId::new("sandbox-1"), "/workspace/repo");
        assert_roundtrip(&qp);
    }

    // QualifiedPath FromStr

    #[test]
    fn qualified_path_from_str_host() {
        let qp: QualifiedPath = "host:desktop:/Users/dev/project".parse().expect("parse host path");
        assert_eq!(qp, QualifiedPath::host(HostId::new("desktop"), "/Users/dev/project"));
    }

    #[test]
    fn qualified_path_from_str_environment() {
        let qp: QualifiedPath = "env:sandbox-1:/workspace/repo".parse().expect("parse env path");
        assert_eq!(qp, QualifiedPath::environment(EnvironmentId::new("sandbox-1"), "/workspace/repo"));
    }

    #[test]
    fn qualified_path_from_str_invalid_no_colon() {
        let result: Result<QualifiedPath, _> = "nocolon".parse();
        assert!(result.is_err());
    }

    #[test]
    fn qualified_path_from_str_invalid_one_colon() {
        let result: Result<QualifiedPath, _> = "host:nopathsep".parse();
        assert!(result.is_err());
    }

    #[test]
    fn qualified_path_from_str_invalid_prefix() {
        let result: Result<QualifiedPath, _> = "unknown:id:/path".parse();
        assert!(result.is_err());
    }

    #[test]
    fn qualified_path_display_roundtrips_through_from_str() {
        let cases = vec![
            QualifiedPath::host(HostId::new("desktop"), "/Users/dev/project"),
            QualifiedPath::environment(EnvironmentId::new("sandbox-1"), "/workspace/repo"),
        ];
        for qp in cases {
            let s = qp.to_string();
            let parsed: QualifiedPath = s.parse().expect("roundtrip parse");
            assert_eq!(parsed, qp);
        }
    }

    // from_host_path migration helper

    #[test]
    fn qualified_path_from_host_path() {
        let host_name = crate::HostName::new("desktop");
        let qp = QualifiedPath::from_host_path(&host_name, "/home/dev/repo");
        assert_eq!(qp, QualifiedPath::host(HostId::new("desktop"), "/home/dev/repo"));
    }

    // qualified_path_map serde

    #[test]
    fn qualified_path_map_serde_roundtrip() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Wrapper {
            #[serde(with = "qualified_path_map")]
            repos: IndexMap<QualifiedPath, String>,
        }

        let mut repos = IndexMap::new();
        repos.insert(QualifiedPath::host(HostId::new("desktop"), "/home/dev/alpha"), "alpha".to_string());
        repos.insert(QualifiedPath::environment(EnvironmentId::new("sandbox-1"), "/workspace/beta"), "beta".to_string());

        let wrapper = Wrapper { repos };
        let json = serde_json::to_string(&wrapper).expect("serialize");
        let back: Wrapper = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(wrapper, back);
    }
}
