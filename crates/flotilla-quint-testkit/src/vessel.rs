//! The production vessel composition ([`Placements`]) against `conformance/vessel.qnt`, over any
//! log: the engine's memory log, or the decided store's (#3629 layer 6).
//!
//! The spec's vessel is one state, `Waiting(request) | Leased({placement, lifecycle}) |
//! Gone(placement)`. [`Placements`] keeps it in two parts: the placement [`Ledger`], where the
//! request waits and the lease is held, and the stored Vessel (the lease as its spec) created when
//! the lease is granted. Event mapping:
//! - `Place(evidence)` on Waiting is [`Placements::place`] on placements built so the ledger's
//!   snapshot is the probe's evidence (below); a grant creates the vessel resource, Requested.
//! - `Realise{present, matches}`, `Release{complete}` and `Vanish{absent}` on Leased are
//!   [`Placements::apply`] lifecycle edges (identity `"vessel"`), with the lifecycle evidence
//!   mapping; any lifecycle refusal is the spec's `MissingEvidence`, and so is `Place` on Leased.
//!   Reaching Gone frees the lease at the ledger.
//! - Anything on Gone, and anything but `Place` on Waiting, is `WrongState`.
//!
//! The placements for a placement on host `h` hold `h` with capacity `available`, the convoy's
//! cap for the class `cap`, one waiting request per entry of `waiting`, and, when `live` is 1, a
//! vessel of the same convoy and class leased on a spare host. The ledger derives admission as a
//! positive cap, so evidence with `admitted != (cap > 0)` has no placements: those probes call the
//! pure [`place`] rule the ledger calls, over the probe's evidence. Every such probe is refused
//! (not admitted, or no cap), so the vessel stays Waiting.
//!
//! State mapping: Waiting is the request before any grant; Leased is the stored vessel's lease
//! (its spec, the spec's `Placement = {request, host}`), which must equal the ledger's, with the
//! lifecycle state (no attempts: a vessel has none); Gone is the stored lease of a Gone vessel,
//! which the ledger no longer holds.

use std::marker::PhantomData;

use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::{
        convoy::{ConvoyId, Priority, VesselClass},
        lifecycle::{LifecycleEdge, LifecycleState, MemoryLifecycleWorld, Observed, Realise, Release, Vanish},
        vessel::Vessel,
    },
    placement::{place, Dimension, Evidence, Host, Lease, Placements, Request, Units, Vessel as Placed, VesselId},
    Home, Log, Ref,
};
use serde_json::{json, Value};

use crate::{
    graph::{bare, refusal, tick, Conformance, Step},
    itf::{boolean, int, map_of, number, pairs, payload, set, tag, variant},
};

const NAME: &str = "v";
const IDENTITY: &str = "vessel";
/// The host the `live` vessel is leased on; no probe places there.
const SPARE: Host = Host(99);

/// Where a run's vessel resources are written: a fresh log per subject.
pub trait VesselLogs: 'static {
    type Log: Log<Vessel>;
    fn log() -> Self::Log;
    /// Checks a subject's stored state against what its home holds (a store's row, say).
    fn check(_home: &Home<Vessel, Self::Log>) {}
}

fn count(v: &Value) -> u32 {
    u32::try_from(number(v)).expect("model count")
}

fn units(v: &Value) -> Units {
    pairs(v).iter().map(|p| (Dimension(count(&p[0])), count(&p[1]))).collect()
}

fn request_of(v: &Value) -> Request {
    Request {
        convoy: ConvoyId::new(count(&v["convoy"]).to_string(), "home"),
        class: VesselClass::new(count(&v["class"]).to_string()),
        shape: units(&v["shape"]),
        priority: Priority(count(&v["priority"])),
    }
}

fn units_value(units: &Units) -> Value {
    map_of(units.iter().map(|(d, n)| (int(d.0.into()), int((*n).into()))))
}

fn request_value(r: &Request) -> Value {
    let convoy: i64 = r.convoy.name.parse().expect("conformance convoy name");
    json!({ "convoy": int(convoy), "class": int(r.class.0.parse().expect("model class")), "shape": units_value(&r.shape), "priority": int(r.priority.0.into()) })
}

