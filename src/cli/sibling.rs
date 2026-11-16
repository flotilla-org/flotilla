//! Handing a command to a sibling binary beside this one (#4249).

use std::{ffi::OsString, sync::OnceLock};

use color_eyre::{eyre::eyre, Result};
use flotilla_client::{
    build_id,
    sibling::{hand_off, Sibling},
};

/// The status a sibling that ran as a child (not exec'd: Windows) exited with, for this process
/// to exit with once the runtime has shut down.
static EXIT_STATUS: OnceLock<i32> = OnceLock::new();

pub(crate) fn handoff_exit_status() -> Option<i32> {
    EXIT_STATUS.get().copied()
}

pub(super) fn record_exit_status(status: i32) {
    let _ = EXIT_STATUS.set(status);
}

/// Runs `sibling` with `args` in place of this command; on Unix it never returns on success.
pub(super) fn run_sibling(sibling: Sibling, args: Vec<OsString>) -> Result<()> {
    record_exit_status(hand_off(sibling, args, build_id()).map_err(|error| eyre!(error))?);
    Ok(())
}
