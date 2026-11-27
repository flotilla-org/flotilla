//! Platform-independent action addresses and whether this viewer can bind Tender publications.
//! Viewers resolve addresses on their own platform; only genuine commands carry argv.

use flotilla_protocol::{HostName, ViewAddress};

/// A structured action recipe. Kind is an open vocabulary; argv is present
/// only for genuine commands. The target is also the viewer's focus-if-live key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    shape: RecipeShape,
    // Compatibility for recipe-shape v1 (#2818): remove after the next fleet roll.
    legacy: LegacyRecipe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecipeShape {
    Address { kind: String, target: String },
    Command { target: String, argv: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LegacyRecipe {
    Attach(Vec<String>),
    View(Vec<String>),
    Command(Vec<String>),
}

impl Recipe {
    pub fn kind(&self) -> &str {
        match &self.shape {
            RecipeShape::Address { kind, .. } => kind,
            RecipeShape::Command { .. } => "command",
        }
    }

    pub fn target(&self) -> &str {
        match &self.shape {
            RecipeShape::Address { target, .. } | RecipeShape::Command { target, .. } => target,
        }
    }

    pub fn argv(&self) -> Option<&[String]> {
        match &self.shape {
            RecipeShape::Address { .. } => None,
            RecipeShape::Command { argv, .. } => Some(argv),
        }
    }

    pub fn command(target: impl Into<String>, argv: Vec<String>) -> Self {
        Self { legacy: LegacyRecipe::Command(argv.clone()), shape: RecipeShape::Command { target: target.into(), argv } }
    }

    fn address(kind: &str, target: String, legacy: LegacyRecipe) -> Self {
        Self { shape: RecipeShape::Address { kind: kind.to_owned(), target }, legacy }
    }

    pub(crate) fn legacy(&self) -> &LegacyRecipe {
        &self.legacy
    }
}

/// Why a viewer that never looked for its Tender instance has none.
const NO_TENDER: &str = "no Tender instance reachable from this viewer";

pub trait RecipeMint: Send + Sync {
    /// Whether this viewer's own Tender instance answers: binding a publication needs it.
    fn tender(&self) -> Result<(), String> {
        Err(NO_TENDER.to_owned())
    }
    /// Recipe attaching a live entity — a session into a pane, or a vessel's
    /// running session into a workspace; `attach_ref` is any reference the
    /// daemon accepts (rows expose it as a capability fact).
    fn attach(&self, attach_ref: &str, host: &HostName) -> Option<Recipe>;
    /// Recipe materialising a scoped view of an entity with no live session.
    fn scoped_view(&self, target: &flotilla_protocol::ViewAddress) -> Option<Recipe>;
}

/// Recipes implemented by the Flotilla CLI: attach a live entity or open a
/// scoped focal view for an awareness-band latent.
pub struct FlotillaRecipes {
    flotilla_bin: String,
    tender: Result<(), String>,
}

impl FlotillaRecipes {
    pub fn new(flotilla_bin: impl Into<String>) -> Self {
        Self { flotilla_bin: flotilla_bin.into(), tender: Err(NO_TENDER.to_owned()) }
    }

    /// What the viewer found of its own Tender instance.
    pub fn with_tender(mut self, tender: Result<(), String>) -> Self {
        self.tender = tender;
        self
    }
}

impl RecipeMint for FlotillaRecipes {
    fn tender(&self) -> Result<(), String> {
        self.tender.clone()
    }

    fn attach(&self, attach_ref: &str, host: &HostName) -> Option<Recipe> {
        let target = host_target("session", host, attach_ref)?;
        Some(Recipe::address(
            "attach",
            target,
            LegacyRecipe::Attach(vec![
                self.flotilla_bin.clone(),
                "attach".to_owned(),
                "--host".to_owned(),
                host.to_string(),
                attach_ref.to_owned(),
            ]),
        ))
    }

    fn scoped_view(&self, target: &ViewAddress) -> Option<Recipe> {
        Some(Recipe::address(
            "view",
            format!("view:{target}"),
            LegacyRecipe::View(vec![self.flotilla_bin.clone(), "view".to_owned(), target.to_string()]),
        ))
    }
}

/// The first slash delimits the host; refs and paths keep their remaining slashes.
fn host_target(kind: &str, host: &HostName, reference: &str) -> Option<String> {
    if host.as_str().contains('/') {
        return None;
    }
    Some(format!("{kind}:{host}/{reference}"))
}
