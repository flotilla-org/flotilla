//! Ensure on the engine behind the `[primitives]` switch (#3629 layer 7).
//!
//! [`EngineEnsurePath`] is admission's [`EngineEnsures`] over the decided store: one engine
//! Ensure per standing role's ConvoyEnsure, named alike, keeping that role's convoy bound. The
//! old standing-convoy pass keeps observing and admitting; each status patch it would write is
//! read here as the edges it means, and the engine decides:
//!
//! | Old patch | Edges |
//! |---|---|
//! | `Running { convoy }` | record and create the convoy if new, a successful attempt, bind it |
//! | `BackingOff` (its convoy failed or was abandoned) | unbind it as gone, a failed retryable attempt |
//! | `Retrying` (admission refused) | a failed retryable attempt |
//! | `RestartLimitReached` | unbind, a failed attempt, hold (the window is spent) |
//! | `ResetBackoff` while held | resume, on the authority that cleared the hold |
//! | anything else (config, drift, driver, holding for verification) | none |
//!
//! The evidence each gate reads is what the old pass observed: a convoy it found running is
//! realised and can receive, its inbox drained (the old pass binds on that and nothing more); one
//! it found failed or abandoned is gone. An admission refused before any convoy existed is an
//! attempt named for the Ensure's admission, since no fulfiller was created. This evidence stands
//! in for reads of the convoy itself until the Convoy kind is on the engine (#3587), when its
//! world reads the engine's Convoy; the operator authority on `Resume` stands in for the hold's
//! clearer, which the old patch does not name.
//!
//! The patch is written only when the engine took every edge; a refused edge writes nothing to the
//! old status, and the next pass reports again. Each edge is its own transaction, so the edges
//! before a refused one stay taken: the next pass's edges are computed from the engine's state, so
//! it resumes after them (a recorded or created convoy is not recorded again). The exception is a
//! refused `Hold`: its failed attempt counted, and the next `RestartLimitReached` counts another,
//! which is what spends the window the old pass already judged spent. An Ensure the engine does not hold, written before
//! the switch, stays on the old path until the import (layer 9).

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_address::{Address, Kind as AddressKind};
use flotilla_admission::convoy_ensure::EngineEnsures;
use flotilla_engine_store::{open_home, StoreLog};
use flotilla_resources::convoy::engine::standing_name;
use flotilla_resources::{Clock, ConvoyEnsure, ConvoyEnsureStatusPatch, ResourceObject};
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::Store;
use flotilla_state_machines::{
    machines::ensure::{
        Attempted, Bind, Create, Ensure, EnsureEdge, EnsureSpec, EnsureState, Fulfiller, FulfillerPhase, FulfillerStatus, Hold,
        MemoryEnsureWorld, Record, Resume, Unbind, UnbindReason,
    },
    AttemptPolicy, Cause, CauseCategory, Failure, Home, Outcome, Ref, Resource, Version, WriteError,
};

/// The old pass's restart budget, as the engine judges it: three consecutive failures, backing
/// off as the old provisioning retry did (30 s doubling, capped at ten minutes).
pub const ENSURE_POLICY: AttemptPolicy = AttemptPolicy { max_failures: 3, base: 30_000_000, factor: 2, cap: 600_000_000 };

/// Who resumes a held Ensure when the old pass clears its hold: an operator dismissed its
/// attention demand, or forced a pass.
const OPERATOR: &str = "operator";

/// The engine's Ensures at this home.
pub struct EngineEnsurePath {
    home: Arc<Mutex<Home<Ensure, StoreLog>>>,
    clock: Arc<dyn Clock>,
}

impl EngineEnsurePath {
    /// The Ensures of this home over `store`, restored from it.
    pub fn open(store: Arc<Mutex<Store>>, clock: Arc<dyn Clock>) -> Result<Self, String> {
        Ok(Self { home: Arc::new(Mutex::new(open_home(StoreLog::new(store))?)), clock })
    }

