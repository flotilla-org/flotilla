use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

/// Filesystem-safe identifier for a sandbox environment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EnvironmentId(String);

impl EnvironmentId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EnvironmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
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
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum EnvironmentInfoTagged {
    Direct {
        id: EnvironmentId,
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
            EnvironmentInfoTagged::Direct { id, display_name, status } => Self::Direct { id, display_name, status },
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
            status: EnvironmentStatus::Running,
        };

        assert_roundtrip(&info);
    }

    #[test]
    fn environment_info_defaults_optional_display_metadata_and_image_for_direct_environments() {
        let info: EnvironmentInfo = serde_json::from_str(r#"{"kind":"direct","id":"env-direct","status":"Running"}"#)
            .expect("should deserialize direct environment without image");

        assert_eq!(info, EnvironmentInfo::Direct {
            id: EnvironmentId::new("env-direct"),
            display_name: None,
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
