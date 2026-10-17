//! Vessel placement composed with the shared lifecycle, against
//! `conformance/vessel.qnt`.
//!
//! The spec's vessel is one state, `Waiting(request) | Leased({placement,
//! lifecycle}) | Gone(placement)`. Rust has two parts: the placement
//! [`Ledger`], where the vessel waits and then holds its [`Lease`], and a
//! [`Lifecycle<VesselKind>`] resource created when the lease is granted.
//! [`Composed`] joins them exactly as `vessel.qnt`'s `step` does:
//! - `Place(evidence)` on Waiting is [`Ledger::place`] on a ledger built so
//!   its snapshot is the probe's evidence (below); a lease creates the
//!   lifecycle resource, Requested.
//! - `Realise{present, matches}`, `Release{complete}` and `Vanish{absent}` on
//!   Leased are lifecycle edges (identity `"vessel"`), with the lifecycle
//!   evidence mapping; any lifecycle refusal is the spec's `MissingEvidence`,
//!   and so is `Place` on Leased. When the lifecycle reaches Gone the vessel
//!   calls [`Ledger::release`], which returns the lease and keeps nothing.
//! - Anything on Gone, and anything but `Place` on Waiting, is `WrongState`.
//!
//! The ledger for a placement on host `h` has `h` with capacity `available`,
//! the convoy's cap for the class `cap`, one waiting vessel per request in
//! `waiting`, and, when `live` is 1, a vessel of the same convoy and class
//! leased on a spare host. The ledger derives admission as a positive cap, so
//! evidence with `admitted != (cap > 0)` has no ledger: those probes call the
//! pure [`place`] rule the ledger calls, over the probe's evidence. Every such
//! probe is refused (not admitted, or no cap), so the vessel stays Waiting.
//!
//! State mapping: Waiting is the ledger's `Vessel::Waiting(request)`; Leased is
//! the ledger's `Vessel::Leased(Lease { request, host })` (the spec's
//! `Placement = {request, host}`) with the lifecycle state value; Gone is the
//! lifecycle's Gone with the lease `release` returned. That lease is the one
//! remaining difference from a single resource: the ledger forgets a released
//! vessel, so the composite keeps the returned lease to show `Gone(placement)`.

use flotilla_schema::Timestamp;
use flotilla_state_machines::{
    machines::{
        convoy::{ConvoyId, Priority, VesselClass},
        lifecycle::{Lifecycle, LifecycleEdge, LifecycleKind, LifecycleState, MemoryLifecycleWorld, Observed, Realise, Release, Vanish},
    },
    placement::{place, Dimension, Evidence, Host, Lease, Ledger, LedgerError, PlacementRefusal, Request, Units, Vessel, VesselId},
    Home, Ref,
};
use serde_json::{json, Value};

use crate::{
    graph::{self, refusal, tick, Conformance, Report, Step},
    itf::{boolean, int, map_of, number, pairs, payload, set, tag, variant},
    lifecycle::encode_state,
};

const NAME: &str = "v";
const IDENTITY: &str = "vessel";
/// The host the `live` vessel is leased on; no probe places there.
const SPARE: Host = Host(99);

/// Vessels as a lifecycle kind, for this test: the spec is the placement request.
struct VesselKind;
impl LifecycleKind for VesselKind {
    const NAME: &'static str = "Vessel";
    type Request = Request;
}

type V = Lifecycle<VesselKind>;

fn count(v: &Value) -> u32 {
    u32::try_from(number(v)).expect("model count")
}

fn units(v: &Value) -> Units {
    pairs(v).iter().map(|p| (Dimension(count(&p[0])), count(&p[1]))).collect()
}

fn request_of(v: &Value) -> Request {
    Request {
        convoy: ConvoyId::new(count(&v["convoy"]).to_string()),
        class: VesselClass(count(&v["class"])),
        shape: units(&v["shape"]),
        priority: Priority(count(&v["priority"])),
    }
}

fn units_value(units: &Units) -> Value {
    map_of(units.iter().map(|(d, n)| (int(d.0.into()), int((*n).into()))))
}

fn request_value(r: &Request) -> Value {
    let convoy: i64 = r.convoy.as_str().parse().expect("conformance convoy name");
    json!({ "convoy": int(convoy), "class": int(r.class.0.into()), "shape": units_value(&r.shape), "priority": int(r.priority.0.into()) })
}

fn placement_value(lease: &Lease) -> Value {
    json!({ "request": request_value(&lease.request), "host": int(lease.host.0.into()) })
}

fn refusal_name(r: PlacementRefusal) -> &'static str {
    match r {
        PlacementRefusal::WrongState => "WrongState",
        PlacementRefusal::NotAdmitted => "NotAdmitted",
        PlacementRefusal::QuotaExceeded => "QuotaExceeded",
        PlacementRefusal::DoesNotFit => "DoesNotFit",
        PlacementRefusal::HigherPriorityWaiting => "HigherPriorityWaiting",
        PlacementRefusal::MissingEvidence => "MissingEvidence",
    }
}

