use std::{fmt, path::PathBuf, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::qualified_path::HostId;

/// Filesystem-safe identifier for a sandbox environment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EnvironmentId {
    Host(HostId),
    Provisioned(String),
}

impl EnvironmentId {
    pub fn new(id: impl Into<String>) -> Self {
        Self::Provisioned(id.into())
    }

    pub fn host(host_id: HostId) -> Self {
        Self::Host(host_id)
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Host(host_id) => host_id.as_str(),
            Self::Provisioned(id) => id,
        }
    }

    pub fn canonical_string(&self) -> String {
        match self {
            Self::Host(host_id) => format!("host:{host_id}"),
            Self::Provisioned(id) => format!("prov:{id}"),
        }
    }

    pub fn is_host(&self) -> bool {
        matches!(self, Self::Host(_))
    }

    pub fn host_id(&self) -> Option<&HostId> {
        match self {
            Self::Host(host_id) => Some(host_id),
            Self::Provisioned(_) => None,
        }
    }

    pub fn provisioned_id(&self) -> Option<&str> {
        match self {
            Self::Host(_) => None,
            Self::Provisioned(id) => Some(id),
        }
    }

    pub(crate) fn qualified_path_component(&self) -> String {
        fn encode(raw: &str) -> String {
            let mut encoded = String::with_capacity(raw.len() * 2);
            for byte in raw.as_bytes() {
                use std::fmt::Write as _;
                let _ = write!(&mut encoded, "{byte:02x}");
            }
            encoded
        }

        match self {
            Self::Host(host_id) => format!("host~{}", encode(host_id.as_str())),
            Self::Provisioned(id) => format!("prov~{}", encode(id)),
        }
    }

    pub(crate) fn from_qualified_path_component(component: &str) -> Result<Self, String> {
        fn decode(encoded: &str) -> Result<String, String> {
            if !encoded.len().is_multiple_of(2) {
                return Err(format!("invalid environment id encoding: expected an even number of hex digits, got '{encoded}'"));
            }

            let mut bytes = Vec::with_capacity(encoded.len() / 2);
            for chunk in encoded.as_bytes().chunks_exact(2) {
                let pair = std::str::from_utf8(chunk).map_err(|err| format!("invalid environment id encoding: {err}"))?;
                let byte = u8::from_str_radix(pair, 16)
                    .map_err(|err| format!("invalid environment id encoding: failed to decode '{pair}': {err}"))?;
                bytes.push(byte);
            }

            String::from_utf8(bytes).map_err(|err| format!("invalid environment id encoding: {err}"))
        }

        if let Some((kind, encoded)) = component.split_once('~') {
            let raw = decode(encoded)?;
            match kind {
                "host" => Ok(Self::Host(HostId::new(raw))),
                "prov" => Ok(Self::Provisioned(raw)),
                other => Err(format!("invalid environment id encoding: expected 'host' or 'prov', got '{other}'")),
            }
        } else {
            Ok(Self::Provisioned(component.to_string()))
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        raw.parse()
    }
}

impl fmt::Display for EnvironmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for EnvironmentId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Host(host_id) => serializer.serialize_str(&format!("host:{host_id}")),
            Self::Provisioned(id) => serializer.serialize_str(&format!("prov:{id}")),
        }
    }
}

impl<'de> Deserialize<'de> for EnvironmentId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        if let Some(host_id) = raw.strip_prefix("host:") {
            if host_id.is_empty() {
                return Err(serde::de::Error::custom("host environment id must not be empty"));
            }
            return Ok(Self::Host(HostId::new(host_id)));
        }
        if let Some(id) = raw.strip_prefix("prov:") {
            return Ok(Self::Provisioned(id.to_string()));
        }
        Ok(Self::Provisioned(raw))
    }
}

impl FromStr for EnvironmentId {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if let Some(host_id) = raw.strip_prefix("host:") {
            if host_id.is_empty() {
                return Err("host environment id must not be empty".into());
            }
            return Ok(Self::Host(HostId::new(host_id)));
        }
        if let Some(id) = raw.strip_prefix("prov:") {
            return Ok(Self::Provisioned(id.to_string()));
        }
        Ok(Self::Provisioned(raw.to_string()))
    }
}

/// Specification for how to provision a sandbox environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentSpec {
    pub image: ImageSource,
    pub token_env_vars: Vec<String>,
}