    /// The Ensure as the engine holds it.
    pub fn get(&self, name: &str) -> Option<Resource<Ensure>> {
        self.home.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(name).cloned()
    }

    /// Decides `patch` for `ensure` now (see the module docs).
    pub fn decide_now(
        &self,
        ensure: &ResourceObject<ConvoyEnsure>,
        patch: ConvoyEnsureStatusPatch,
    ) -> Result<Option<ConvoyEnsureStatusPatch>, String> {
        decide(&self.home, Timestamp::from_datetime(&self.clock.now()), ensure, patch)
    }
}

/// [`EngineEnsurePath::decide_now`] at `now`, over the home it holds.
fn decide(
    home: &Mutex<Home<Ensure, StoreLog>>,
    now: Timestamp,
    ensure: &ResourceObject<ConvoyEnsure>,
    patch: ConvoyEnsureStatusPatch,
) -> Result<Option<ConvoyEnsureStatusPatch>, String> {
    let name = ensure.metadata.name.as_str();
    let mut home = home.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if home.get(name).is_none() {
        // Taken on only from its start: one with history is the import's (layer 9).
        if !untouched(ensure) {
            return Ok(Some(patch));
        }
        let inbox = standing_inbox(ensure)?;
        home.create(name, EnsureSpec { inbox, policy: ENSURE_POLICY }, &MemoryEnsureWorld::default(), now)
            .map_err(|error| format!("Ensure `{name}`: {error:?}"))?;
    }
    let state = home.get(name).expect("held").state().clone();
    let inbox = home.get(name).expect("held").spec().inbox.clone();
    for (edge, world) in edges(name, &inbox, &state, &patch) {
        match home.apply(name, edge.clone(), &world, now) {
            Ok(_) => {}
            Err(WriteError::Refused(refusal)) => {
                tracing::info!(ensure = name, ?edge, ?refusal, "the engine did not take the Ensure's edge; the old status waits");
                return Ok(None);
            }
            Err(error) => return Err(format!("Ensure `{name}`: {error:?}")),
        }
    }
    Ok(Some(patch))
}

#[async_trait]
impl EngineEnsures for EngineEnsurePath {
    async fn decide(
        &self,
        _namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        patch: ConvoyEnsureStatusPatch,
    ) -> Result<Option<ConvoyEnsureStatusPatch>, String> {
        // Each edge is a synchronous SQLite transaction under the home's lock: off the async workers.
        let (home, now, ensure) = (Arc::clone(&self.home), Timestamp::from_datetime(&self.clock.now()), ensure.clone());
        tokio::task::spawn_blocking(move || decide(&home, now, &ensure, patch))
            .await
            .map_err(|error| format!("Ensure decision: {error}"))?
    }
}

/// Whether the old status has no history the engine would have to import.
fn untouched(ensure: &ResourceObject<ConvoyEnsure>) -> bool {
    ensure.status.as_ref().is_none_or(|status| status.convoy_ref.is_none() && status.restart_count == 0 && status.hold_reason.is_none())
}

/// The standing role's address, the inbox an Ensure keeps served.
fn standing_inbox(ensure: &ResourceObject<ConvoyEnsure>) -> Result<Address, String> {
    Address::project(&ensure.spec.project_ref)
        .and_then(|project| project.child(AddressKind::Role, &ensure.spec.role))
        .map_err(|error| format!("standing role address: {error}"))
}

fn fulfiller(convoy: &str) -> Ref<Fulfiller> {
    Ref::new(convoy)
}

fn failed(detail: &str) -> Outcome {
    Outcome::Failed { cause: Cause::new(CauseCategory::External, detail), retryable: true }
}

