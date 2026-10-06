//! Compatibility for recipe-shape v1 (#2818): remove after the next fleet roll.
//! Existing viewers still execute POSIX shell strings; structured recipe minting
//! never quotes or chooses a client platform.

use flotilla_protocol::arg::shell_quote;

use crate::recipe::{LegacyRecipe, Recipe};

pub(crate) fn command(recipe: &Recipe) -> String {
    // Only the known old CLI verbs/options are literal. Genuine commands quote
    // every argument, even when they happen to contain an attach/view verb.
    let (argv, literal_end) = match &recipe.legacy {
        LegacyRecipe::Address(argv) => (argv, if recipe.kind() == "attach" { 3 } else { 2 }),
        LegacyRecipe::Checkout(argv) => (argv, 4),
        LegacyRecipe::Command(argv) => (argv, 1),
    };
    argv.iter()
        .enumerate()
        .map(|(index, argument)| if index > 0 && index < literal_end { argument.clone() } else { shell_quote(argument) })
        .collect::<Vec<_>>()
        .join(" ")
}
