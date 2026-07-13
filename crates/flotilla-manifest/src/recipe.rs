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

/// A scoped-view target, mirroring ADR 0013's kind-rooted, host-free
/// addresses without depending on the (unlanded) address type itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewTarget {
    Project { namespace: String, name: String },
    Convoy { namespace: String, name: String },
    Vessel { namespace: String, convoy: String, vessel: String },
}

pub trait RecipeMint: Send + Sync {
    /// Recipe materialising a live session into a pane.
    fn session_attach(&self, attach_ref: &str) -> Option<Recipe>;
    /// Recipe materialising a scoped view of an entity with no live session.
    fn scoped_view(&self, target: &ViewTarget) -> Option<Recipe>;
}

/// v0 mint: attach recipes only. Scoped views return `None` — the entry
/// lists truthfully unmaterialisable — until #589 lands and a view-capable
/// mint replaces this one.
pub struct AttachOnlyRecipes {
    flotilla_bin: String,
}

impl AttachOnlyRecipes {
    pub fn new(flotilla_bin: impl Into<String>) -> Self {
        AttachOnlyRecipes { flotilla_bin: flotilla_bin.into() }
    }
}

impl RecipeMint for AttachOnlyRecipes {
    fn session_attach(&self, attach_ref: &str) -> Option<Recipe> {
        Some(Recipe::Command(format!("{} attach {attach_ref}", self.flotilla_bin)))
    }

    fn scoped_view(&self, _target: &ViewTarget) -> Option<Recipe> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_only_mint_formats_attach_and_declines_views() {
        let mint = AttachOnlyRecipes::new("flotilla");
        assert_eq!(mint.session_attach("implement"), Some(Recipe::Command("flotilla attach implement".to_owned())));
        assert_eq!(
            mint.scoped_view(&ViewTarget::Vessel {
                namespace: "dev".to_owned(),
                convoy: "manifest-extraction".to_owned(),
                vessel: "implement".to_owned(),
            }),
            None
        );
    }
}