/// The edges `patch` means for an Ensure in `state`, each with the evidence its gate reads.
fn edges(name: &str, inbox: &Address, state: &EnsureState, patch: &ConvoyEnsureStatusPatch) -> Vec<(EnsureEdge, MemoryEnsureWorld)> {
    let gone = |convoy: &Ref<Fulfiller>| {
        let mut world = MemoryEnsureWorld::default();
        world.fulfillers.put(convoy, FulfillerStatus { phase: FulfillerPhase::Gone, can_receive: false });
        world
    };
    let unbind = |edges: &mut Vec<(EnsureEdge, MemoryEnsureWorld)>| {
        if let Some(bound) = state.bound() {
            edges.push((Unbind { reason: UnbindReason::Gone, fulfiller: bound.clone() }.into(), gone(bound)));
        }
    };
    // An attempt names the fulfiller it tried: the bound one, or the admission when none existed.
    let tried = state.bound().cloned().unwrap_or_else(|| fulfiller(&format!("{name}/admission")));
    let mut edges = Vec::new();
    match patch {
        ConvoyEnsureStatusPatch::Running { .. } => {
            let convoy = fulfiller(
                &standing_name(inbox.get(AddressKind::Project).expect("standing inbox has a project"), inbox.name())
                    .expect("standing inbox has a valid role"),
            );
            if state.bound() == Some(&convoy) {
                return edges;
            }
            unbind(&mut edges);
            if !state.recorded().contains(&convoy) {
                edges.push((Record { fulfiller: convoy.clone() }.into(), MemoryEnsureWorld::default()));
            }
            if !state.created().contains(&convoy) {
                // Every fulfiller recorded before it is gone: the old pass starts one only when no
                // ensured convoy is active.
                let mut world = MemoryEnsureWorld::default();
                for earlier in state.recorded().iter().filter(|earlier| **earlier != convoy) {
                    world.fulfillers.put(earlier, FulfillerStatus { phase: FulfillerPhase::Gone, can_receive: false });
                }
                edges.push((Create { fulfiller: convoy.clone() }.into(), world));
            }
            edges.push((Attempted { fulfiller: convoy.clone(), outcome: Outcome::Succeeded }.into(), MemoryEnsureWorld::default()));
            let mut world = MemoryEnsureWorld::default();
            world.fulfillers.put(&convoy, FulfillerStatus { phase: FulfillerPhase::Realised, can_receive: true });
            world.drains.put(&Ref::new(inbox.to_string()), true);
            edges.push((Bind { fulfiller: convoy }.into(), world));
        }
        ConvoyEnsureStatusPatch::BackingOff { failure, .. } => {
            unbind(&mut edges);
            edges.push((Attempted { fulfiller: tried, outcome: failed(failure) }.into(), MemoryEnsureWorld::default()));
        }
        ConvoyEnsureStatusPatch::Retrying { failure, .. } => {
            edges.push((Attempted { fulfiller: tried, outcome: failed(failure) }.into(), MemoryEnsureWorld::default()));
        }
        ConvoyEnsureStatusPatch::RestartLimitReached { failure, .. } => {
            unbind(&mut edges);
            edges.push((Attempted { fulfiller: tried, outcome: failed(failure) }.into(), MemoryEnsureWorld::default()));
            edges.push((Hold.into(), MemoryEnsureWorld::default()));
        }
        ConvoyEnsureStatusPatch::ResetBackoff if matches!(state, EnsureState::Held { .. }) => {
            let mut world = MemoryEnsureWorld::default();
            world.authorities.put(&Ref::new(OPERATOR), true);
            edges.push((Resume { by: OPERATOR.into(), authority: Ref::new(OPERATOR) }.into(), world));
        }
        _ => {}
    }
    edges
}

