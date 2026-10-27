//! Vessels held against an SSH host's declared cap (#3793).
//!
//! The owning daemon is the only placer on its SSH hosts, so admission,
//! the published `free_vessel_slots` fact and a convoy's capacity wait all
//! count from the same records. A vessel holds its slot from its convoy's
//! admission until its Vessel record is gone, which the teardown finalizer
//! allows only once teardown has finished: a crash mid-teardown keeps the slot.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use flotilla_protocol::PlacementDecision;

use crate::{
    Convoy, ReadResourceObject, Resource, ResourceObject, ResourceProvenance, Vessel, VesselPlacementPin, VESSEL_PLACEMENTS_ANNOTATION,
};

/// The vessels one host holds, counted from the convoys and vessels that name it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostOccupancy {
    /// Vessel records placed on the host, including any still tearing down.
    pub placed: u32,
    /// Live convoys with vessels on the host that do not exist yet, oldest
    /// first, with the number of those vessels each holds.
    pub admitted: Vec<(String, u32)>,
}

impl HostOccupancy {
    /// Count what `host` (a canonical Host name) holds. The first record of
    /// each name counts, so callers list each record's home copy first
    /// ([`home_copies_first`]). `workflow_vessels` names the frozen workflow's
    /// vessels for each convoy whose status has no workflow snapshot yet
    /// ([`HostOccupancy::needs_workflow_vessels`]).
    pub fn count<'a>(
        host: &str,
        convoys: impl IntoIterator<Item = &'a ResourceObject<Convoy>>,
        vessels: impl IntoIterator<Item = &'a ResourceObject<Vessel>>,
        workflow_vessels: &BTreeMap<String, Vec<String>>,
    ) -> Self {
        let mut convoys_by_name = BTreeMap::new();
        for convoy in convoys {
            convoys_by_name.entry(convoy.metadata.name.as_str()).or_insert(convoy);
        }
        let mut seen = BTreeSet::new();
        let vessels = vessels.into_iter().filter(|vessel| seen.insert(vessel.metadata.name.as_str())).collect::<Vec<_>>();
        let pins = convoys_by_name.iter().map(|(name, convoy)| (*name, placement_pins(convoy))).collect::<BTreeMap<_, _>>();
        let mut created = BTreeMap::<&str, BTreeSet<&str>>::new();
        let mut placed = 0;
        for vessel in &vessels {
            created.entry(vessel.spec.convoy_ref.as_str()).or_default().insert(vessel.spec.vessel_name.as_str());
            // The vessel reconciler's own lookup: the vessel's pin, else the
            // convoy's decision. A vessel whose convoy record is gone keeps
            // the decision it recorded.
            let decision = match convoys_by_name.get(vessel.spec.convoy_ref.as_str()) {
                Some(convoy) => vessel_decision(convoy, &pins[vessel.spec.convoy_ref.as_str()], &vessel.spec.vessel_name),
                None => vessel.status.as_ref().and_then(|status| status.placement_decision.as_ref()),
            };
            if decision.is_some_and(|decision| targets(decision, host)) {
                placed += 1;
            }
        }
        let mut admitted = Vec::<(DateTime<Utc>, String, u32)>::new();
        for (name, convoy) in &convoys_by_name {
            if !is_live(convoy) {
                continue;
            }
            let pins = &pins[name];
            let created = created.get(name).cloned().unwrap_or_default();
            let names = convoy
                .status
                .as_ref()
                .and_then(|status| status.workflow_snapshot.as_ref())
                .map(|snapshot| snapshot.vessels.iter().map(|vessel| vessel.name.clone()).collect::<Vec<_>>())
                .or_else(|| workflow_vessels.get(*name).cloned());
            let held = match names {
                Some(names) => names
                    .iter()
                    .filter(|vessel| {
                        !created.contains(vessel.as_str())
                            && vessel_decision(convoy, pins, vessel).is_some_and(|decision| targets(decision, host))
                    })
                    .count() as u32,
                // Without the workflow, each pin names a vessel and a
                // convoy-wide decision holds at least one slot.
                None => {
                    let pinned =
                        pins.iter().filter(|(vessel, pin)| targets(&pin.decision, host) && !created.contains(vessel.as_str())).count();
                    let convoy_wide = created.is_empty()
                        && convoy
                            .status
                            .as_ref()
                            .and_then(|status| status.placement_decision.as_ref())
                            .is_some_and(|decision| targets(decision, host));
                    pinned.max(usize::from(convoy_wide)) as u32
                }
            };
            if held > 0 {
                admitted.push((convoy.metadata.creation_timestamp, name.to_string(), held));
            }
        }
        admitted.sort();
        Self { placed, admitted: admitted.into_iter().map(|(_, name, held)| (name, held)).collect() }
    }

    /// Whether counting `convoy` on `host` needs its frozen workflow's vessel
    /// names from the workflow snapshot record: it is live, places on the
    /// host, and has not yet copied the snapshot into its status.
    pub fn needs_workflow_vessels(convoy: &ResourceObject<Convoy>, host: &str) -> bool {
        is_live(convoy)
            && convoy.status.as_ref().is_some_and(|status| status.workflow_snapshot.is_none())
            && (convoy
                .status
                .as_ref()
                .and_then(|status| status.placement_decision.as_ref())
                .is_some_and(|decision| targets(decision, host))
                || placement_pins(convoy).values().any(|pin| targets(&pin.decision, host)))
    }

    /// Vessels counted against the cap.
    pub fn live(&self) -> u32 {
        self.placed + self.admitted.iter().map(|(_, held)| held).sum::<u32>()
    }

    pub fn free_slots(&self, cap: u32) -> u32 {
        cap.saturating_sub(self.live())
    }

    /// Whether `convoy` may provision its vessels now. A convoy waits behind
    /// older admissions; one holding nothing outstanding on the host passes.
    pub fn admits(&self, convoy: &str, cap: u32) -> bool {
        let Some(position) = self.admitted.iter().position(|(name, _)| name == convoy) else {
            return true;
        };
        let through = self.admitted[..=position].iter().map(|(_, held)| held).sum::<u32>();
        self.placed + through <= cap
    }
}

