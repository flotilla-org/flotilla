//! Vessels held against an agentless SSH host's declared cap (#3793).
//!
//! The owning daemon is the only placer on its agentless hosts, so admission,
//! the published `free_vessel_slots` fact and a convoy's capacity wait all
//! count from the same records. A vessel holds its slot from its convoy's
//! admission until its Vessel record is gone, which the teardown finalizer
//! allows only once teardown has finished: a crash mid-teardown keeps the slot.

use chrono::{DateTime, Utc};
use flotilla_protocol::PlacementDecision;

use crate::{vessel_placement_pin, Convoy, ResourceObject, Vessel, VesselPlacementPin, VESSEL_PLACEMENTS_ANNOTATION};

/// The vessels one host holds, counted from the convoys and vessels that name it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostOccupancy {
    /// Vessel records placed on the host, including any still tearing down.
    pub placed: u32,
    /// Admitted convoys whose vessels do not exist yet, oldest first, with
    /// the number of vessels each holds on the host.
    pub admitted: Vec<(String, u32)>,
}

impl HostOccupancy {
    /// Count what `host` (a canonical Host name) holds. The first record of
    /// each name counts; later copies are ignored.
    pub fn count<'a>(
        host: &str,
        convoys: impl IntoIterator<Item = &'a ResourceObject<Convoy>>,
        vessels: impl IntoIterator<Item = &'a ResourceObject<Vessel>>,
    ) -> Self {
        // One record per name: a caller lists the home copy first.
        let mut seen = std::collections::BTreeSet::new();
        let convoys = convoys.into_iter().filter(|convoy| seen.insert(convoy.metadata.name.clone())).collect::<Vec<_>>();
        let mut seen = std::collections::BTreeSet::new();
        let vessels = vessels.into_iter().filter(|vessel| seen.insert(vessel.metadata.name.clone())).collect::<Vec<_>>();
        let placed = vessels
            .iter()
            .filter(|vessel| {
                let convoy = convoys.iter().find(|convoy| convoy.metadata.name == vessel.spec.convoy_ref);
                // The vessel reconciler's own lookup: the vessel's pin, else
                // the convoy's decision. A vessel whose convoy record is gone
                // keeps the decision it recorded.
                let decision = convoy
                    .and_then(|convoy| {
                        vessel_placement_pin(convoy, &vessel.spec.vessel_name)
                            .map(|pin| pin.decision)
                            .or_else(|| convoy.status.as_ref().and_then(|status| status.placement_decision.clone()))
                    })
                    .or_else(|| vessel.status.as_ref().and_then(|status| status.placement_decision.clone()));
                decision.is_some_and(|decision| targets(&decision, host))
            })
            .count() as u32;
        let mut admitted = convoys
            .iter()
            .filter(|convoy| {
                convoy.metadata.deletion_timestamp.is_none()
                    && !convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal())
                    && !vessels.iter().any(|vessel| vessel.spec.convoy_ref == convoy.metadata.name)
            })
            .filter_map(|convoy| {
                let held = admitted_vessels(convoy, host);
                (held > 0).then(|| (convoy.metadata.creation_timestamp, convoy.metadata.name.clone(), held))
            })
            .collect::<Vec<(DateTime<Utc>, String, u32)>>();
        admitted.sort();
        Self { placed, admitted: admitted.into_iter().map(|(_, name, held)| (name, held)).collect() }
    }

    /// Vessels counted against the cap.
    pub fn live(&self) -> u32 {
        self.placed + self.admitted.iter().map(|(_, held)| held).sum::<u32>()
    }

    pub fn free_slots(&self, cap: u32) -> u32 {
        cap.saturating_sub(self.live())
    }

    /// Whether `convoy` may provision its vessels now. An admitted convoy
    /// waits behind older admissions; one with vessels already holds them.
    pub fn admits(&self, convoy: &str, cap: u32) -> bool {
        let Some(position) = self.admitted.iter().position(|(name, _)| name == convoy) else {
            return true;
        };
        let through = self.admitted[..=position].iter().map(|(_, held)| held).sum::<u32>();
        self.placed + through <= cap
    }
}

fn targets(decision: &PlacementDecision, host: &str) -> bool {
    decision.target_host.reference.as_str() == host
}

