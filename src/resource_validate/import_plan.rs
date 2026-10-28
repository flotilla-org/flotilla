//! The importer's plan for every old-store kind (#3824 layer 0). Each kind the old store has held
//! is imported by a named rehearsal, retired by a ruling (its rows dropped), or still moving in a
//! later layer. A kind with no entry fails closed, so a new old-store kind cannot be left behind
//! silently. Each layer of #3824 moves its kinds from [`Plan::Moving`] to [`Plan::Imported`].

use std::collections::BTreeMap;

use flotilla_store::{retired_resource_kind, REGISTERED_RESOURCE_KINDS};

/// What the cut-over import does with one old-store kind's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Plan {
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
pub(super) const PLAN: &[(&str, Plan)] = &[
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
    ("FulfilmentKind", Plan::Imported("placement import (#3824 layer 4a)")),
    ("Host", Plan::Moving("#3824 layer 4c")),
    // Each policy becomes a FulfilmentKind of its name.
    ("PlacementPolicy", Plan::Imported("placement import (#3824 layer 4a)")),
    ("ChangeRequest", Plan::Moving("#3824 layer 5")),
    ("Issue", Plan::Moving("#3824 layer 5")),
    ("ForgeRead", Plan::Moving("#3824 layer 5")),
    ("Artifact", Plan::Imported("Artifact import (#3824 layer 7)")),
    ("Demand", Plan::Moving("#3824 layer 8")),
    ("Regard", Plan::Moving("#3938")),
    // A plain record beside AccountProvision; its type already derives `Stored` (#3964).
    ("AccessDelivery", Plan::Moving("#3824, plain like AccountProvision")),
];

/// The plan for the kind listed under `plural`, if it has one.
pub(super) fn plan(plural: &str) -> Option<(&'static str, Plan)> {
    let kind = REGISTERED_RESOURCE_KINDS
        .iter()
        .find(|entry| entry.plural == plural)
        .map(|entry| entry.kind)
        .or_else(|| retired_resource_kind(plural).map(|retired| retired.kind))?;
    PLAN.iter().find(|(name, _)| *name == kind).copied()
}

/// Whether `plural` is a retired kind this candidate no longer decodes; the import drops its rows.
pub(super) fn is_retired(plural: &str) -> bool {
    !REGISTERED_RESOURCE_KINDS.iter().any(|entry| entry.plural == plural) && matches!(plan(plural), Some((_, Plan::Retired(_))))
}

/// Row counts of one namespace's listed kinds, by plural, and what the plan makes of them.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Report {
    pub dropped: BTreeMap<&'static str, usize>,
    pub moving: BTreeMap<&'static str, (&'static str, usize)>,
    /// Plurals listed by the daemon with rows and no plan.
    pub unplanned: BTreeMap<String, usize>,
}

impl Report {
    /// Counts `rows` rows listed under `plural`. Imported kinds are their rehearsals' business.
    pub(super) fn count(&mut self, plural: &str, rows: usize) {
        if rows == 0 {
            return;
        }
        match plan(plural) {
            Some((_, Plan::Imported(_))) => {}
            Some((kind, Plan::Retired(_))) => *self.dropped.entry(kind).or_default() += rows,
            Some((kind, Plan::Moving(layer))) => self.moving.entry(kind).or_insert((layer, 0)).1 += rows,
            None => *self.unplanned.entry(plural.to_owned()).or_default() += rows,
        }
    }

    /// Whether the import has no plan for rows the daemon holds. Dropped rows never block it, and
    /// rows of a kind still moving are reported, not refused: until its layer lands the kind stays
    /// on the old store under `[primitives]` too, and a host running the switch must still roll.
    pub(super) fn blocks_import(&self) -> bool {
        !self.unplanned.is_empty()
    }
}

/// One line each for dropped, waiting and unplanned rows.
pub(super) fn print(namespace: &str, report: &Report) {
    let join = |items: Vec<String>| items.join(", ");
    if !report.dropped.is_empty() {
        let counts = join(report.dropped.iter().map(|(kind, rows)| format!("{kind} {rows}")).collect());
        println!("import plan {namespace}: retired, dropped: {counts}");
    }
    if !report.moving.is_empty() {
        let counts = join(report.moving.iter().map(|(kind, (layer, rows))| format!("{kind} {rows} ({layer})")).collect());
        println!("import plan {namespace}: not imported yet, stays on the old store: {counts}");
    }
    if !report.unplanned.is_empty() {
        let counts = join(report.unplanned.iter().map(|(plural, rows)| format!("{plural} {rows}")).collect());
        println!("import plan {namespace}: no import plan: {counts}");
    }
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

    // Behaviour (#3824 layer 0, #3760 gate): retired rows are dropped, including those of a kind the
    // store no longer registers (#4007), and rows of a kind still moving are counted, and neither
    // blocks the import; rows of a kind with no plan block it;
    // imported kinds are left to their rehearsals; an empty listing counts nothing.
    #[test]
    fn only_rows_with_no_plan_block_the_import() {
        let mut report = Report::default();
        report.count("dispatchholds", 3);
        report.count("events", 2);
        report.count("usages", 2);
        report.count("demands", 0);
        report.count("messages", 9);
        assert_eq!(report.dropped, BTreeMap::from([("DispatchHold", 3), ("Event", 2), ("Usage", 2)]));
        assert!(!report.blocks_import(), "dropped and imported rows never block the import");
        report.count("demands", 1);
        assert_eq!(report.moving, BTreeMap::from([("Demand", ("#3824 layer 8", 1))]));
        assert!(!report.blocks_import(), "a kind still moving stays on the old store until its layer");
        let mut unknown = Report::default();
        unknown.count("widgets", 1);
        assert!(unknown.blocks_import(), "a kind with no plan fails closed");
    }

    #[test]
    fn a_retired_unregistered_kind_listed_by_an_older_daemon_is_dropped() {
        assert!(is_retired("crewimagebaselines"));
        assert!(!is_retired("events"), "a retired kind still registered keeps its decoder");
        let mut report = Report::default();
        report.count("crewimagebaselines", 2);
        assert_eq!(report.dropped.get("CrewImageBaseline"), Some(&2));
        assert!(!report.blocks_import());
    }
}
