//! A standing role's running values on the engine: a plain record (a spec and no machine) named by
//! its ConvoyEnsure, homed where the ensure is admitted and written only by that home's ensure
//! pass, under the ensure's reconciliation lock. The pass folds each generation's end into it once,
//! so nothing walks or holds old generations to count strikes or time a backoff.

use flotilla_schema::{KindRegistry, RegistryError, Stored, Timestamp};

/// One standing role's strike count and backoff: kind code `sr`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Stored)]
#[store(kind = "StandingRole", code = "sr")]
pub struct StandingRoleRecord {
    /// Generations in a row that ended Failed under `strike_config`; one that ran past the reset
    /// window is the first of a new run. At the limit the role is held.
    pub strikes: u32,
    /// The configuration the strikes were counted under; a launch under another resets them.
    pub strike_config: Option<String>,
    /// When the last strike ended: the backoff runs from here.
    pub backoff_from: Option<Timestamp>,
    /// The last strike's reason.
    pub last_failure: Option<String>,
    /// The launch number of the last generation whose end is folded in.
    pub counted: Option<u64>,
}

/// Registers StandingRole into `registry` under its code, `sr`: a plain kind.
pub fn register(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    registry.register::<StandingRoleRecord>()
}

/// StandingRole alone, for its DDL snapshot.
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    register(&mut registry)?;
    Ok(registry)
}