/// The Ensure an old ConvoyEnsure imports as (#3629 layer 9), its state derived from its live
/// convoy: bound to the convoy it records while that convoy is live, held once its restart limit
/// was reached, else unfulfilled with its strikes as the window's failures. `convoy_live` says
/// whether the convoy it records is still live (not terminal). Its only refusal is a standing role
/// with no address; every old status maps.
pub fn import_ensure(ensure: &ResourceObject<ConvoyEnsure>, convoy_live: bool) -> Result<Resource<Ensure>, String> {
    use flotilla_resources::ConvoyEnsureHoldReason;
    let status = ensure.status.clone().unwrap_or_default();
    let stable = standing_name(&ensure.spec.project_ref, &ensure.spec.role)?;
    let recorded: BTreeSet<Ref<Fulfiller>> = status.convoy_ref.iter().map(|_| fulfiller(&stable)).collect();
    let failures = status.restart_count;
    let last_failure = (failures > 0).then(|| Failure {
        // Failures with no recorded window read as long past (the epoch): the engine's backoff has
        // run out and the next pass retries at once, the safe direction.
        at: status.retry.as_ref().map_or(Timestamp(0), |retry| Timestamp::from_datetime(&retry.first_failure_at)),
        cause: Cause::new(CauseCategory::External, status.last_failure.clone().unwrap_or_else(|| "the standing convoy failed".into())),
        retryable: true,
    });
    let state = match (&status.hold_reason, &status.convoy_ref) {
        (Some(ConvoyEnsureHoldReason::RestartLimit), _) => {
            EnsureState::Held { recorded: recorded.clone(), created: recorded, failures, last_failure }
        }
        (_, Some(_)) if convoy_live => {
            EnsureState::Fulfilled { recorded: recorded.clone(), created: recorded, failures, last_failure, bound: fulfiller(&stable) }
        }
        _ => EnsureState::Unfulfilled { recorded: recorded.clone(), created: recorded, failures, last_failure },
    };
    let since = Timestamp::from_datetime(&ensure.metadata.creation_timestamp);
    Ok(Resource::stored(
        ensure.metadata.name.clone(),
        EnsureSpec { inbox: standing_inbox(ensure)?, policy: ENSURE_POLICY },
        flotilla_schema::State::new(state, since),
        Version(1),
    ))
}

/// Imports `ensures` (each an old ConvoyEnsure, its home, and whether the convoy it records is
/// live) and rehearses them in a temporary store holding the crew kinds (#3629 layer 9). Returns
/// the counts by stored state and each refused record named, with why.
pub fn rehearse_ensures(
    ensures: &[(ResourceObject<ConvoyEnsure>, String, bool)],
) -> (BTreeMap<&'static str, usize>, Vec<(String, String)>) {
    let mut refused = Vec::new();
    let mut imported = Vec::new();
    for (ensure, home, convoy_live) in ensures {
        match import_ensure(ensure, *convoy_live) {
            Ok(resource) => imported.push((home.clone(), resource)),
            Err(reason) => refused.push((ensure.metadata.name.clone(), reason)),
        }
    }
    let mut kinds = flotilla_schema::KindRegistry::new();
    if let Err(error) = flotilla_engine_store::register_crew(&mut kinds) {
        refused.extend(imported.iter().map(|(_, resource)| (resource.name().to_owned(), format!("registry: {error:?}"))));
        return (BTreeMap::new(), refused);
    }
    let mut failed: BTreeMap<usize, String> = flotilla_engine_store::rehearse(kinds, &imported).into_iter().collect();
    let mut counts = BTreeMap::new();
    for (index, (_, resource)) in imported.iter().enumerate() {
        match failed.remove(&index) {
            Some(reason) => refused.push((resource.name().to_owned(), reason)),
            None => *counts.entry(ensure_label(resource.state())).or_default() += 1,
        }
    }
    (counts, refused)
}

