//! `flotilla` with no subcommand, and `flotilla view`: hand off to the flotilla-tui sibling beside
//! this binary (#4263). It parses the same global flags, so this passes its arguments unchanged.

use color_eyre::Result;
use flotilla_client::sibling::Sibling;

use super::sibling::run_sibling;

pub(crate) fn run_tui() -> Result<()> {
    run_sibling(Sibling::Tui, std::env::args_os().skip(1).collect())
}