fn placement_value(lease: &Lease) -> Value {
    json!({ "request": request_value(&lease.request), "host": int(lease.host.0.into()) })
}

fn refusal_name(r: flotilla_state_machines::placement::PlacementRefusal) -> &'static str {
    use flotilla_state_machines::placement::PlacementRefusal;
    match r {
        PlacementRefusal::WrongState => "WrongState",
        PlacementRefusal::NotAdmitted => "NotAdmitted",
        PlacementRefusal::QuotaExceeded => "QuotaExceeded",
        PlacementRefusal::DoesNotFit => "DoesNotFit",
        PlacementRefusal::HigherPriorityWaiting => "HigherPriorityWaiting",
        PlacementRefusal::MissingEvidence => "MissingEvidence",
    }
}

/// The lifecycle state in the spec's `lifecycle` shape: a vessel takes no attempts, so Requested's
/// history is empty.
fn lifecycle_value(state: &LifecycleState) -> Value {
    match state {
        LifecycleState::Requested { failures, .. } => {
            assert_eq!(*failures, 0, "a vessel takes no attempts");
            variant("Requested", Value::Array(vec![]))
        }
        LifecycleState::Realised { identity } => variant("Realised", Value::from(identity.as_str())),
        LifecycleState::Gone => bare("Gone"),
        LifecycleState::GaveUp { cause } => variant("GaveUp", Value::from(cause.detail.as_str())),
    }
}

/// One vessel: its request until a grant, then the placements holding its lease and resource.
pub struct Composed<F: VesselLogs> {
    id: VesselId,
    request: Request,
    placements: Option<Placements<F::Log>>,
}

impl<F: VesselLogs> Composed<F> {
    fn vessel(&self) -> Option<&flotilla_state_machines::Resource<Vessel>> {
        self.placements.as_ref().and_then(|placements| placements.get(&self.id))
    }

    /// Placements whose snapshot for placing the vessel on `evidence.host` is `evidence`, or
    /// `None` when no ledger can present it.
    fn placements_for(&self, evidence: &Evidence) -> Option<Placements<F::Log>> {
        if evidence.admitted != (evidence.cap > 0) || evidence.live > 1 {
            return None;
        }
        let mut hosts = vec![(evidence.host, evidence.available.clone())];
        if evidence.live == 1 {
            // Room to spare, so the live vessel's own fit is never in question.
            hosts.push((SPARE, self.request.shape.iter().map(|(dimension, units)| (*dimension, units.saturating_mul(2).max(1))).collect()));
        }
        let home = Home::with_log("home", F::log()).expect("valid declaration");
        let (mut placements, unplaced) = Placements::new(home, hosts).expect("new hosts");
        assert!(unplaced.is_empty());
        placements.set_caps(self.request.convoy.clone(), [(self.request.class.clone(), evidence.cap)].into());
        if evidence.live == 1 {
            // Leased before anything else waits, so nothing outranks it.
            let live = VesselId::new("live");
            placements.request(live.clone(), self.request.clone()).expect("a new vessel");
            placements.place(&live, SPARE, Timestamp(0)).expect("the live vessel fits the spare host under its cap");
        }
        for (index, waiting) in evidence.waiting.iter().enumerate() {
            placements.request(VesselId::new(format!("waiting{index}")), waiting.clone()).expect("a new vessel");
        }
        placements.request(self.id.clone(), self.request.clone()).expect("a new vessel");
        assert_eq!(
            placements.ledger().evidence(&self.id, evidence.host).as_ref(),
            Ok(evidence),
            "the ledger presents the probe's evidence"
        );
        Some(placements)
    }

    fn place(&mut self, evidence: Evidence) -> Step {
        let Some(mut placements) = self.placements_for(&evidence) else {
            return match place(&self.request, &evidence) {
                Err(r) => refusal(refusal_name(r)),
                Ok(_) => panic!("evidence without a ledger cannot place"),
            };
        };
        match placements.place(&self.id, evidence.host, Timestamp(0)) {
            Ok(_) => {
                F::check(placements.home());
                self.placements = Some(placements);
                Step::Accepted
            }
            Err(error) => match error.refusal() {
                Some(r) => refusal(refusal_name(r)),
                None => panic!("placing on the probe's ledger: {error:?}"),
            },
        }
    }

