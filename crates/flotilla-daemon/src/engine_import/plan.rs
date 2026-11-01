//! The importer's plan for every old-store kind (#3824 layer 0). Each kind the old store has held
//! is imported by a named rehearsal, retired by a ruling (its rows dropped), or still moving in a
//! later layer. A kind with no entry fails closed, so a new old-store kind cannot be left behind
//! silently. Each layer of #3824 moves its kinds from [`Plan::Moving`] to [`Plan::Imported`].

use flotilla_store::{retired_resource_kind, REGISTERED_RESOURCE_KINDS};

/// What the cut-over import does with one old-store kind's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Mapped onto the engine by the named importer.
    Imported(&'static str),
    /// Retired by the named ruling; the import drops its rows.
    Retired(&'static str),
    /// Moves to the engine in the named layer or ticket; not imported yet.
    Moving(&'static str),
}

/// Every kind the old store has held, by kind name, with its plan. A retired kind the store no
/// longer registers is still listed by a previous-generation daemon; while it is in
/// `RETIRED_RESOURCE_KINDS` (one generation, ADR 0047) its rows are counted as dropped.
pub const PLAN: &[(&str, Plan)] = &[
    ("Message", Plan::Imported("Message import (#3317)")),
    ("Environment", Plan::Imported("lifecycle import (#3629)")),
    ("ImageBuild", Plan::Imported("lifecycle import (#3629)")),
    ("Clone", Plan::Imported("lifecycle import (#3629)")),
    ("Checkout", Plan::Imported("lifecycle import (#3629)")),
    ("TerminalSession", Plan::Imported("lifecycle import (#3629)")),
    ("Vessel", Plan::Imported("lifecycle import (#3629)")),
    ("ConvoyEnsure", Plan::Imported("lifecycle import (#3629)")),
    ("Convoy", Plan::Imported("Convoy import (#3587)")),
    ("AccountBinding", Plan::Imported("account import (#3824 layer 1)")),
    ("AccountProvision", Plan::Imported("account import (#3824 layer 1)")),
    ("Event", Plan::Retired("#3212: typed columns, records and edges replace it")),
    ("MaterialPool", Plan::Retired("#3824 summary, 2026-10-26")),
    ("CrewImageBaseline", Plan::Retired("#3824 summary, 2026-10-26")),
    ("ForgeReadHeartbeat", Plan::Retired("#3824 summary, 2026-10-26")),
    ("Usage", Plan::Retired("#3824, 2026-10-26: nothing writes it since #3402")),
    ("DispatchObservation", Plan::Retired("#3824, 2026-10-26: telemetry, not state (#3936)")),
    ("DispatchHold", Plan::Retired("#3824, 2026-10-26: ordering is blocked-by edges in the tracker")),
    ("DispatchDeployment", Plan::Retired("#3824, 2026-10-26: ordering is blocked-by edges in the tracker")),
    ("CredentialGrant", Plan::Imported("definitions import (#3824 layer 2b)")),
    ("CredentialSpec", Plan::Imported("definitions import (#3824 layer 2b)")),
    ("CrewDefaults", Plan::Imported("definitions import (#3824 layer 2b)")),
    ("FleetDesignation", Plan::Imported("definitions import (#3824 layer 2b)")),
    ("Forge", Plan::Imported("definitions import (#3824 layer 2b)")),
    ("ImageLayer", Plan::Imported("definitions import (#3824 layer 2b)")),
    ("ManifestRoot", Plan::Imported("ManifestRoot import (#3824 layer 2c)")),
    ("WorkflowTemplate", Plan::Imported("definitions import (#3824 layer 2b)")),
    ("Project", Plan::Imported("definitions import (#3824 layer 3b)")),
    ("Repository", Plan::Imported("Repository import (#3824 layer 3a)")),
    // Mapped by the placement import (#3824 layer 4a), but the engine store holds no FulfilmentKind until
    // a layer serves it from there.
    ("FulfilmentKind", Plan::Moving("the layer that serves FulfilmentKind from the engine")),
    ("Host", Plan::Imported("hosts")),
    // Each policy becomes a FulfilmentKind of its name.
    ("PlacementPolicy", Plan::Moving("the layer that serves FulfilmentKind from the engine")),
    ("ChangeRequest", Plan::Imported("forge observations import (#3824 layer 5)")),
    ("Issue", Plan::Imported("forge observations import (#3824 layer 5)")),
    ("ForgeRead", Plan::Imported("forge observations import (#3824 layer 5)")),
    ("Artifact", Plan::Imported("Artifact import (#3824 layer 7)")),
    ("Demand", Plan::Moving("#3824 layer 8")),
    ("Regard", Plan::Moving("#3938")),
    // A plain record beside AccountProvision; its type already derives `Stored` (#3964).
    ("AccessDelivery", Plan::Moving("#3824, plain like AccountProvision")),
];

/// The plan for the kind listed under `plural`, if it has one.
pub fn plan(plural: &str) -> Option<(&'static str, Plan)> {
    let kind = REGISTERED_RESOURCE_KINDS
        .iter()
        .find(|entry| entry.plural == plural)
        .map(|entry| entry.kind)
        .or_else(|| retired_resource_kind(plural).map(|retired| retired.kind))?;
    PLAN.iter().find(|(name, _)| *name == kind).copied()
}

/// Whether `plural` is a retired kind this candidate no longer decodes; the import drops its rows.
pub fn is_retired(plural: &str) -> bool {
    !REGISTERED_RESOURCE_KINDS.iter().any(|entry| entry.plural == plural) && matches!(plan(plural), Some((_, Plan::Retired(_))))
}

#[cfg(test)]
mod tests {
    use flotilla_store::RETIRED_RESOURCE_KINDS;

    use super::*;

    // Contract (#3824 layer 0): the plan is total over the old store's registry, and names each
    // kind once, so no registered kind is left out of the import silently.
    #[test]
    fn every_registered_kind_has_one_plan() {
        for entry in REGISTERED_RESOURCE_KINDS {
            assert_eq!(PLAN.iter().filter(|(kind, _)| *kind == entry.kind).count(), 1, "{} needs one import plan", entry.kind);
        }
        let mut kinds: Vec<_> = PLAN.iter().map(|(kind, _)| *kind).collect();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), PLAN.len(), "a kind is planned twice");
        for retired in RETIRED_RESOURCE_KINDS {
            assert!(matches!(plan(retired.plural), Some((_, Plan::Retired(_)))), "{} keeps a drop decoder, so it is retired", retired.kind);
        }
        for (kind, plan) in PLAN {
            if !REGISTERED_RESOURCE_KINDS.iter().any(|entry| entry.kind == *kind) {
                assert!(matches!(plan, Plan::Retired(_)), "{kind} is not registered, so only a retired kind may be");
            }
        }
    }

    #[test]
    fn a_retired_unregistered_kind_listed_by_an_older_daemon_is_dropped() {
        assert!(is_retired("crewimagebaselines"));
        assert!(!is_retired("events"), "a retired kind still registered keeps its decoder");
    }
}
