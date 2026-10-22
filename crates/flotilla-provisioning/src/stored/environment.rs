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
/// provider's is a [`DockerRecipe`] and host-direct's a [`HostDirectRecipe`].
/// A stored recipe is a stored document: renaming or retiring one of its
/// fields takes the same one-generation alias or default shim as any stored
/// shape (ADR 0047), and its content hash must stay stable.
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

/// The Docker provider's recipe: the document a Docker environment's
/// [`Recipe`] holds. Declared beside the request because the provider's own
/// spec (`flotilla_resources::DockerEnvironmentSpec`) is the old store's,
/// with legacy host fields and an image placeholder this shape leaves out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerRecipe {
    pub image: DockerImage,
    /// Adapters the placement policy expects discovery to find in the image.
    pub declared_adapters: std::collections::BTreeSet<String>,
    /// Adapters this environment's workflow will launch.
    pub required_adapters: std::collections::BTreeSet<String>,
    pub pull_policy: flotilla_resources::DockerImagePullPolicy,
    /// The placement policy's memory formula, resolved against host memory at
    /// container creation (where it belongs is #3734).
    pub memory_policy: flotilla_resources::EnvironmentMemoryPolicy,
    pub mounts: Vec<flotilla_resources::EnvironmentMount>,
    /// The values the placement policy and workflow declare, and nothing
    /// derived at launch: credential grants and skills are composed then and
    /// passed to the provider, never stored ([`LAUNCH_DERIVED_ENV`]).
    pub env: std::collections::BTreeMap<String, String>,
}

/// A Docker environment's image: pinned, or built (owner's look-over ruling
/// 6). A built image names no reference here: its ImageBuild is the
/// request's indexed `image_build` column, and the digest used is the build's
/// realised identity, read at container creation and never copied in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DockerImage {
    Pinned { reference: String },
    Built,
}

/// The host-direct provider's recipe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDirectRecipe {
    pub repo_default_dir: String,
}

/// Env entries derived at launch, which no stored recipe holds (owner's
/// look-over ruling 5): credential names, scopes and permission maps, and
/// the crew's skills.
pub const LAUNCH_DERIVED_ENV: [&str; 5] = [
    flotilla_resources::CREDENTIAL_REFS_ENV,
    flotilla_resources::CREDENTIAL_SCOPES_ENV,
    flotilla_resources::CREDENTIAL_PERMISSIONS_ENV,
    "FLOTILLA_CREW_SKILLS",
    "FLOTILLA_RESOLVED_SKILLS",
];

impl DockerRecipe {
    /// The recipe's own rules beside the request's `image_build` column: a
    /// built image has its build there and a pinned one has none, a pinned
    /// reference is not empty, and the env holds nothing derived at launch.
    pub fn validate(&self, image_build: Option<&Ref<Resource<ImageBuild>>>) -> Result<(), String> {
        match (&self.image, image_build) {
            (DockerImage::Built, None) => return Err("a built image names its ImageBuild in the request".into()),
            (DockerImage::Pinned { .. }, Some(_)) => return Err("a pinned image names no ImageBuild".into()),
            (DockerImage::Pinned { reference }, None) if reference.trim().is_empty() => {
                return Err("a pinned image names its reference".into())
            }
            _ => {}
        }
        match LAUNCH_DERIVED_ENV.iter().find(|key| self.env.contains_key(**key)) {
            Some(key) => Err(format!("{key} is composed at launch, never stored")),
            None => Ok(()),
        }
    }
}
