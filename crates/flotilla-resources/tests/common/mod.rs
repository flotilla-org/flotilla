use flotilla_resources::{ApiPaths, InputMeta, Resource};
use serde::{Deserialize, Serialize};

pub struct ConvoyResource;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoySpec {
    pub template: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoyStatus {
    pub phase: String,
}

impl Resource for ConvoyResource {
    type Spec = ConvoySpec;
    type Status = ConvoyStatus;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.io", version: "v1", plural: "convoys", kind: "Convoy" };
}

pub fn input_meta(name: &str) -> InputMeta {
    InputMeta {
        name: name.to_string(),
        labels: [("app".to_string(), "flotilla".to_string())].into_iter().collect(),
        annotations: [("note".to_string(), "test".to_string())].into_iter().collect(),
    }
}

pub fn spec(template: &str) -> ConvoySpec {
    ConvoySpec { template: template.to_string() }
}

pub fn status(phase: &str) -> ConvoyStatus {
    ConvoyStatus { phase: phase.to_string() }
}