/// Source from which to obtain a container image.
///
/// In YAML config, written as a map with one key:
/// ```yaml
/// image:
///   dockerfile: .flotilla/Dockerfile.dev-env
/// ```
/// or:
/// ```yaml
/// image:
///   registry: ubuntu:24.04
/// ```
/// Source from which to obtain a container image.
///
/// In YAML config, written as a map with one key:
/// ```yaml
/// image:
///   dockerfile: .flotilla/Dockerfile.dev-env
/// ```
/// or:
/// ```yaml
/// image:
///   registry: ubuntu:24.04
/// ```
///
/// Custom serde impls because serde_yml uses YAML tags (`!dockerfile path`) for
/// externally-tagged enums, which is unfriendly for hand-written config files.
/// These impls produce plain map keys (`dockerfile: path`) instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    Dockerfile(PathBuf),
    Registry(String),
}

impl Serialize for ImageSource {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            ImageSource::Dockerfile(path) => map.serialize_entry("dockerfile", path)?,
            ImageSource::Registry(image) => map.serialize_entry("registry", image)?,
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for ImageSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use std::collections::HashMap;
        let map: HashMap<String, String> = HashMap::deserialize(deserializer)?;
        if let Some(path) = map.get("dockerfile") {
            Ok(ImageSource::Dockerfile(PathBuf::from(path)))
        } else if let Some(image) = map.get("registry") {
            Ok(ImageSource::Registry(image.clone()))
        } else {
            Err(serde::de::Error::custom("expected 'dockerfile' or 'registry' key in image"))
        }
    }
}

/// Identifier for a built container image.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ImageId(String);

impl ImageId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lifecycle status of a sandbox environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvironmentStatus {
    Building,
    Starting,
    Running,
    Stopped,
    Failed(String),
}

/// Kind of managed environment visible in protocol summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentKind {
    Direct,
    Provisioned,
}

/// Runtime information about a visible managed environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvironmentInfo {
    Direct {
        id: EnvironmentId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host_id: Option<HostId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
        status: EnvironmentStatus,
    },
    Provisioned {
        id: EnvironmentId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
        image: ImageId,
        status: EnvironmentStatus,
    },
}

impl EnvironmentInfo {
    pub fn kind(&self) -> EnvironmentKind {
        match self {
            Self::Direct { .. } => EnvironmentKind::Direct,
            Self::Provisioned { .. } => EnvironmentKind::Provisioned,
        }
    }

    pub fn environment_id(&self) -> &EnvironmentId {
        match self {
            Self::Direct { id, .. } | Self::Provisioned { id, .. } => id,
        }
    }

