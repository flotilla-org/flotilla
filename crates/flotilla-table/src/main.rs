//! `flotilla-table`, the table viewer beside `flotilla`; `flotilla table` hands off to it (#4249).

use clap::Parser;
use color_eyre::{eyre::eyre, Result};
use flotilla_client::sibling::{check_parent_build, PARENT_BUILD_ENV};

fn main() -> Result<()> {
    flotilla_daemon_api::build_info::initialize_build_id(env!("FLOTILLA_BUILD_ID"));
    rigging_tls::install_default_provider();
    color_eyre::install()?;
    let parent = std::env::var(PARENT_BUILD_ENV).ok();
    check_parent_build("flotilla-table", parent.as_deref(), env!("FLOTILLA_BUILD_ID")).map_err(|error| eyre!(error))?;
    let args = flotilla_table::Args::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let exit = runtime.block_on(flotilla_table::run(args));
    // The runtime shuts down before an interrupted table exits with its signal's status.
    drop(runtime);
    if let Some(status) = exit?.status() {
        std::process::exit(status);
    }
    Ok(())
}