/// Records ordered so each name's home copy comes before its replicas: the
/// one dedupe rule occupancy counting relies on.
pub fn home_copies_first<T: Resource>(mut items: Vec<ReadResourceObject<T>>) -> Vec<ResourceObject<T>> {
    items.sort_by_key(|item| !matches!(item.provenance, ResourceProvenance::Local));
    items.into_iter().map(|item| item.object).collect()
}

fn is_live(convoy: &ResourceObject<Convoy>) -> bool {
    convoy.metadata.deletion_timestamp.is_none() && !convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal())
}

fn targets(decision: &PlacementDecision, host: &str) -> bool {
    decision.target_host.reference.as_str() == host
}

fn placement_pins(convoy: &ResourceObject<Convoy>) -> BTreeMap<String, VesselPlacementPin> {
    convoy.metadata.annotations.get(VESSEL_PLACEMENTS_ANNOTATION).and_then(|encoded| serde_json::from_str(encoded).ok()).unwrap_or_default()
}

/// A vessel's placement: its pin, else the convoy-wide decision.
fn vessel_decision<'a>(
    convoy: &'a ResourceObject<Convoy>,
    pins: &'a BTreeMap<String, VesselPlacementPin>,
    vessel: &str,
) -> Option<&'a PlacementDecision> {
    pins.get(vessel).map(|pin| &pin.decision).or_else(|| convoy.status.as_ref().and_then(|status| status.placement_decision.as_ref()))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use flotilla_protocol::{CanonicalHostId, PlacementTargetHost};
    use hegel::generators as gs;

    use super::*;
    use crate::{ConvoyPhase, ConvoySpec, ConvoyStatus, ObjectMeta, VesselRequirement, VesselSpec, WorkflowSnapshot};

    fn meta(name: &str, created: i64) -> ObjectMeta {
        ObjectMeta {
            name: name.into(),
            namespace: "flotilla".into(),
            resource_version: "1".into(),
            labels: Default::default(),
            annotations: Default::default(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: Utc.timestamp_opt(1_700_000_000 + created, 0).unwrap(),
            merge: None,
        }
    }

    fn decision(host: &str) -> PlacementDecision {
        PlacementDecision {
            policy_name: format!("host-direct-{host}"),
            target_host: PlacementTargetHost { reference: CanonicalHostId::resolved(host), display_name: host.into() },
            minimal_alternatives: Vec::new(),
            escalation_reason: None,
            refused_candidates: Vec::new(),
            viable_not_selected: Vec::new(),
            allocation: None,
            provider_workspace_root: None,
        }
    }

    const HOSTS: [&str; 2] = ["b1", "b2"];

    /// One convoy in the model: its host, its vessels (absent, or created and
    /// whether tearing down), whether its workflow is frozen into its status
    /// or read from the snapshot record, and whether the convoy has finished.
    #[derive(Clone)]
    struct Modelled {
        host: usize,
        vessels: Vec<Option<bool>>,
        snapshot_in_status: bool,
        terminal: bool,
    }

    type Records = (Vec<ResourceObject<Convoy>>, Vec<ResourceObject<Vessel>>, BTreeMap<String, Vec<String>>);

    fn records(model: &[Modelled]) -> Records {
        let mut convoys = Vec::new();
        let mut vessels = Vec::new();
        let mut workflow_vessels = BTreeMap::new();
        for (index, convoy) in model.iter().enumerate() {
            let name = format!("convoy-{index}");
            let names = (0..convoy.vessels.len()).map(|vessel| format!("v{vessel}")).collect::<Vec<_>>();
            let snapshot = convoy.snapshot_in_status.then(|| WorkflowSnapshot {
                cascade: None,
                exit: None,
                turn_delivery: Default::default(),
                stall_nudges: Default::default(),
                supervision: None,
                vessels: names.iter().map(|vessel| VesselRequirement::builder().name(vessel.clone()).crew(Vec::new()).build()).collect(),
            });
            let convoy_record = ResourceObject {
                metadata: meta(&name, index as i64),
                spec: ConvoySpec::builder().workflow_ref("work".to_string()).build(),
                status: Some(ConvoyStatus {
                    placement_decision: Some(decision(HOSTS[convoy.host])),
                    workflow_snapshot: snapshot,
                    phase: if convoy.terminal { ConvoyPhase::Landed } else { ConvoyPhase::Active },
                    ..Default::default()
                }),
            };
            if HostOccupancy::needs_workflow_vessels(&convoy_record, HOSTS[convoy.host]) {
                workflow_vessels.insert(name.clone(), names.clone());
            }
            convoys.push(convoy_record);
            for (vessel, state) in convoy.vessels.iter().enumerate() {
                let Some(tearing_down) = state else { continue };
                let mut metadata = meta(&format!("{name}-v{vessel}"), index as i64);
                metadata.deletion_timestamp = tearing_down.then(Utc::now);
                vessels.push(ResourceObject {
                    metadata,
                    spec: VesselSpec {
                        convoy_ref: name.clone(),
                        vessel_name: format!("v{vessel}"),
                        placement_policy_ref: format!("host-direct-{}", HOSTS[convoy.host]),
                        adopted_checkout_refs: Default::default(),
                    },
                    status: None,
                });
            }
        }
        (convoys, vessels, workflow_vessels)
    }

    // #3793: convoys of one or two vessels are admitted, provisioned a vessel
    // at a time (the first only when the count admits the convoy), and torn
    // down in any interleaving. After every step, for every host:
    // - vessels that exist (including any mid-teardown) never exceed the cap;
    // - free slots are the cap minus every vessel live convoys place there,
    //   created or not, and every vessel still tearing down;
    // - a waiting convoy provisions exactly when the vessels and older
    //   admissions on its host leave room for all of its vessels.
    // Caps span 0 (always full) through 3. Admission refuses a convoy with
    // more vessels than the cap, as placement does. The workflow is read from
    // the convoy's status or from the snapshot record.
    #[hegel::test]
    fn vessels_never_exceed_the_cap_and_free_slots_track_admission_to_teardown(tc: hegel::TestCase) {
        let caps = [tc.draw(gs::integers::<u32>().min_value(0).max_value(3)), tc.draw(gs::integers::<u32>().min_value(0).max_value(3))];
        let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(20));
        let mut model = Vec::<Modelled>::new();
        for _ in 0..steps {
            let op = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
            let pick = tc.draw(gs::integers::<usize>().min_value(0).max_value(7));
            let (convoys, vessels, workflow_vessels) = records(&model);
            match op {
                0 => {
                    let host = pick % HOSTS.len();
                    let size = 1 + pick % 2;
                    if size as u32 <= caps[host] {
                        model.push(Modelled {
                            host,
                            vessels: vec![None; size],
                            snapshot_in_status: pick.is_multiple_of(3),
                            terminal: false,
                        });
                    }
                }
                1 if !model.is_empty() => {
                    let index = pick % model.len();
                    let convoy = &model[index];
                    let started = convoy.vessels.iter().any(Option::is_some);
                    let occupancy = HostOccupancy::count(HOSTS[convoy.host], &convoys, &vessels, &workflow_vessels);
                    if !convoy.terminal && (started || occupancy.admits(&format!("convoy-{index}"), caps[convoy.host])) {
                        if let Some(next) = model[index].vessels.iter_mut().find(|vessel| vessel.is_none()) {
                            *next = Some(false);
                        }
                    }
                }
                2 if !model.is_empty() => {
                    let index = pick % model.len();
                    model[index].terminal = true;
                    for vessel in model[index].vessels.iter_mut().flatten() {
                        *vessel = true;
                    }
                }
                3 if !model.is_empty() => {
                    let index = pick % model.len();
                    if let Some(vessel) = model[index].vessels.iter_mut().find(|vessel| **vessel == Some(true)) {
                        *vessel = None;
                    }
                }
                _ => {}
            }
            let (convoys, vessels, workflow_vessels) = records(&model);
            for (host_index, host) in HOSTS.iter().enumerate() {
                let cap = caps[host_index];
                let occupancy = HostOccupancy::count(host, &convoys, &vessels, &workflow_vessels);
                let on_host = model.iter().enumerate().filter(|(_, convoy)| convoy.host == host_index).collect::<Vec<_>>();
                let existing = on_host.iter().map(|(_, convoy)| convoy.vessels.iter().flatten().count() as u32).sum::<u32>();
                let outstanding = |convoy: &Modelled| {
                    if convoy.terminal {
                        0
                    } else {
                        convoy.vessels.iter().filter(|vessel| vessel.is_none()).count() as u32
                    }
                };
                let pending = on_host.iter().map(|(_, convoy)| outstanding(convoy)).sum::<u32>();
                assert!(existing <= cap, "{host}: {existing} vessels exceed cap {cap}");
                assert_eq!(occupancy.free_slots(cap), cap.saturating_sub(existing + pending), "{host}");
                let mut ahead = 0;
                for (index, convoy) in &on_host {
                    if outstanding(convoy) == 0 {
                        continue;
                    }
                    if convoy.vessels.iter().all(Option::is_none) {
                        assert_eq!(
                            occupancy.admits(&format!("convoy-{index}"), cap),
                            existing + ahead + outstanding(convoy) <= cap,
                            "{host}: convoy-{index} with {ahead} older vessels outstanding"
                        );
                    }
                    ahead += outstanding(convoy);
                }
            }
        }
    }
}
