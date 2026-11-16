//! What each kind's engine port takes and keeps: the in-memory port serves a kind as its real port
//! does, and the port contract holds both to it.

/// What a kind's engine port takes and keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// Whether the kind's records are written and deleted through the port; otherwise the engine
    /// holds them as its machines run, and the port refuses both.
    pub writes: bool,
    /// Whether the port keeps a status written to it; otherwise it refuses the write.
    pub status: bool,
    /// Whether the labels written with a record read back; otherwise they are dropped.
    pub labels: bool,
    /// Whether the annotations written with a record read back; otherwise they are dropped.
    pub annotations: bool,
    /// Whether a status write moves the record's version. Writing the same spec again never does.
    pub status_moves_version: bool,
}

/// A kind written as a whole spec, with no status, labels or annotations.
const SPEC_ONLY: Profile = Profile { writes: true, status: false, labels: false, annotations: false, status_moves_version: false };

/// A kind written as a spec, with a status kept in the same record: writing it moves the version.
const SPEC_WITH_STATUS: Profile = Profile { status: true, status_moves_version: true, ..SPEC_ONLY };

/// A kind the engine holds as its machines run: read through the port, never written to it.
const HELD: Profile = Profile { writes: false, ..SPEC_ONLY };

/// The profile of `kind`'s engine port. Each is checked against the real port by the port contract
/// in `flotilla-provisioning`'s `kind_records_contract` tests; a kind is added here with its check.
pub fn profile_of(kind: &str) -> Profile {
    match kind {
        "Artifact" => SPEC_ONLY,
        "Repository" => SPEC_WITH_STATUS,
        "Environment" => HELD,
        other => panic!("{other} has no in-memory port profile; check one against its real port first"),
    }
}