/// Vessels an unprovisioned convoy holds on `host`: one per vessel pin there,
/// or one for a convoy-wide decision there. The convoy-wide decision does not
/// name its vessels, so it holds a single slot until they are created.
fn admitted_vessels(convoy: &ResourceObject<Convoy>, host: &str) -> u32 {
    let pins = convoy
        .metadata
        .annotations
        .get(VESSEL_PLACEMENTS_ANNOTATION)
        .and_then(|encoded| serde_json::from_str::<std::collections::BTreeMap<String, VesselPlacementPin>>(encoded).ok())
        .unwrap_or_default();
    let pinned = pins.values().filter(|pin| targets(&pin.decision, host)).count() as u32;
    let convoy_wide =
        convoy.status.as_ref().and_then(|status| status.placement_decision.as_ref()).is_some_and(|decision| targets(decision, host));
    pinned.max(u32::from(convoy_wide))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use flotilla_protocol::{CanonicalHostId, PlacementTargetHost};
    use hegel::generators as gs;

    use super::*;
    use crate::{ConvoyPhase, ConvoySpec, ConvoyStatus, ObjectMeta, VesselSpec};

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
        }
    }

    const HOSTS: [&str; 2] = ["b1", "b2"];

    /// One convoy in the model: its host, whether its vessel exists (and is
    /// tearing down), and whether the convoy has finished.
    #[derive(Clone)]
    struct Modelled {
        host: usize,
        vessel: Option<bool>,
        terminal: bool,
    }

    fn records(model: &[Modelled]) -> (Vec<ResourceObject<Convoy>>, Vec<ResourceObject<Vessel>>) {
        let mut convoys = Vec::new();
        let mut vessels = Vec::new();
        for (index, convoy) in model.iter().enumerate() {
            let name = format!("convoy-{index}");
            convoys.push(ResourceObject {
                metadata: meta(&name, index as i64),
                spec: ConvoySpec::builder().workflow_ref("work".to_string()).build(),
                status: Some(ConvoyStatus {
                    placement_decision: Some(decision(HOSTS[convoy.host])),
                    phase: if convoy.terminal { ConvoyPhase::Landed } else { ConvoyPhase::Active },
                    ..Default::default()
                }),
            });
            if let Some(tearing_down) = convoy.vessel {
                let mut metadata = meta(&format!("{name}-work"), index as i64);
                metadata.deletion_timestamp = tearing_down.then(Utc::now);
                vessels.push(ResourceObject {
                    metadata,
                    spec: VesselSpec {
                        convoy_ref: name,
                        vessel_name: "work".into(),
                        placement_policy_ref: format!("host-direct-{}", HOSTS[convoy.host]),
                        adopted_checkout_refs: Default::default(),
                    },
                    status: None,
                });
            }
        }
        (convoys, vessels)
    }

    // #3793: convoys are admitted, provisioned when the count admits them, and
    // torn down in any interleaving. After every step, for every host:
    // - vessels that exist (including any mid-teardown) never exceed the cap;
    // - free slots are the cap minus vessels held from admission until
    //   teardown finishes;
    // - a waiting convoy provisions exactly when the vessels and older
    //   admissions on its host leave it room (admission order).
    // Caps span 0 (always full) through 2; operations cover every lifecycle edge.
    #[hegel::test]
    fn vessels_never_exceed_the_cap_and_free_slots_track_admission_to_teardown(tc: hegel::TestCase) {
        let caps = [tc.draw(gs::integers::<u32>().min_value(0).max_value(2)), tc.draw(gs::integers::<u32>().min_value(0).max_value(2))];
        let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(16));
        let mut model = Vec::<Modelled>::new();
        for _ in 0..steps {
            let op = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
            let pick = tc.draw(gs::integers::<usize>().min_value(0).max_value(7));
            let (convoys, vessels) = records(&model);
            match op {
                0 => model.push(Modelled { host: pick % HOSTS.len(), vessel: None, terminal: false }),
                1 if !model.is_empty() => {
                    let index = pick % model.len();
                    let convoy = &model[index];
                    let occupancy = HostOccupancy::count(HOSTS[convoy.host], &convoys, &vessels);
                    if convoy.vessel.is_none() && !convoy.terminal && occupancy.admits(&format!("convoy-{index}"), caps[convoy.host]) {
                        model[index].vessel = Some(false);
                    }
                }
                2 if !model.is_empty() => {
                    let index = pick % model.len();
                    model[index].terminal = true;
                    if model[index].vessel.is_some() {
                        model[index].vessel = Some(true);
                    }
                }
                3 if !model.is_empty() => {
                    let index = pick % model.len();
                    if model[index].vessel == Some(true) {
                        model[index].vessel = None;
                    }
                }
                _ => {}
            }
            let (convoys, vessels) = records(&model);
            for (host_index, host) in HOSTS.iter().enumerate() {
                let cap = caps[host_index];
                let occupancy = HostOccupancy::count(host, &convoys, &vessels);
                let on_host = model.iter().enumerate().filter(|(_, convoy)| convoy.host == host_index).collect::<Vec<_>>();
                let existing = on_host.iter().filter(|(_, convoy)| convoy.vessel.is_some()).count() as u32;
                let waiting = on_host
                    .iter()
                    .filter(|(_, convoy)| convoy.vessel.is_none() && !convoy.terminal)
                    .map(|(index, _)| *index)
                    .collect::<Vec<_>>();
                assert!(existing <= cap, "{host}: {existing} vessels exceed cap {cap}");
                assert_eq!(occupancy.free_slots(cap), cap.saturating_sub(existing + waiting.len() as u32), "{host}");
                for (ahead, index) in waiting.iter().enumerate() {
                    assert_eq!(
                        occupancy.admits(&format!("convoy-{index}"), cap),
                        existing + (ahead as u32) < cap,
                        "{host}: convoy-{index} with {ahead} older admissions"
                    );
                }
            }
        }
    }
}
