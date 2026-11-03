use serde::{Deserialize, Serialize};

/// Reserved name prefix of the Docker kinds seeding writes.
pub const DOCKER_POLICY_NAME_PREFIX: &str = "docker-on-";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockerImagePullPolicy {
    #[default]
    Always,
    IfNotPresent,
    Never,
}

/// A placement-specific literal, or the per-identity composed image admission froze into a
/// prepared snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PlacementImageSource {
    Literal(String),
    /// The vessel's composed image: its identity and the per-identity ImageBuild
    /// (`image-<hex>-<arch>`) that realises it on the placement architecture.
    Built {
        identity: String,
        image_build_ref: String,
    },
}

/// Decode an optional image source, dropping the ADR 0053
/// `{composition: {selection…}}` form as omitted. ADR 0047: remove the
/// composition shim one roll after ADR 0062 images land. A retired
/// CrewImageBaseline reference decodes as omitted too; remove that arm one roll
/// after CrewImageBaseline retires.
pub fn decode_optional_image_source<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<PlacementImageSource>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Object(object)) if object.contains_key("composition") => {
            Ok(retired_composition_image(&object["composition"]))
        }
        Some(serde_json::Value::Object(object)) if object.contains_key("image_baseline_ref") => {
            tracing::warn!(
                baseline = %object["image_baseline_ref"],
                "an image names a retired CrewImageBaseline; it runs the vessel's composed image"
            );
            Ok(None)
        }
        Some(value) => serde_json::from_value(value).map(Some).map_err(serde::de::Error::custom),
    }
}

/// The image an ADR 0053 composition already carried, with the previous
/// resolution precedence: a bound identity (registry digest, else local ID),
/// then a literal base, then a generation-1 baseline image. A selection-only
/// composition carried none and decodes as omitted. ADR 0047: remove with the
/// composition shim one roll after ADR 0062 images land.
fn retired_composition_image(composition: &serde_json::Value) -> Option<PlacementImageSource> {
    fn text(value: Option<&serde_json::Value>) -> Option<&str> {
        value.and_then(serde_json::Value::as_str).filter(|text| !text.trim().is_empty())
    }
    let identity = composition.get("identity");
    text(identity.and_then(|identity| identity.get("registry_digest")))
        .or_else(|| text(identity.and_then(|identity| identity.get("local_image_id"))))
        .or_else(|| text(composition.get("literal_base")))
        .or_else(|| text(composition.get("baseline_image")))
        .map(|image| PlacementImageSource::Literal(image.to_string()))
}

impl From<String> for PlacementImageSource {
    fn from(image: String) -> Self {
        Self::Literal(image)
    }
}

impl From<&str> for PlacementImageSource {
    fn from(image: &str) -> Self {
        Self::Literal(image.to_string())
    }
}

#[cfg(test)]
mod image_source_tests {
    use super::*;
    use crate::{FulfilmentKindSpec, FulfilmentRealisation};

    fn kind(image: Option<serde_json::Value>) -> serde_json::Value {
        let mut kind = serde_json::json!({"host_ref": "feta", "pool": "lab", "grants": [], "realisation": "docker_per_vessel"});
        if let Some(image) = image {
            kind["image"] = image;
        }
        kind
    }

    #[test]
    fn composition_is_dropped_and_built_round_trips() {
        let composition = serde_json::json!({"composition": {"selection": {"base": "base", "utilities": null, "harness": null, "project": null}, "layers": []}});
        for (image, expected) in [
            (None, None),
            (Some(serde_json::Value::Null), None),
            (Some(composition), None),
            (
                Some(serde_json::json!("registry.test/crew@sha256:abc")),
                Some(PlacementImageSource::Literal("registry.test/crew@sha256:abc".into())),
            ),
            (Some(serde_json::json!({"image_baseline_ref": "crew"})), None),
            (
                Some(serde_json::json!({"identity": "sha256:abc", "image_build_ref": "image-abc-amd64"})),
                Some(PlacementImageSource::Built { identity: "sha256:abc".into(), image_build_ref: "image-abc-amd64".into() }),
            ),
        ] {
            let spec: FulfilmentKindSpec = serde_json::from_value(kind(image)).expect("kind decodes");
            assert_eq!(spec.realisation, FulfilmentRealisation::DockerPerVessel { image: expected.clone() });
            let encoded = serde_json::to_value(&spec).expect("encode");
            assert!(encoded.get("image").is_none() == expected.is_none());
            assert_eq!(serde_json::from_value::<FulfilmentKindSpec>(encoded).expect("round-trip"), spec);
        }
        let bound = serde_json::json!({"composition": {"selection": {"base": "base"}, "layers": [], "literal_base": "crew:literal",
            "baseline_image": "crew:baseline", "identity": {"local_image_id": "sha256:local", "registry_digest": "registry.test/crew@sha256:d"}}});
        for (composition, expected) in [
            (bound.clone(), "registry.test/crew@sha256:d"),
            (
                serde_json::json!({"composition": {"selection": {}, "layers": [{"name": "base"}], "identity": {"local_image_id": "sha256:local"}}}),
                "sha256:local",
            ),
            (
                serde_json::json!({"composition": {"selection": {}, "literal_base": "crew:literal", "baseline_image": "crew:baseline"}}),
                "crew:literal",
            ),
            (serde_json::json!({"composition": {"selection": {}, "baseline_image": "crew:baseline"}}), "crew:baseline"),
        ] {
            let spec: FulfilmentKindSpec = serde_json::from_value(kind(Some(composition))).expect("old composition");
            assert_eq!(
                spec.realisation,
                FulfilmentRealisation::DockerPerVessel { image: Some(PlacementImageSource::Literal(expected.into())) }
            );
        }
    }
}