    fn lifecycle_edge(&mut self, edge: LifecycleEdge, world: &MemoryLifecycleWorld) -> Step {
        let Some(placements) = self.placements.as_mut() else { return refusal("WrongState") };
        match placements.apply(&self.id, edge, world, tick(1)) {
            Ok(_) => {
                F::check(placements.home());
                Step::Accepted
            }
            Err(error) => match error.refusal() {
                Some(_) => refusal("MissingEvidence"),
                None => panic!("a lifecycle edge on a leased vessel: {error:?}"),
            },
        }
    }
}

/// `vessel.qnt` against [`Placements`] over `F`'s logs.
pub struct VesselConformance<F>(PhantomData<F>);

impl<F: VesselLogs> Conformance for VesselConformance<F> {
    const MACHINE: &'static str = "vessel";
    type Subject = Composed<F>;

    fn create(state: &Value) -> Composed<F> {
        Composed { id: VesselId::new(NAME), request: request_of(payload(state)), placements: None }
    }

    fn apply(v: &mut Composed<F>, event: &Value) -> Step {
        let o = payload(event);
        match (v.vessel().map(|vessel| vessel.state().clone()), tag(event)) {
            (Some(LifecycleState::Gone), _) => refusal("WrongState"),
            (None, "Place") => v.place(Evidence {
                host: Host(count(&o["host"])),
                available: units(&o["available"]),
                waiting: set(&o["waiting"]).iter().map(request_of).collect(),
                live: count(&o["live"]),
                cap: count(&o["cap"]),
                admitted: boolean(&o["admitted"]),
            }),
            (None, _) => refusal("WrongState"),
            (Some(_), "Place") => refusal("MissingEvidence"),
            (Some(_), "Realise") => {
                let mut world = MemoryLifecycleWorld::default();
                if boolean(&o["present"]) {
                    let seen = if boolean(&o["matches"]) { IDENTITY.to_string() } else { format!("not-{IDENTITY}") };
                    world.realisations.put(&Ref::new(NAME), Observed { identity: seen });
                }
                v.lifecycle_edge(Realise { identity: IDENTITY.into(), observed: Ref::new(NAME) }.into(), &world)
            }
            (Some(_), "Release") => {
                let mut world = MemoryLifecycleWorld::default();
                world.teardowns.put(&Ref::new(NAME), boolean(&o["complete"]));
                v.lifecycle_edge(Release { teardown: Ref::new(NAME) }.into(), &world)
            }
            (Some(_), "Vanish") => {
                let mut world = MemoryLifecycleWorld::default();
                world.absences.put(&Ref::new(NAME), boolean(&o["absent"]));
                v.lifecycle_edge(Vanish { observed: Ref::new(NAME) }.into(), &world)
            }
            (_, other) => panic!("vessel event {other}"),
        }
    }

    fn encode(v: &Composed<F>) -> Value {
        let Some(placements) = &v.placements else { return variant("Waiting", request_value(&v.request)) };
        let vessel = placements.get(&v.id).expect("a granted lease has its vessel");
        let lease = vessel.spec().lease();
        match (vessel.state(), placements.ledger().vessel(&v.id)) {
            (LifecycleState::Gone, None) => variant("Gone", placement_value(&lease)),
            (lifecycle, Some(Placed::Leased(held))) => {
                assert_eq!(held, &lease, "the stored vessel holds the ledger's lease");
                variant("Leased", json!({ "placement": placement_value(&lease), "lifecycle": lifecycle_value(lifecycle) }))
            }
            (lifecycle, held) => panic!("an inconsistent composite: lifecycle {lifecycle:?}, ledger {held:?}"),
        }
    }

    /// The vessel resource's version: 0 before its grant.
    fn version(v: &Composed<F>) -> u64 {
        v.vessel().map_or(0, |vessel| vessel.version().0)
    }
}