/// A ledger whose snapshot for placing `id` on `evidence.host` is `evidence`,
/// or `None` when the ledger cannot present it.
fn ledger_for(id: &VesselId, request: &Request, evidence: &Evidence) -> Option<Ledger> {
    if evidence.admitted != (evidence.cap > 0) || evidence.live > 1 {
        return None;
    }
    let mut ledger = Ledger::new();
    ledger.add_host(evidence.host, evidence.available.clone()).expect("a new host");
    ledger.set_caps(request.convoy.clone(), [(request.class, evidence.cap)].into());
    if evidence.live == 1 {
        // Leased before anything else waits, so nothing outranks it.
        // Room to spare, so the live vessel's own fit is never in question.
        let roomy = request.shape.iter().map(|(dimension, units)| (*dimension, units.saturating_mul(2).max(1))).collect();
        ledger.add_host(SPARE, roomy).expect("a new host");
        let live = VesselId::new("live");
        ledger.request(live.clone(), request.clone()).expect("a new vessel");
        ledger.place(&live, SPARE).expect("the live vessel fits the spare host under its cap");
    }
    for (index, waiting) in evidence.waiting.iter().enumerate() {
        ledger.request(VesselId::new(format!("waiting{index}")), waiting.clone()).expect("a new vessel");
    }
    ledger.request(id.clone(), request.clone()).expect("a new vessel");
    assert_eq!(ledger.evidence(id, evidence.host).as_ref(), Ok(evidence), "the ledger presents the probe's evidence");
    Some(ledger)
}

/// Placement composed with the vessel's lifecycle, mirroring `vessel.qnt`'s `step`.
struct Composed {
    id: VesselId,
    ledger: Ledger,
    lifecycle: Option<Home<V>>,
    /// The lease the ledger returned when the lifecycle reached Gone.
    released: Option<Lease>,
}

impl Composed {
    fn request(&self) -> Option<&Request> {
        match self.ledger.vessel(&self.id) {
            Some(Vessel::Waiting(request)) => Some(request),
            _ => None,
        }
    }

    fn lifecycle_state(&self) -> Option<&LifecycleState> {
        self.lifecycle.as_ref().map(|home| home.get(NAME).expect("exists").state())
    }

    fn place(&mut self, evidence: Evidence) -> Step {
        let request = self.request().expect("a waiting vessel").clone();
        let Some(mut ledger) = ledger_for(&self.id, &request, &evidence) else {
            return match place(&request, &evidence) {
                Err(r) => refusal(refusal_name(r)),
                Ok(_) => panic!("evidence without a ledger cannot place"),
            };
        };
        match ledger.place(&self.id, evidence.host) {
            Ok(lease) => {
                let mut home = Home::<Lifecycle<VesselKind>>::new("home").expect("valid declaration");
                home.create(NAME, lease.request.clone(), &MemoryLifecycleWorld::default(), Timestamp(0)).expect("created");
                self.ledger = ledger;
                self.lifecycle = Some(home);
                Step::Accepted
            }
            Err(LedgerError::Refused(r)) => refusal(refusal_name(r)),
            Err(other) => panic!("placing on the probe's ledger: {other:?}"),
        }
    }

    /// A lifecycle edge on a Leased vessel; any lifecycle refusal is `MissingEvidence`.
    fn lifecycle_edge(&mut self, edge: LifecycleEdge, world: &MemoryLifecycleWorld) -> Step {
        let Some(home) = self.lifecycle.as_mut() else { return refusal("WrongState") };
        match home.apply(NAME, edge, world, tick(1)) {
            Ok(_) => {}
            Err(_) => return refusal("MissingEvidence"),
        }
        if self.lifecycle_state() == Some(&LifecycleState::Gone) {
            self.released = Some(self.ledger.release(&self.id).expect("a Gone vessel held a lease"));
        }
        Step::Accepted
    }
}

struct VesselConformance;

impl Conformance for VesselConformance {
    const MACHINE: &'static str = "vessel";
    type Subject = Composed;

    fn create(state: &Value) -> Composed {
        let id = VesselId::new(NAME);
        let mut ledger = Ledger::new();
        ledger.request(id.clone(), request_of(payload(state))).expect("a new vessel");
        Composed { id, ledger, lifecycle: None, released: None }
    }

    fn apply(v: &mut Composed, event: &Value) -> Step {
        let o = payload(event);
        match (v.lifecycle_state(), tag(event)) {
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

    fn encode(v: &Composed) -> Value {
        match (v.ledger.vessel(&v.id), v.lifecycle_state(), &v.released) {
            (Some(Vessel::Waiting(request)), None, None) => variant("Waiting", request_value(request)),
            (Some(Vessel::Leased(lease)), Some(lifecycle), None) => {
                variant("Leased", json!({ "placement": placement_value(lease), "lifecycle": encode_state(lifecycle, &[]) }))
            }
            (None, Some(LifecycleState::Gone), Some(lease)) => variant("Gone", placement_value(lease)),
            (vessel, lifecycle, released) => {
                panic!("an inconsistent composite: ledger {vessel:?}, lifecycle {lifecycle:?}, released {released:?}")
            }
        }
    }

    /// The lease's grant and its release, plus the lifecycle's writes.
    fn version(v: &Composed) -> u64 {
        let lifecycle = v.lifecycle.as_ref().map_or(0, |home| home.get(NAME).expect("exists").version().0);
        u64::from(v.lifecycle.is_some()) + u64::from(v.released.is_some()) + lifecycle
    }
}

// Every reachable vessel state is reached in Rust by the spec's path, and
// every probe agrees: placement refuses in the spec's order (admission, the
// cap, the fit, a higher-priority waiting request that fits), a lease holds
// the request on one host, the lifecycle half moves or refuses as composed,
// and a Gone vessel shows the lease the ledger released.
#[test]
#[ignore = "export graphs with specs/primitives/check.sh"]
fn vessel_follows_the_full_graph() {
    let mut report = Report::new("vessel");
    graph::check::<VesselConformance>(&graph::load(VesselConformance::MACHINE), &mut report);
    report.finish();
}
