//! Placement (spec `machines/vessel.qnt`, scenario `admission_placement.qnt`;
//! ADR 0064): placement reads only fit and priority.

use std::collections::BTreeMap;

use flotilla_state_machines::{
    machines::convoy::{ConvoyId, Priority, VesselClass},
    placement::{fits, place, Dimension, Evidence, Host, Lease, Ledger, LedgerError, PlacementRefusal, Request, Units, Vessel, VesselId},
};
use hegel::generators as gs;

fn units(pairs: &[(u32, u32)]) -> Units {
    pairs.iter().map(|(dimension, n)| (Dimension(*dimension), *n)).collect()
}

fn convoy(n: u32) -> ConvoyId {
    ConvoyId::new(format!("convoy-{n}"), "home")
}

fn vessel(n: u32) -> VesselId {
    VesselId::new(format!("vessel-{n}"))
}

fn request(convoy_n: u32, class: u32, shape: &[(u32, u32)], priority: u32) -> Request {
    Request { convoy: convoy(convoy_n), class: VesselClass::new(class.to_string()), shape: units(shape), priority: Priority(priority) }
}

fn evidence(available: &[(u32, u32)], waiting: Vec<Request>) -> Evidence {
    Evidence { host: Host::new("1"), available: units(available), waiting }
}

// Generator: shapes and capacities over dimensions 0..=2 with 0..=3 units, so
// a dimension may be missing on either side and units collide at the boundary.
fn draw_units(tc: &hegel::TestCase) -> Units {
    (0..3).filter(|_| tc.draw(gs::booleans())).map(|d| (Dimension(d), tc.draw(gs::integers::<u32>().min_value(0).max_value(3)))).collect()
}

fn draw_request(tc: &hegel::TestCase) -> Request {
    Request {
        convoy: convoy(tc.draw(gs::integers::<u32>().min_value(1).max_value(2))),
        class: VesselClass::new(tc.draw(gs::integers::<u32>().min_value(0).max_value(1)).to_string()),
        shape: draw_units(tc),
        priority: Priority(tc.draw(gs::integers::<u32>().min_value(0).max_value(2))),
    }
}

// Fit is monotone and reflexive: more capacity never breaks a fit, a shape
// fits capacity equal to itself, and an empty shape always fits.
#[hegel::test(test_cases = 200)]
fn fit_is_monotone_and_reflexive(tc: hegel::TestCase) {
    let r = Request { shape: draw_units(&tc), ..request(1, 0, &[], 0) };
    let available = draw_units(&tc);
    let more: Units = (0..3).map(|d| (Dimension(d), available.get(&Dimension(d)).copied().unwrap_or(0) + 1)).collect();
    if fits(&r, &available) {
        assert!(fits(&r, &more));
    }
    assert!(fits(&r, &r.shape));
    assert!(fits(&Request { shape: Units::new(), ..r }, &available));
}

// A request with an empty shape fits anywhere, even a host with no
// capacity entries: the spec's `fits` quantifies over the request's own
// dimensions, so it holds vacuously.
#[test]
fn an_empty_shape_fits_vacuously() {
    let empty = request(1, 0, &[], 1);
    assert!(fits(&empty, &units(&[])));
    assert!(place(&empty, &evidence(&[], vec![])).is_ok());
}

// A shape fits only when every one of its dimensions is present and no larger
// than what is available; a missing dimension does not fit.
#[test]
fn fit_is_per_dimension() {
    let r = request(1, 0, &[(0, 2), (1, 1)], 1);
    assert!(fits(&r, &units(&[(0, 2), (1, 1)])));
    assert!(!fits(&r, &units(&[(0, 1), (1, 5)])));
    assert!(!fits(&r, &units(&[(0, 5)])));
}

// The spec's Place order: fit, then a fitting waiting request of strictly
// higher priority. A refusal to fit wins over priority, and the granted lease
// is the request on the snapshot's host.
#[test]
fn place_checks_the_rules_in_the_spec_order() {
    let r = request(1, 0, &[(0, 2)], 1);
    let higher = request(2, 0, &[(0, 1)], 2);
    let unfitting = evidence(&[(0, 1)], vec![higher.clone()]);
    assert_eq!(place(&r, &unfitting), Err(PlacementRefusal::DoesNotFit));
    let fitting = Evidence { available: units(&[(0, 2)]), ..unfitting };
    assert_eq!(place(&r, &fitting), Err(PlacementRefusal::HigherPriorityWaiting));
    let granted = place(&r, &Evidence { waiting: vec![], host: Host::new("7"), ..fitting });
    assert_eq!(granted, Ok(Lease { request: r, host: Host::new("7") }));
}

