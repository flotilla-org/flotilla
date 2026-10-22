//! Environment: where crews and tools run, realised by one provider instance.

use flotilla_schema::{Address, Frozen, Ref, Stored};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleKind, LifecycleState},
    Resource,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use super::{image_build::ImageBuild, Holder};

/// Environments as a lifecycle kind.
pub struct EnvironmentKind;

impl LifecycleKind for EnvironmentKind {
    const NAME: &'static str = "Environment";
    type Request = EnvironmentRequest;
}

/// The Environment machine: the shared lifecycle over its request.
pub type Environment = Lifecycle<EnvironmentKind>;

/// What an Environment asks its provider for. It declares the stored
/// Environment (kind code `ev`), homed at the host whose provider realises it.
///
/// Columns are for what is queried: the provider (a provider's live
/// environments), the ImageBuild that realises its image, and its holder (a
/// vessel finds its environment by lookup, look-over ruling 4). Everything else
/// is the provider's own [`Recipe`], one frozen document, so a new provider
/// never changes this table (owner's schema look-over, 2026-10-22).
///
/// Realised under the provider's identity for it: a Docker container id, or
/// the host-direct root. The image the container runs, its configured limits
/// and its runtime samples are observations.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "Environment",
    code = "ev",
    state = LifecycleState,
    index(name = "live_by_provider", eq(provider), when(state in [Requested, Realised])),
    index(name = "by_image_build", eq(image_build), when(image_build is_some)),
    index(name = "by_holder", eq(holder), when(holder is_some))
)]
pub struct EnvironmentRequest {
    /// The provider instance: `host:<host>/provider:<instance>` or
    /// `account:<account>/provider:<instance>` (ADR 0058).
    pub provider: Address,
    /// The ImageBuild that realises the environment's image, when one does.
    /// A column of its own, outside the recipe, so that it is indexed.
    pub image_build: Option<Ref<Resource<ImageBuild>>>,
    /// The provider's recipe, stored once by content hash.
    #[store(frozen)]
    pub recipe: Frozen<Recipe>,
    /// Who it is held for: `vessel:<name>` for a crew's environment.
    pub holder: Option<Holder>,
}

/// A provider's own recipe for an environment: a document whose shape the
/// provider owns and validates, opaque to the shared table. The Docker
/// provider's is its `DockerEnvironmentSpec`, memory policy included (where
/// that policy belongs is #3734); host-direct's is its
/// `HostDirectEnvironmentSpec`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Recipe(serde_json::Value);

impl Recipe {
    /// The recipe a provider writes, from its own type.
    pub fn of<T: Serialize>(recipe: &T) -> Result<Recipe, String> {
        serde_json::to_value(recipe).map(Recipe).map_err(|error| format!("encode the provider's recipe: {error}"))
    }

    /// The recipe as its provider reads it.
    pub fn read<T: DeserializeOwned>(&self) -> Result<T, String> {
        serde_json::from_value(self.0.clone()).map_err(|error| format!("decode the provider's recipe: {error}"))
    }
}
