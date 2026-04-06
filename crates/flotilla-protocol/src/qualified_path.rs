use std::{fmt, path::PathBuf, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{EnvironmentId, HostPath};

/// Unique identifier for a host machine.
///
/// A UUID generated on first daemon start and stored on-disk. Stable across
/// hostname changes, reboots, and network reconfigurations.
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

/// Qualifier that distinguishes which namespace a path belongs to.
///
/// Three variants track migration progress at the type level:
/// - `Host(HostId)` — real stable identity (UUID). The target state.
/// - `HostName(HostName)` — legacy hostname-derived qualifier. Each usage is
///   a migration target for future phases.
/// - `Environment(EnvironmentId)` — sandboxed execution environment.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum PathQualifier {
    Host(HostId),
    HostName(crate::HostName),
    Environment(EnvironmentId),
}

/// A filesystem path qualified by a host, hostname, or environment.
///
/// Used as the identity key for checkouts in `ProviderData`, correlation keys,
/// and work item identity. The qualifier determines which namespace the path
/// belongs to.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct QualifiedPath {
    pub qualifier: PathQualifier,
    pub path: PathBuf,
}

impl QualifiedPath {
    /// Create a path qualified by a real stable host identity (UUID).
    pub fn host(host: HostId, path: impl Into<PathBuf>) -> Self {
        Self { qualifier: PathQualifier::Host(host), path: path.into() }
    }

    /// Create a path qualified by a legacy hostname.
    /// Every call site is a migration target — grep for `from_host_name` to find them.
    pub fn from_host_name(host: &crate::HostName, path: impl Into<PathBuf>) -> Self {
        Self { qualifier: PathQualifier::HostName(host.clone()), path: path.into() }
    }

    /// Create an environment-qualified path.
    pub fn environment(env: EnvironmentId, path: impl Into<PathBuf>) -> Self {
        Self { qualifier: PathQualifier::Environment(env), path: path.into() }
    }

    /// Returns the `HostId` if this is a `Host`-qualified path.
    pub fn host_id(&self) -> Option<&HostId> {
        match &self.qualifier {
            PathQualifier::Host(id) => Some(id),
            _ => None,
        }
    }

    /// Returns the `HostName` if this is a `HostName`-qualified path.
    pub fn host_name(&self) -> Option<&crate::HostName> {
        match &self.qualifier {
            PathQualifier::HostName(name) => Some(name),
            _ => None,
        }
    }

    /// Returns the `EnvironmentId` if this is an `Environment`-qualified path.
    pub fn environment_id(&self) -> Option<&EnvironmentId> {
        match &self.qualifier {
            PathQualifier::Environment(id) => Some(id),
            _ => None,
        }
    }

    /// Returns true if this path is owned by the given `HostId`.
    pub fn is_owned_by_host_id(&self, id: &HostId) -> bool {
        matches!(&self.qualifier, PathQualifier::Host(h) if h == id)
    }

    /// Returns true if this path is owned by the given `HostName`.
    pub fn is_owned_by_host_name(&self, name: &crate::HostName) -> bool {
        matches!(&self.qualifier, PathQualifier::HostName(n) if n == name)
    }
}

impl From<HostPath> for QualifiedPath {
    fn from(value: HostPath) -> Self {
        Self::from_host_name(&value.host, value.path)
    }
}

impl From<&HostPath> for QualifiedPath {
    fn from(value: &HostPath) -> Self {
        Self::from_host_name(&value.host, value.path.clone())
    }
}

impl TryFrom<QualifiedPath> for HostPath {
    type Error = QualifiedPath;

    fn try_from(value: QualifiedPath) -> Result<Self, Self::Error> {
        match value.qualifier {
            PathQualifier::HostName(host) => Ok(Self::new(host, value.path)),
            _ => Err(value),
        }
    }
}

impl TryFrom<&QualifiedPath> for HostPath {
    type Error = ();

    fn try_from(value: &QualifiedPath) -> Result<Self, Self::Error> {
        match &value.qualifier {
            PathQualifier::HostName(host) => Ok(Self::new(host.clone(), value.path.clone())),
            _ => Err(()),
        }
    }
}

impl fmt::Display for QualifiedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.qualifier {
            PathQualifier::Host(id) => write!(f, "host:{}:{}", id, self.path.display()),
            PathQualifier::HostName(name) => write!(f, "hn:{}:{}", name, self.path.display()),
            PathQualifier::Environment(id) => write!(f, "env:{}:{}", id, self.path.display()),
        }
    }
}