// A lower-priority request cannot pass a waiting higher-priority request that
// fits this host; a higher-priority request that does not fit here, or one of
// equal priority, does not block.
#[test]
fn priority_blocks_only_a_fitting_higher_request() {
    let r = request(1, 0, &[(0, 1)], 1);
    let higher = request(2, 0, &[(0, 2)], 2);
    let wider = request(2, 0, &[(0, 1), (1, 1)], 2);
    let equal = request(2, 0, &[(0, 1)], 1);
    assert_eq!(place(&r, &evidence(&[(0, 2)], vec![higher.clone()])), Err(PlacementRefusal::HigherPriorityWaiting));
    assert!(place(&r, &evidence(&[(0, 1)], vec![higher])).is_ok());
    assert!(place(&r, &evidence(&[(0, 2)], vec![wider, equal])).is_ok());
}

// The ledger's explicit errors: unknown vessel and host, a duplicate host or
// request, placing a vessel that is not waiting, releasing one not leased, and
// a placement refusal. (UnknownDimension and CapacityUnderflow guard ledger
// corruption; leases are only granted within capacity, so the public API cannot reach them.)
#[test]
fn the_ledger_reports_errors() {
    let (v1, v7) = (vessel(1), vessel(7));
    let mut ledger = Ledger::new();
    ledger.add_host(Host::new("1"), units(&[(0, 2)])).expect("host");
    assert_eq!(ledger.add_host(Host::new("1"), units(&[(0, 1)])), Err(LedgerError::DuplicateHost(Host::new("1"))));
    assert_eq!(ledger.available(Host::new("9")), Err(LedgerError::UnknownHost(Host::new("9"))));
    ledger.request(v1.clone(), request(1, 0, &[(0, 1)], 1)).expect("requested");
    assert_eq!(ledger.request(v1.clone(), request(1, 0, &[(0, 1)], 1)), Err(LedgerError::DuplicateRequest(v1.clone())));
    assert_eq!(ledger.place(&v7, Host::new("1")).err(), Some(LedgerError::UnknownVessel(v7.clone())));
    assert_eq!(ledger.place(&v1, Host::new("9")).err(), Some(LedgerError::UnknownHost(Host::new("9"))));
    ledger.request(vessel(2), request(1, 0, &[(0, 3)], 1)).expect("requested");
    assert_eq!(ledger.place(&vessel(2), Host::new("1")).err(), Some(LedgerError::Refused(PlacementRefusal::DoesNotFit)));
    assert_eq!(ledger.release(&v1).err(), Some(LedgerError::NotLeased(v1.clone())));
    assert!(matches!(ledger.vessel(&v1), Some(Vessel::Waiting(_))), "a refused release keeps the waiting vessel");
    assert_eq!(ledger.release(&v7).err(), Some(LedgerError::UnknownVessel(v7)));
    ledger.place(&v1, Host::new("1")).expect("placed");
    assert_eq!(ledger.place(&v1, Host::new("1")).err(), Some(LedgerError::NotWaiting(v1.clone())));
    assert_eq!(ledger.evidence(&v1, Host::new("1")).err(), Some(LedgerError::NotWaiting(v1.clone())));
}

// Release hands the lease back by value and forgets the vessel: nothing
// returned is retained, a second release finds no vessel, and the name may
// request again.
#[test]
fn release_returns_the_lease_and_keeps_nothing() {
    let v1 = vessel(1);
    let mut ledger = Ledger::new();
    ledger.add_host(Host::new("1"), units(&[(0, 2)])).expect("host");
    ledger.request(v1.clone(), request(1, 0, &[(0, 1)], 1)).expect("requested");
    ledger.place(&v1, Host::new("1")).expect("placed");
    assert_eq!(ledger.release(&v1), Ok(Lease { request: request(1, 0, &[(0, 1)], 1), host: Host::new("1") }));
    assert_eq!(ledger.vessel(&v1), None);
    assert_eq!(ledger.leases().count(), 0);
    assert_eq!(ledger.release(&v1), Err(LedgerError::UnknownVessel(v1.clone())));
    ledger.request(v1, request(1, 0, &[(0, 1)], 1)).expect("requested again");
}

