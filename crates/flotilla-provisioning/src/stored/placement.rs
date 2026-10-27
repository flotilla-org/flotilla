//! Placement's definition on the engine (#3824 layer 4a): FulfilmentKind, a plain definition kind
//! written by its home's single writer.
//!
//! The 2026-10-26 rulings retire PlacementPolicy by finishing ADR 0046 (admission selects kinds by
//! grants and cost). A FulfilmentKind keeps host, pool, grants, cost class and the realisation with
//! its image; the provider is named by the realisation alone. The settings only a policy carried
//! are code constants ([`flotilla_resources::realisation_settings`]; the 2026-10-27 ruling), and the
//! row holds no provenance: why it was written is its change's (#4024).
//!
//! Nothing reads or writes this table yet: the import rehearsal restores into it, and it joins the
//! daemon's decided store with admission and realisation (layer 4b).

use std::collections::BTreeSet;

use flotilla_resources::{DockerImageSource, FulfilmentCostClass, FulfilmentGrant, FulfilmentKindSpec, FulfilmentRealisation};
use flotilla_schema::{KindRegistry, RegistryError, Stored};

/// How a kind realises its vessels, with the image it runs.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub enum KindRealisation {
    /// A cloud sandbox running `sandbox_image`.
    #[store(code = "cs")]
    CloudSandboxPerVessel { sandbox_image: String },
    /// A container per vessel running its composed image (ADR 0062), frozen per vessel at
    /// admission.
    #[store(code = "dk")]
    DockerComposed,
    /// A container per vessel running a literal image reference.
    #[store(code = "dl")]
    DockerLiteral { image: String },
    /// A container per vessel running a composed identity, as a snapshot froze it, with the
    /// ImageBuild that realises it.
    #[store(code = "db")]
    DockerBuilt { identity: String, image_build_ref: String },
    /// The host itself.
    #[store(code = "hd")]
    HostDirect,
}

/// A FulfilmentKind, as the engine stores it: kind code `fk`.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "FulfilmentKind", code = "fk")]
pub struct FulfilmentKindDefinition {
    /// The host that realises it; empty for a kind bound to no host (a cloud sandbox).
    pub host_ref: String,
    pub pool: String,
    pub cost_class: FulfilmentCostClass,
    /// What a vessel placed by this kind is granted; admission selects by them.
    #[store(child)]
    pub grants: BTreeSet<String>,
    pub realisation: KindRealisation,
}

impl FulfilmentKindDefinition {
    /// The record of `spec`. A kind naming a CrewImageBaseline is refused: the kind retires.
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
                Some(DockerImageSource::Baseline { image_baseline_ref }) => {
                    return Err(format!("its image names CrewImageBaseline `{image_baseline_ref}`, which retires"));
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

/// Registers FulfilmentKind `fk` into `registry`, a plain kind.
pub fn register(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    registry.register::<FulfilmentKindDefinition>()?;
    Ok(())
}

/// Placement's kind alone, for their DDL snapshot and the import rehearsal.
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    register(&mut registry)?;
    Ok(registry)
}