fn ensure_label(state: &EnsureState) -> &'static str {
    match state {
        EnsureState::Unfulfilled { .. } => "unfulfilled",
        EnsureState::Fulfilled { .. } => "fulfilled",
        EnsureState::Held { .. } => "held",
        EnsureState::Retired { .. } => "retired",
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::BTreeMap;

    use chrono::{DateTime, Utc};
    use flotilla_resources::{ConvoyEnsureSpec, ConvoyEnsureStatus, ObjectMeta, StatusPatch, SystemClock};
    use flotilla_schema::KindRegistry;

    use super::*;

    fn path() -> EngineEnsurePath {
        let mut registry = KindRegistry::new();
        flotilla_engine_store::register_crew(&mut registry).expect("registry");
        let store = Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", registry).expect("store");
        EngineEnsurePath::open(Arc::new(Mutex::new(store)), Arc::new(SystemClock)).expect("path")
    }

    pub(super) fn ensure(status: Option<ConvoyEnsureStatus>) -> ResourceObject<ConvoyEnsure> {
        ResourceObject {
            metadata: ObjectMeta {
                name: "flotilla-governor".into(),
                namespace: "flotilla".into(),
                resource_version: "1".into(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                owner_references: vec![],
                finalizers: vec![],
                deletion_timestamp: None,
                creation_timestamp: DateTime::UNIX_EPOCH,
                merge: None,
            },
            spec: ConvoyEnsureSpec::builder().project_ref("flotilla".into()).role("governor".into()).repositories(vec![]).build(),
            status,
        }
    }

    fn label(path: &EngineEnsurePath) -> String {
        format!("{:?}", path.get("flotilla-governor").expect("held").label())
    }

    // Behaviour (#3629 layer 7): behind `[primitives]` the engine's Ensure decides a standing
    // role's convoy: a running convoy is recorded, created and bound; its failure unbinds it and
    // counts an attempt; its replacement is bound in turn; three failures in a row hold the Ensure,
    // and the operator clearing the hold resumes it. Each patch is written as the engine took it.
    #[test]
    fn the_engine_ensure_keeps_a_standing_convoy_bound() {
        let path = path();
        let mut obj = ensure(None);
        let write = |patch: ConvoyEnsureStatusPatch, obj: &mut ResourceObject<ConvoyEnsure>| {
            let decided = path.decide_now(obj, patch.clone()).expect("decided");
            if let Some(patch) = &decided {
                patch.apply(obj.status.get_or_insert_with(Default::default));
            }
            decided.is_some()
        };
        let running = |convoy: &str| ConvoyEnsureStatusPatch::Running { convoy_ref: convoy.into(), observed_at: Utc::now() };
        let failed = || ConvoyEnsureStatusPatch::BackingOff { retry_at: Utc::now(), failure: "convoy failed".into() };

        assert!(write(running("governor-1"), &mut obj));
        assert_eq!(label(&path), "Fulfilled");
        assert_eq!(path.get("flotilla-governor").expect("held").state().bound(), Some(&Ref::new("project:flotilla/convoy:governor")));
        assert!(write(running("governor-1"), &mut obj), "observing the bound convoy again changes nothing");
        assert!(write(failed(), &mut obj));
        assert_eq!(label(&path), "Unfulfilled");
        assert_eq!(path.get("flotilla-governor").expect("held").state().failures(), 1);
        assert!(write(running("governor-2"), &mut obj));
        assert_eq!(path.get("flotilla-governor").expect("held").state().bound(), Some(&Ref::new("project:flotilla/convoy:governor")));
        assert_eq!(path.get("flotilla-governor").expect("held").state().failures(), 0, "a bound replacement is a success");

        for _ in 0..2 {
            assert!(write(failed(), &mut obj));
            assert!(write(ConvoyEnsureStatusPatch::Retrying { retry_at: Utc::now(), failure: "admission refused".into() }, &mut obj));
        }
        assert_eq!(label(&path), "Unfulfilled");
        let limit = ConvoyEnsureStatusPatch::RestartLimitReached { convoy_ref: "governor-3".into(), failure: "admission refused".into() };
        assert!(write(limit, &mut obj), "held once the window is spent");
        assert_eq!(label(&path), "Held");
        assert!(write(ConvoyEnsureStatusPatch::ResetBackoff, &mut obj));
        assert_eq!(label(&path), "Unfulfilled", "resumed on the operator's authority");
    }

    // Behaviour: an Ensure with history from before the switch stays on the old path until the
    // import: its patches pass and the engine holds nothing.
    #[test]
    fn an_ensure_from_before_the_switch_stays_on_the_old_path() {
        let path = path();
        let obj = ensure(Some(ConvoyEnsureStatus { convoy_ref: Some("governor-7".into()), restart_count: 2, ..Default::default() }));
        let patch = ConvoyEnsureStatusPatch::BackingOff { retry_at: Utc::now(), failure: "x".into() };
        assert_eq!(path.decide_now(&obj, patch.clone()).expect("decided"), Some(patch));
        assert!(path.get("flotilla-governor").is_none());
    }

    // Behaviour (#3747 review): the edges before a refused one stay taken, and the next pass
    // resumes from the engine's state. The old pass judges the window spent before the engine
    // does: each refused `Hold` leaves its failed attempt counted, nothing is written, and the
    // third report holds the Ensure.
    #[test]
    fn a_refused_edge_keeps_the_edges_before_it_and_the_next_pass_resumes() {
        let path = path();
        let mut obj = ensure(None);
        let limit = ConvoyEnsureStatusPatch::RestartLimitReached { convoy_ref: "governor-1".into(), failure: "admission refused".into() };
        for failures in 1..=2 {
            assert_eq!(path.decide_now(&obj, limit.clone()).expect("decided"), None, "Hold refused: nothing written");
            assert_eq!(path.get("flotilla-governor").expect("held").state().failures(), failures, "the attempt before it stays");
            assert_eq!(label(&path), "Unfulfilled");
        }
        let decided = path.decide_now(&obj, limit.clone()).expect("decided");
        assert_eq!(decided, Some(limit.clone()));
        limit.apply(obj.status.get_or_insert_with(Default::default));
        assert_eq!(label(&path), "Held");
    }
}

#[cfg(test)]
mod import_tests {
    use flotilla_resources::{ConvoyEnsureHoldReason, ConvoyEnsureStatus};

    use super::{tests::ensure, *};

    // Behaviour (#3629 layer 9): a ConvoyEnsure imports as an Ensure whose state its live convoy
    // decides: bound while its convoy is live, unfulfilled once it is not (its strikes the
    // window's failures), and held once its restart limit was reached.
    #[test]
    fn a_convoy_ensure_imports_with_its_state_derived_from_its_live_convoy() {
        let with = |convoy: Option<&str>, strikes, hold| {
            ensure(Some(ConvoyEnsureStatus {
                convoy_ref: convoy.map(str::to_owned),
                restart_count: strikes,
                hold_reason: hold,
                ..Default::default()
            }))
        };
        let bound = import_ensure(&with(Some("governor-3"), 0, None), true).expect("imports");
        assert_eq!(bound.state().bound(), Some(&Ref::new("project:flotilla/convoy:governor")));
        assert_eq!(bound.spec().inbox.as_str(), "project:flotilla/role:governor");
        let unbound = import_ensure(&with(Some("governor-3"), 2, None), false).expect("imports");
        assert!(matches!(unbound.state(), EnsureState::Unfulfilled { failures: 2, .. }));
        let held = import_ensure(&with(None, 3, Some(ConvoyEnsureHoldReason::RestartLimit)), false).expect("imports");
        assert!(matches!(held.state(), EnsureState::Held { failures: 3, .. }));
    }

    // Behaviour (#3629 layer 9): the rehearsal restores each imported Ensure into a temporary
    // store holding the crew kinds and reads it back; it counts what restores and names the
    // duplicate it refuses, keeping the first.
    #[test]
    fn the_ensure_rehearsal_counts_restored_ensures_and_names_refusals() {
        let live = ensure(Some(ConvoyEnsureStatus { convoy_ref: Some("governor-3".into()), ..Default::default() }));
        let (counts, refused) = rehearse_ensures(&[(live.clone(), "local".into(), true)]);
        assert_eq!(counts, BTreeMap::from([("fulfilled", 1)]));
        assert_eq!(refused, vec![]);
        let (counts, refused) = rehearse_ensures(&[(live.clone(), "local".into(), true), (live, "local".into(), false)]);
        assert_eq!(counts, BTreeMap::from([("fulfilled", 1)]), "the first restores");
        assert_eq!(refused.len(), 1, "{refused:?}");
    }
}