    pub fn display_name(&self) -> Option<&str> {
        match self {
            Self::Direct { display_name, .. } | Self::Provisioned { display_name, .. } => display_name.as_deref(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum EnvironmentInfoTagged {
    Direct {
        id: EnvironmentId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host_id: Option<HostId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
        status: EnvironmentStatus,
    },
    Provisioned {
        id: EnvironmentId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
        image: ImageId,
        status: EnvironmentStatus,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyProvisionedEnvironmentInfo {
    id: EnvironmentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    image: ImageId,
    status: EnvironmentStatus,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum EnvironmentInfoRepr {
    Tagged(EnvironmentInfoTagged),
    LegacyProvisioned(LegacyProvisionedEnvironmentInfo),
}

impl From<EnvironmentInfoTagged> for EnvironmentInfo {
    fn from(value: EnvironmentInfoTagged) -> Self {
        match value {
            EnvironmentInfoTagged::Direct { id, host_id, display_name, status } => Self::Direct { id, host_id, display_name, status },
            EnvironmentInfoTagged::Provisioned { id, display_name, image, status } => Self::Provisioned { id, display_name, image, status },
        }
    }
}

impl From<LegacyProvisionedEnvironmentInfo> for EnvironmentInfo {
    fn from(value: LegacyProvisionedEnvironmentInfo) -> Self {
        Self::Provisioned { id: value.id, display_name: value.display_name, image: value.image, status: value.status }
    }
}

impl<'de> Deserialize<'de> for EnvironmentInfo {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match EnvironmentInfoRepr::deserialize(deserializer)? {
            EnvironmentInfoRepr::Tagged(value) => Ok(value.into()),
            EnvironmentInfoRepr::LegacyProvisioned(value) => Ok(value.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::assert_roundtrip;

    #[test]
    fn parse_environment_yaml_dockerfile() {
        let yaml = r#"
image:
  dockerfile: .flotilla/Dockerfile.dev-env
token_env_vars:
  - GITHUB_TOKEN
"#;
        let spec: EnvironmentSpec = serde_yml::from_str(yaml).expect("should parse dockerfile variant");
        assert_eq!(spec.image, ImageSource::Dockerfile(PathBuf::from(".flotilla/Dockerfile.dev-env")));
        assert_eq!(spec.token_env_vars, vec!["GITHUB_TOKEN"]);
    }

    #[test]
    fn parse_environment_yaml_registry() {
        let yaml = r#"
image:
  registry: ubuntu:24.04
token_env_vars: []
"#;
        let spec: EnvironmentSpec = serde_yml::from_str(yaml).expect("should parse registry variant");
        assert_eq!(spec.image, ImageSource::Registry("ubuntu:24.04".into()));
        assert!(spec.token_env_vars.is_empty());
    }

    #[test]
    fn parse_environment_yaml_no_tokens() {
        let yaml = r#"
image:
  dockerfile: Dockerfile
token_env_vars: []
"#;
        let spec: EnvironmentSpec = serde_yml::from_str(yaml).expect("should parse with empty tokens");
        assert_eq!(spec.image, ImageSource::Dockerfile(PathBuf::from("Dockerfile")));
    }

    #[test]
    fn environment_info_roundtrips_direct_environment_without_image() {
        let info = EnvironmentInfo::Direct {
            id: EnvironmentId::new("env-direct"),
            display_name: Some("ssh-dev".into()),
            host_id: None,
            status: EnvironmentStatus::Running,
        };

        assert_roundtrip(&info);
    }

    #[test]
    fn environment_id_roundtrips_host_variant() {
        let id = EnvironmentId::host(HostId::new("desktop-host"));

        assert_roundtrip(&id);
    }

    #[test]
    fn environment_id_roundtrips_provisioned_value_with_reserved_prefix() {
        let id = EnvironmentId::new("host:looks-like-a-host");

        let json = serde_json::to_string(&id).expect("serialize");
        let decoded: EnvironmentId = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(decoded, id);
        assert!(!decoded.is_host(), "reserved host prefix must not flip a provisioned environment into a host environment");
    }

    #[test]
    fn environment_id_roundtrips_provisioned_value_with_provisioned_prefix() {
        let id = EnvironmentId::new("prov:foo");

        assert_roundtrip(&id);
    }

    #[test]
    fn environment_id_parse_recovers_host_variant_from_canonical_string() {
        assert_eq!(
            EnvironmentId::parse("host:desktop-host").expect("parse host environment id"),
            EnvironmentId::host(HostId::new("desktop-host"))
        );
    }

    #[test]
    fn environment_id_parse_recovers_provisioned_variant_from_canonical_string() {
        assert_eq!(EnvironmentId::parse("prov:builder-1").expect("parse provisioned environment id"), EnvironmentId::new("builder-1"));
    }

    #[test]
    fn environment_info_defaults_optional_display_metadata_and_image_for_direct_environments() {
        let info: EnvironmentInfo = serde_json::from_str(r#"{"kind":"direct","id":"env-direct","status":"Running"}"#)
            .expect("should deserialize direct environment without image");

        assert_eq!(info, EnvironmentInfo::Direct {
            id: EnvironmentId::new("env-direct"),
            display_name: None,
            host_id: None,
            status: EnvironmentStatus::Running,
        });
    }

    #[test]
    fn environment_info_roundtrips_provisioned_environment_with_image() {
        let info = EnvironmentInfo::Provisioned {
            id: EnvironmentId::new("env-provisioned"),
            display_name: None,
            image: ImageId::new("ubuntu:24.04"),
            status: EnvironmentStatus::Stopped,
        };

        assert_roundtrip(&info);
    }

    #[test]
    fn environment_info_requires_image_for_provisioned_environments() {
        serde_json::from_str::<EnvironmentInfo>(r#"{"kind":"provisioned","id":"env-provisioned","status":"Stopped"}"#)
            .expect_err("provisioned environments should require an image");
    }

    #[test]
    fn environment_info_rejects_images_for_direct_environments() {
        serde_json::from_str::<EnvironmentInfo>(r#"{"kind":"direct","id":"env-direct","image":"ubuntu:24.04","status":"Running"}"#)
            .expect_err("direct environments should not accept an image");
    }

    #[test]
    fn environment_info_deserializes_legacy_provisioned_shape_without_kind() {
        let info: EnvironmentInfo = serde_json::from_str(r#"{"id":"env-provisioned","image":"ubuntu:24.04","status":"Stopped"}"#)
            .expect("legacy provisioned environments without kind should still deserialize");

        assert_eq!(info, EnvironmentInfo::Provisioned {
            id: EnvironmentId::new("env-provisioned"),
            display_name: None,
            image: ImageId::new("ubuntu:24.04"),
            status: EnvironmentStatus::Stopped,
        });
    }
}