// A lease holds its whole shape on one host until released: availability is
// capacity less live leases, and a released lease frees its shape.
#[test]
fn a_lease_holds_capacity_until_released() {
    let (v1, v2) = (vessel(1), vessel(2));
    let mut ledger = Ledger::new();
    ledger.add_host(Host::new("1"), units(&[(0, 2), (1, 1)])).expect("host");
    ledger.request(v1.clone(), request(1, 0, &[(0, 2), (1, 1)], 1)).expect("requested");
    ledger.request(v2.clone(), request(1, 0, &[(0, 1)], 1)).expect("requested");
    ledger.place(&v1, Host::new("1")).expect("placed");
    assert_eq!(ledger.available(Host::new("1")), Ok(units(&[(0, 0), (1, 0)])));
    assert_eq!(ledger.place(&v2, Host::new("1")).err(), Some(LedgerError::Refused(PlacementRefusal::DoesNotFit)));
    ledger.release(&v1).expect("released");
    assert_eq!(ledger.available(Host::new("1")), Ok(units(&[(0, 2), (1, 1)])));
    assert_eq!(ledger.place(&v2, Host::new("1")).cloned(), Ok(Lease { request: request(1, 0, &[(0, 1)], 1), host: Host::new("1") }));
}

#[derive(Clone, Debug)]
enum Op {
    Request(u32, Request),
    Place(u32, u32),
    Release(u32),
}

// Generator: the admission_placement bounds (two convoys, two hosts with its
// capacities, optionally doubled, two classes, two dimensions) widened to five vessels, and the
// scenario's three request shapes.
fn draw_op(tc: &hegel::TestCase) -> Op {
    let id = tc.draw(gs::integers::<u32>().min_value(1).max_value(5));
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(8)) {
        0..=2 => {
            let shapes = [units(&[(0, 2), (1, 1)]), units(&[(0, 1), (1, 2)]), units(&[(0, 1), (1, 1)])];
            let s = shapes[tc.draw(gs::integers::<usize>().min_value(0).max_value(2))].clone();
            Op::Request(id, Request { shape: s, ..draw_request(tc) })
        }
        3..=6 => Op::Place(id, tc.draw(gs::integers::<u32>().min_value(1).max_value(2))),
        _ => Op::Release(id),
    }
}

// The ledger's invariants over any sequence of requests, placements and
// releases, checked after every step: each host's leased shapes never exceed
// its capacity, and availability is exactly capacity less live leases, so a
// release restores what its lease held; a release returns exactly the lease
// it held; and the ledger's placement agrees with `place` over the evidence
// it reports.
#[hegel::test(test_cases = 200)]
fn the_ledger_keeps_capacity(tc: hegel::TestCase) {
    // The scenario's capacities, sometimes doubled so several vessels fit at once.
    let scale = tc.draw(gs::integers::<u32>().min_value(1).max_value(2));
    let capacities =
        BTreeMap::from([(Host::new("1"), units(&[(0, 2 * scale), (1, scale)])), (Host::new("2"), units(&[(0, scale), (1, 2 * scale)]))]);
    let mut ledger = Ledger::new();
    for (host, capacity) in &capacities {
        ledger.add_host(host.clone(), capacity.clone()).expect("host");
    }
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(30));
    for _ in 0..steps {
        match draw_op(&tc) {
            Op::Request(id, r) => {
                let fresh = ledger.vessel(&vessel(id)).is_none();
                assert_eq!(ledger.request(vessel(id), r).is_ok(), fresh);
            }
            Op::Place(id, host) => {
                let expected = ledger.evidence(&vessel(id), Host::new(host.to_string())).and_then(|e| match ledger.vessel(&vessel(id)) {
                    Some(Vessel::Waiting(r)) => place(r, &e).map_err(LedgerError::Refused),
                    _ => Err(LedgerError::NotWaiting(vessel(id))),
                });
                assert_eq!(ledger.place(&vessel(id), Host::new(host.to_string())).cloned(), expected);
            }
            Op::Release(id) => {
                let held = match ledger.vessel(&vessel(id)) {
                    Some(Vessel::Leased(lease)) => Some(lease.clone()),
                    _ => None,
                };
                assert_eq!(ledger.release(&vessel(id)).ok(), held);
            }
        }
        for (host, capacity) in &capacities {
            let mut expected = capacity.clone();
            for (_, lease) in ledger.leases().filter(|(_, lease)| lease.host == *host) {
                for (d, n) in &lease.request.shape {
                    let free = expected.get_mut(d).expect("leased dimension exists");
                    *free = free.checked_sub(*n).expect("host over capacity");
                }
            }
            assert_eq!(ledger.available(host.clone()), Ok(expected));
        }
    }
}
