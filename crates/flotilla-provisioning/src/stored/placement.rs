//! FulfilmentKind on the engine. The settings a policy carried are code constants in
//! [`flotilla_resources::realisation_settings`], and why a row was written belongs to its change.

use std::collections::BTreeSet;

use flotilla_resources::{DockerImageSource, FulfilmentCostClass, FulfilmentGrant, FulfilmentKindSpec, FulfilmentRealisation};
use flotilla_schema::{KindRegistry, RegistryError, Stored};
use serde::{Deserialize, Serialize};

/// How a kind realises its vessels, with the image it runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KindRealisation {
    CloudSandboxPerVessel {
        sandbox_image: String,
    },
    /// The vessel's composed image, frozen per vessel at admission.
    DockerComposed,
    DockerLiteral {
        image: String,
    },
    DockerBuilt {
        identity: String,
        image_build_ref: String,
    },
    HostDirect,
}

#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "FulfilmentKind", code = "fk")]
pub struct FulfilmentKindDefinition {
    /// Empty for a kind bound to no host, such as a cloud sandbox.
    pub host_ref: String,
    pub pool: String,
    pub cost_class: FulfilmentCostClass,
    #[store(child)]
    pub grants: BTreeSet<String>,
    /// One document: admission picks a kind in memory, so no read selects by it.
    #[store(cbor)]
    pub realisation: KindRealisation,
}

impl FulfilmentKindDefinition {
    pub fn new(spec: &FulfilmentKindSpec) -> Result<Self, String> {
        let realisation = match &spec.realisation {
            FulfilmentRealisation::CloudSandboxPerVessel { image } => {
                KindRealisation::CloudSandboxPerVessel { sandbox_image: image.clone() }
            }
            FulfilmentRealisation::HostDirect => KindRealisation::HostDirect,
            FulfilmentRealisation::DockerPerVessel { image } => match image {
                None => KindRealisation::DockerComposed,
                Some(DockerImageSource::Literal(image)) => KindRealisation::DockerLiteral { image: image.clone() },
                Some(DockerImageSource::Built { identity, image_build_ref }) => {
                    KindRealisation::DockerBuilt { identity: identity.clone(), image_build_ref: image_build_ref.clone() }
                }
            },
        };
        Ok(Self {
            host_ref: spec.host_ref.clone(),
            pool: spec.pool.clone(),
            cost_class: spec.cost_class,
            grants: spec.grants.iter().map(|grant| grant.0.clone()).collect(),
            realisation,
        })
    }

    pub fn spec(&self) -> FulfilmentKindSpec {
        let realisation = match &self.realisation {
            KindRealisation::CloudSandboxPerVessel { sandbox_image } => {
                FulfilmentRealisation::CloudSandboxPerVessel { image: sandbox_image.clone() }
            }
            KindRealisation::HostDirect => FulfilmentRealisation::HostDirect,
            KindRealisation::DockerComposed => FulfilmentRealisation::DockerPerVessel { image: None },
            KindRealisation::DockerLiteral { image } => {
                FulfilmentRealisation::DockerPerVessel { image: Some(DockerImageSource::Literal(image.clone())) }
            }
            KindRealisation::DockerBuilt { identity, image_build_ref } => FulfilmentRealisation::DockerPerVessel {
                image: Some(DockerImageSource::Built { identity: identity.clone(), image_build_ref: image_build_ref.clone() }),
            },
        };
        FulfilmentKindSpec {
            host_ref: self.host_ref.clone(),
            pool: self.pool.clone(),
            cost_class: self.cost_class,
            grants: self.grants.iter().cloned().map(FulfilmentGrant).collect(),
            realisation,
        }
    }
}

pub fn register(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    registry.register::<FulfilmentKindDefinition>()?;
    Ok(())
}

pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    register(&mut registry)?;
    Ok(registry)
}