impl FromStr for QualifiedPath {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Format: "host:<id>:<path>", "hn:<name>:<path>", or "env:<id>:<path>"
        // Constraint: <id>/<name> must not contain colons (splits on first two colons).
        let (prefix, rest) = s
            .split_once(':')
            .ok_or_else(|| format!("invalid QualifiedPath: expected 'host:id:path', 'hn:name:path', or 'env:id:path', got '{s}'"))?;
        let (id, path) = rest
            .split_once(':')
            .ok_or_else(|| format!("invalid QualifiedPath: expected 'host:id:path', 'hn:name:path', or 'env:id:path', got '{s}'"))?;
        match prefix {
            "host" => Ok(Self::host(HostId::new(id), PathBuf::from(path))),
            "hn" => Ok(Self::from_host_name(&crate::HostName::new(id), PathBuf::from(path))),
            "env" => Ok(Self::environment(EnvironmentId::new(id), PathBuf::from(path))),
            other => Err(format!("invalid QualifiedPath prefix: expected 'host', 'hn', or 'env', got '{other}'")),
        }
    }
}

/// Serde helpers for `IndexMap<QualifiedPath, V>` — serializes keys as display strings
/// so they work as JSON object keys.
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

/// Serde helpers for `QualifiedPath` fields that must accept legacy `HostPath`
/// values during migration.
pub mod qualified_path_or_host_path {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::QualifiedPath;
    use crate::HostPath;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Qualified(QualifiedPath),
        Legacy(HostPath),
    }

    // Serialization is a straight pass-through; only deserialization needs the
    // legacy HostPath fallback for migration compatibility.
    pub fn serialize<S>(path: &QualifiedPath, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        path.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<QualifiedPath, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Repr::deserialize(deserializer)? {
            Repr::Qualified(path) => Ok(path),
            Repr::Legacy(path) => Ok(QualifiedPath::from(path)),
        }
    }

    pub mod option {
        use serde::{Deserialize, Deserializer, Serialize, Serializer};

        use super::{QualifiedPath, Repr};

        pub fn serialize<S>(path: &Option<QualifiedPath>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            path.serialize(serializer)
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<QualifiedPath>, D::Error>
        where
            D: Deserializer<'de>,
        {
            #[derive(Deserialize)]
            #[serde(untagged)]
            enum MaybeRepr {
                Some(Repr),
                None(()),
            }

            match MaybeRepr::deserialize(deserializer)? {
                MaybeRepr::Some(Repr::Qualified(path)) => Ok(Some(path)),
                MaybeRepr::Some(Repr::Legacy(path)) => Ok(Some(QualifiedPath::from(path))),
                MaybeRepr::None(_) => Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use super::*;
    use crate::test_helpers::assert_roundtrip;

    #[test]
    fn host_id_display() {
        let h = HostId::new("a3f8-uuid");
        assert_eq!(h.as_str(), "a3f8-uuid");
        assert_eq!(format!("{h}"), "a3f8-uuid");
    }

    #[test]
    fn host_id_equality() {
        assert_eq!(HostId::new("a"), HostId::new("a"));
        assert_ne!(HostId::new("a"), HostId::new("b"));
    }

    #[test]
    fn host_id_serde_roundtrip() {
        let h = HostId::new("cloud-vm");
        let json = serde_json::to_string(&h).expect("serialize");
        assert_eq!(json, "\"cloud-vm\"");
        let back: HostId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(h, back);
    }

    // QualifiedPath — Host variant (real HostId)

    #[test]
    fn qualified_path_host_display() {
        let qp = QualifiedPath::host(HostId::new("uuid-123"), "/home/dev/repo");
        assert_eq!(format!("{qp}"), "host:uuid-123:/home/dev/repo");
    }

    #[test]
    fn qualified_path_host_accessors() {
        let qp = QualifiedPath::host(HostId::new("uuid-123"), "/home/dev");
        assert_eq!(qp.host_id(), Some(&HostId::new("uuid-123")));
        assert_eq!(qp.host_name(), None);
        assert_eq!(qp.environment_id(), None);
        assert!(qp.is_owned_by_host_id(&HostId::new("uuid-123")));
        assert!(!qp.is_owned_by_host_name(&crate::HostName::new("desktop")));
    }

    // QualifiedPath — HostName variant (legacy)

    #[test]
    fn qualified_path_hostname_display() {
        let qp = QualifiedPath::from_host_name(&crate::HostName::new("desktop"), "/home/dev/repo");
        assert_eq!(format!("{qp}"), "hn:desktop:/home/dev/repo");
    }

    #[test]
    fn qualified_path_hostname_accessors() {
        let qp = QualifiedPath::from_host_name(&crate::HostName::new("desktop"), "/home/dev");
        assert_eq!(qp.host_id(), None);
        assert_eq!(qp.host_name(), Some(&crate::HostName::new("desktop")));
        assert_eq!(qp.environment_id(), None);
        assert!(!qp.is_owned_by_host_id(&HostId::new("uuid-123")));
        assert!(qp.is_owned_by_host_name(&crate::HostName::new("desktop")));
    }

    #[test]
    fn qualified_path_from_host_path_uses_legacy_hostname_qualifier() {
        let hp = HostPath::new(crate::HostName::new("desktop"), "/home/dev/repo");
        let qp = QualifiedPath::from(hp.clone());
        assert_eq!(qp, QualifiedPath::from_host_name(&crate::HostName::new("desktop"), "/home/dev/repo"));
        assert_eq!(HostPath::try_from(qp).expect("legacy hostname path should convert back"), hp);
    }

    #[test]
    fn host_path_try_from_rejects_non_hostname_qualified_paths() {
        let qp = QualifiedPath::host(HostId::new("uuid-123"), "/home/dev/repo");
        assert!(HostPath::try_from(qp).is_err());
    }

    // QualifiedPath — Environment variant

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
        assert_eq!(qp.host_name(), None);
    }

    // Equality across variants

    #[test]
    fn host_and_hostname_are_not_equal() {
        let host = QualifiedPath::host(HostId::new("desktop"), "/home/dev/repo");
        let hostname = QualifiedPath::from_host_name(&crate::HostName::new("desktop"), "/home/dev/repo");
        assert_ne!(host, hostname, "Host(HostId) and HostName(HostName) must be distinct even with same string");
    }

    #[test]
    fn host_and_environment_are_not_equal() {
        let host = QualifiedPath::host(HostId::new("laptop"), "/home/dev/repo");
        let env = QualifiedPath::environment(EnvironmentId::new("laptop"), "/home/dev/repo");
        assert_ne!(host, env);
    }

    // Serde roundtrips

    #[test]
    fn qualified_path_serde_roundtrip_host() {
        assert_roundtrip(&QualifiedPath::host(HostId::new("uuid-123"), "/opt/repos/app"));
    }

    #[test]
    fn qualified_path_serde_roundtrip_hostname() {
        assert_roundtrip(&QualifiedPath::from_host_name(&crate::HostName::new("desktop"), "/home/dev/repo"));
    }

    #[test]
    fn qualified_path_serde_roundtrip_environment() {
        assert_roundtrip(&QualifiedPath::environment(EnvironmentId::new("sandbox-1"), "/workspace/repo"));
    }

    // FromStr

    #[test]
    fn qualified_path_from_str_host() {
        let qp: QualifiedPath = "host:uuid-123:/home/dev/repo".parse().expect("parse");
        assert_eq!(qp, QualifiedPath::host(HostId::new("uuid-123"), "/home/dev/repo"));
    }

    #[test]
    fn qualified_path_from_str_hostname() {
        let qp: QualifiedPath = "hn:desktop:/home/dev/repo".parse().expect("parse");
        assert_eq!(qp, QualifiedPath::from_host_name(&crate::HostName::new("desktop"), "/home/dev/repo"));
    }

    #[test]
    fn qualified_path_from_str_environment() {
        let qp: QualifiedPath = "env:sandbox-1:/workspace/repo".parse().expect("parse");
        assert_eq!(qp, QualifiedPath::environment(EnvironmentId::new("sandbox-1"), "/workspace/repo"));
    }

    #[test]
    fn qualified_path_from_str_invalid() {
        assert!("nocolon".parse::<QualifiedPath>().is_err());
        assert!("host:nopathsep".parse::<QualifiedPath>().is_err());
        assert!("unknown:id:/path".parse::<QualifiedPath>().is_err());
    }

    #[test]
    fn qualified_path_display_roundtrips() {
        let cases = vec![
            QualifiedPath::host(HostId::new("uuid-123"), "/home/dev/repo"),
            QualifiedPath::from_host_name(&crate::HostName::new("desktop"), "/home/dev/repo"),
            QualifiedPath::environment(EnvironmentId::new("sandbox-1"), "/workspace/repo"),
        ];
        for qp in cases {
            let s = qp.to_string();
            let parsed: QualifiedPath = s.parse().expect("roundtrip parse");
            assert_eq!(parsed, qp);
        }
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
        repos.insert(QualifiedPath::host(HostId::new("uuid-1"), "/home/dev/alpha"), "alpha".to_string());
        repos.insert(QualifiedPath::from_host_name(&crate::HostName::new("desktop"), "/home/dev/beta"), "beta".to_string());
        repos.insert(QualifiedPath::environment(EnvironmentId::new("sandbox-1"), "/workspace/gamma"), "gamma".to_string());

        let wrapper = Wrapper { repos };
        let json = serde_json::to_string(&wrapper).expect("serialize");
        let back: Wrapper = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(wrapper, back);
    }
}
