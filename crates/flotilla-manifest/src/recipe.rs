//! Recipe minting — the commands a PM runs to materialise a catalog entry.
//!
//! The formatter is pluggable so v0 can ship attach-only: entities with a
//! live session get `flotilla attach <ref>`; everything else truthfully
//! lists without a recipe until `flotilla view <address>` (ADR 0013,
//! flotilla-org/flotilla#589) gives scoped views a command. The connector
//! owns the GroupPath → address mapping when that lands.

/// A materialisation recipe. Command-only for now; the Leg-1 freeze is asked
/// to bless a `{kind: command | layout}` shape (gap report §9.1) since
/// andamento's factory tabs use layout paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recipe {
    Command(String),
}

impl Recipe {
    pub fn command(&self) -> &str {
        match self {
            Recipe::Command(command) => command,
        }
    }
}

pub trait RecipeMint: Send + Sync {
    /// Recipe attaching a live entity — a session into a pane, or a vessel's
    /// running session into a workspace; `attach_ref` is any reference the
    /// daemon accepts (rows expose it as a capability fact).
    fn attach(&self, attach_ref: &str) -> Option<Recipe>;
    /// Recipe materialising a scoped view of an entity with no live session.
    fn scoped_view(&self, target: &flotilla_protocol::ViewAddress) -> Option<Recipe>;
}

/// Recipes implemented by the Flotilla CLI: attach a live entity or open a
/// scoped focal view for an awareness-band latent.
pub struct FlotillaRecipes {
    flotilla_bin: String,
}

impl FlotillaRecipes {
    pub fn new(flotilla_bin: impl Into<String>) -> Self {
        Self { flotilla_bin: flotilla_bin.into() }
    }
}

impl RecipeMint for FlotillaRecipes {
    fn attach(&self, attach_ref: &str) -> Option<Recipe> {
        Some(Recipe::Command(format!("{} attach {attach_ref}", self.flotilla_bin)))
    }

    fn scoped_view(&self, target: &flotilla_protocol::ViewAddress) -> Option<Recipe> {
        Some(Recipe::Command(format!("{} view {}", self.flotilla_bin, flotilla_protocol::arg::shell_quote(&target.to_string()))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flotilla_mint_formats_attach_and_scoped_view_recipes() {
        let mint = FlotillaRecipes::new("flotilla");
        assert_eq!(mint.attach("implement"), Some(Recipe::Command("flotilla attach implement".to_owned())));
        assert_eq!(
            mint.scoped_view(&flotilla_protocol::ViewAddress::Vessel {
                namespace: "dev".to_owned(),
                convoy: "manifest-extraction".to_owned(),
                vessel: "implement".to_owned(),
            }),
            Some(Recipe::Command("flotilla view 'vessel/dev/manifest-extraction/implement'".to_owned()))
        );
    }
}
