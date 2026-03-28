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

/// Runtime information about a sandbox environment instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentInfo {
    pub id: EnvironmentId,
    pub image: ImageId,
    pub status: EnvironmentStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
