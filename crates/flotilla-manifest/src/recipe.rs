//! Platform-independent action addresses and whether this viewer can bind Tender publications.
//! Viewers resolve addresses on their own platform; only genuine commands carry argv.

use std::sync::{Arc, RwLock};

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

/// The `tender/<fingerprint>` scopes of Tender instances' own publications.
const INSTANCE_SCOPE_PREFIX: &str = "tender/";

/// Where a publication is looked for among the viewer's Bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingScope<'a> {
    Exact(&'a str),
    /// Some host instance's own scope, which only its Tender knows.
    AnyInstance,
}

/// One of this viewer's Bindings, and why its Tender cannot reach the publication when it cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerBinding {
    pub scope: String,
    pub name: String,
    pub reachable: Result<(), String>,
}

/// This viewer's Bindings as its Tender instance last listed them, or why it could not list them,
/// shared between a mint and whatever lists them.
#[derive(Debug, Clone)]
pub struct ViewerBindings(Arc<RwLock<Result<Vec<ViewerBinding>, String>>>);

impl Default for ViewerBindings {
    fn default() -> Self {
        Self::new(Err(NO_TENDER.to_owned()))
    }
}

impl ViewerBindings {
    pub fn new(listing: Result<Vec<ViewerBinding>, String>) -> Self {
        Self(Arc::new(RwLock::new(listing)))
    }

    pub fn set(&self, listing: Result<Vec<ViewerBinding>, String>) {
        *self.0.write().expect("viewer bindings lock") = listing;
    }

    /// The scope of the Binding of `name` in `scope` this viewer's Tender reaches, or why there is
    /// none to bind.
    pub fn reach(&self, scope: BindingScope<'_>, name: &str) -> Result<String, String> {
        let listing = self.0.read().expect("viewer bindings lock");
        let bindings = listing.as_ref().map_err(Clone::clone)?;
        let mut found = bindings.iter().filter(|binding| {
            binding.name == name
                && match scope {
                    BindingScope::Exact(scope) => binding.scope == scope,
                    BindingScope::AnyInstance => binding.scope.starts_with(INSTANCE_SCOPE_PREFIX),
                }
        });
        let wanted = match scope {
            BindingScope::Exact(scope) => format!("{scope}:{name}"),
            BindingScope::AnyInstance => format!("{name} in a host's own scope"),
        };
        let binding = found.next().ok_or_else(|| {
            format!("this viewer's Tender instance has no Binding of {wanted}; pair it with the publishing host's Tender instance")
        })?;
        if found.next().is_some() {
            return Err(format!("this viewer's Tender instance has Bindings of {wanted} from more than one host"));
        }
        binding.reachable.clone().map_err(|why| format!("this viewer's Tender instance cannot reach {}:{name}: {why}", binding.scope))?;
        Ok(binding.scope.clone())
    }
}

pub trait RecipeMint: Send + Sync {
    /// The scope of this viewer's Binding of publication `name`, once its own Tender instance
    /// reaches the publication, or why it cannot be bound.
    fn binding(&self, _scope: BindingScope<'_>, _name: &str) -> Result<String, String> {
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
    bindings: ViewerBindings,
}

impl FlotillaRecipes {
    pub fn new(flotilla_bin: impl Into<String>) -> Self {
        Self { flotilla_bin: flotilla_bin.into(), bindings: ViewerBindings::default() }
    }

    /// The viewer's Bindings, as its Tender instance lists them.
    pub fn with_bindings(mut self, bindings: ViewerBindings) -> Self {
        self.bindings = bindings;
        self
    }
}

impl RecipeMint for FlotillaRecipes {
    fn binding(&self, scope: BindingScope<'_>, name: &str) -> Result<String, String> {
        self.bindings.reach(scope, name)
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
