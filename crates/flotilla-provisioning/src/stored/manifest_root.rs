//! ManifestRoot on the engine (#3824 layer 2c): the root that declares a manifest tree's
//! definitions, homed at its host and written only by that host's manifest reconciler and the
//! resolution requests it is sent. The machine is `specs/primitives/machines/manifest_root.qnt`.
//!
//! Owner ruling 2026-10-26: the applied revision and each document's baseline hash, the decided
//! state drift detection needs, are records on the root; each document's phase is a judgement
//! ([`phase`]), never stored; refusal reasons and the source error are latest-value records
//! written on change. The root has one live state, `Bound`, whose data holds the scalar records
//! (the binding, the applied revision, the source error and when it was first recorded). Its collections (baselines, refusals,
//! suspensions, resolutions, and the removals of documents no longer declared, #3992) are spec
//! collections only its edges write, as a Convoy's duties are.
//! A root whose source is removed ends, `Retired`, keeping its records for attribution.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::CharterSource;
use flotilla_schema::{Edge, EdgeError, KindRegistry, RegistryError, Stored, Timestamp};
use flotilla_state_machines::{
    edges, states, Admission, EdgeDef, Ended, Handle, Machine, MachineDef, RecordWrite, Resource, StoredWrite, WrongState,
};
use serde::{Deserialize, Serialize};

/// A document a root declares, by its path, kind, namespace and name, encoded as one key (the old
/// status's `DocumentKey` encoding). Interned in its own domain (`md`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
#[serde(transparent)]
#[store(intern = "md")]
pub struct DocumentName(pub String);

/// Why a document was refused, and since when: the first pass that refused it for this reason.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct RefusalRecord {
    pub reason: String,
    pub since: Timestamp,
}

/// What a resolution does: make the manifest win, or adopt the live spec into the manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum Act {
    #[store(code = "sy")]
    Sync,
    #[store(code = "ad")]
    Adopt,
}

/// Where a resolution stands: requested by an operator, claimed by the reconciler (Started), then
/// one settled outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum Outcome {
    #[store(code = "rq")]
    Requested,
    #[store(code = "st")]
    Started,
    #[store(code = "sy")]
    Synced,
    #[store(code = "ad")]
    Adopted,
    #[store(code = "fl")]
    Failed { reason: String },
}

impl Outcome {
    pub fn settled(&self) -> bool {
        !matches!(self, Outcome::Requested | Outcome::Started)
    }
}

/// One document's resolution request and where it stands.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct ResolutionRecord {
    pub act: Act,
    pub token: String,
    pub by: String,
    pub outcome: Outcome,
}

/// A manifest root, as the engine stores it: kind code `mr`, homed at its host.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "ManifestRoot", code = "mr", state = RootState, index(name = "bound_by_host", eq(host), when(state in [Bound])))]
pub struct RootSpec {
    /// What declares the root: its host, tree path and source. Fixed at declaration.
    pub host: String,
    pub path: String,
    pub source: String,
    /// The hash of what the reconciler last applied, by document.
    #[store(child)]
    pub baselines: BTreeMap<DocumentName, String>,
    /// The documents the last pass to reach them refused.
    #[store(child)]
    pub refusals: BTreeMap<DocumentName, RefusalRecord>,
    /// The documents an operator suspended.
    #[store(child)]
    pub suspended: BTreeSet<DocumentName>,
    #[store(child)]
    pub resolutions: BTreeMap<DocumentName, ResolutionRecord>,
    /// The documents no longer declared whose removal the root still reports, with the revision
    /// that removed each: deleted, or orphaned where a refusal records why its resource was kept.
    #[store(child)]
    pub removals: BTreeMap<DocumentName, String>,
}

states! {
    /// A root's state; its data is the scalar records.
    pub enum RootState => RootLabel {
        /// Declared, and reconciled by its host.
        Bound {
            binding: Option<CharterSource>,
            applied: Option<String>,
            source_error: Option<String>,
            source_failed_at: Option<Timestamp>,
        } = "bd",
        /// Its source was removed; its records stay for attribution.
        Retired {
            binding: Option<CharterSource>,
            applied: Option<String>,
            source_error: Option<String>,
            source_failed_at: Option<Timestamp>,
        } = "rt",
    }
}

impl RootState {
    pub fn binding(&self) -> Option<&CharterSource> {
        match self {
            RootState::Bound { binding, .. } | RootState::Retired { binding, .. } => binding.as_ref(),
        }
    }

    pub fn applied(&self) -> Option<&str> {
        match self {
            RootState::Bound { applied, .. } | RootState::Retired { applied, .. } => applied.as_deref(),
        }
    }

    pub fn source_error(&self) -> Option<&str> {
        match self {
            RootState::Bound { source_error, .. } | RootState::Retired { source_error, .. } => source_error.as_deref(),
        }
    }

    /// When the current source error was first recorded: the start of the stall it makes.
    pub fn source_failed_at(&self) -> Option<Timestamp> {
        match self {
            RootState::Bound { source_failed_at, .. } | RootState::Retired { source_failed_at, .. } => *source_failed_at,
        }
    }
}

/// One reconciliation pass: the documents it reached, with the baseline, refusal and removal it
/// recorded for each. A complete pass reached every declared document, and every removed one it
/// still reports, and advances the applied revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    pub revision: String,
    pub complete: bool,
    pub reached: BTreeSet<String>,
    pub baselines: BTreeMap<String, String>,
    pub refusals: BTreeMap<String, String>,
    /// ADR 0047: absent from an edge a previous generation logged.
    #[serde(default)]
    pub removals: BTreeMap<String, String>,
}

/// The source did not load.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFailed {
    pub error: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suspend {
    pub document: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unsuspend {
    pub document: String,
}

/// An operator's request to resolve one document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestResolution {
    pub document: String,
    pub act: Act,
    pub token: String,
    pub by: String,
}

/// The reconciler's claim (`Started`) or settlement of a request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolve {
    pub document: String,
    pub token: String,
    pub outcome: Outcome,
}

/// The charter binding the root reads changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rebind {
    pub binding: Option<CharterSource>,
}

/// The root's source was removed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retire {}

edges! {
    pub enum RootEdge => RootEdgeKind {
        Applied(Applied) = "ap",
        SourceFailed(SourceFailed) = "sf",
        Suspend(Suspend) = "su",
        Unsuspend(Unsuspend) = "us",
        RequestResolution(RequestResolution) = "rr",
        Resolve(Resolve) = "rs",
        Rebind(Rebind) = "rb",
        Retire(Retire) = "re",
    }
}

/// Why a root write was refused: the spec's `Refusal`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootRefusal {
    /// A failure with no error, a request with no token, a complete pass with no revision, or an
    /// outcome of Requested.
    Empty,
    /// A write that changes nothing, a request repeating the current token, or a second claim.
    Unchanged,
    /// A pass recording a document it did not reach.
    Unreached,
    /// An outcome for a token not requested.
    NoRequest,
    /// A settled outcome for a request not yet claimed.
    Unclaimed,
    /// An outcome for a request already settled.
    Settled,
    /// Any write at a retired root.
    Ended,
}

impl From<Ended<RootLabel>> for RootRefusal {
    fn from(_: Ended<RootLabel>) -> Self {
        RootRefusal::Ended
    }
}

/// Unreachable: every edge leaves Bound, and Retired is an end and refuses first.
impl From<WrongState<RootLabel, RootEdgeKind>> for RootRefusal {
    fn from(_: WrongState<RootLabel, RootEdgeKind>) -> Self {
        RootRefusal::Ended
    }
}

/// The spec writes the root's edges make: each collection replaced whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootWrite {
    Baselines { before: BTreeMap<DocumentName, String>, after: BTreeMap<DocumentName, String> },
    Refusals { before: BTreeMap<DocumentName, RefusalRecord>, after: BTreeMap<DocumentName, RefusalRecord> },
    Suspended { before: BTreeSet<DocumentName>, after: BTreeSet<DocumentName> },
    Resolutions { before: BTreeMap<DocumentName, ResolutionRecord>, after: BTreeMap<DocumentName, ResolutionRecord> },
    Removals { before: BTreeMap<DocumentName, String>, after: BTreeMap<DocumentName, String> },
}

impl RecordWrite<RootSpec> for RootWrite {
    fn field(&self) -> &'static str {
        match self {
            RootWrite::Baselines { .. } => "baselines",
            RootWrite::Refusals { .. } => "refusals",
            RootWrite::Suspended { .. } => "suspended",
            RootWrite::Resolutions { .. } => "resolutions",
            RootWrite::Removals { .. } => "removals",
        }
    }

    fn apply(&self, spec: &mut RootSpec, _version: u64) -> Result<(), flotilla_schema::RecordsFull> {
        match self {
            RootWrite::Baselines { after, .. } => spec.baselines.clone_from(after),
            RootWrite::Refusals { after, .. } => spec.refusals.clone_from(after),
            RootWrite::Suspended { after, .. } => spec.suspended.clone_from(after),
            RootWrite::Resolutions { after, .. } => spec.resolutions.clone_from(after),
            RootWrite::Removals { after, .. } => spec.removals.clone_from(after),
        }
        Ok(())
    }
}

impl StoredWrite<Resource<ManifestRoot>> for RootWrite {
    fn store(&self, edge: Edge<Resource<ManifestRoot>>) -> Result<Edge<Resource<ManifestRoot>>, EdgeError> {
        match self {
            RootWrite::Baselines { before, after } => edge.replace_children("baselines", before, after),
            RootWrite::Refusals { before, after } => edge.replace_children("refusals", before, after),
            RootWrite::Suspended { before, after } => edge.replace_children("suspended", before, after),
            RootWrite::Resolutions { before, after } => edge.replace_children("resolutions", before, after),
            RootWrite::Removals { before, after } => edge.replace_children("removals", before, after),
        }
    }
}

/// What root gates read: nothing beyond the root itself.
pub trait RootWorld {}
impl<T: ?Sized> RootWorld for T {}

/// What declares a root: its host, path and source, and the charter binding it reads first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootRequest {
    pub host: String,
    pub path: String,
    pub source: String,
    pub binding: Option<CharterSource>,
}

/// The ManifestRoot machine.
pub struct ManifestRoot;

/// What a pass leaves: the collections and the state, or why it is refused.
struct Pass {
    baselines: BTreeMap<DocumentName, String>,
    refusals: BTreeMap<DocumentName, RefusalRecord>,
    suspended: BTreeSet<DocumentName>,
    resolutions: BTreeMap<DocumentName, ResolutionRecord>,
    removals: BTreeMap<DocumentName, String>,
    state: RootState,
}

fn names<'a>(documents: impl IntoIterator<Item = &'a String>) -> BTreeSet<DocumentName> {
    documents.into_iter().map(|document| DocumentName(document.clone())).collect()
}

/// The spec's `replaceKeys`: `current` with each of `replaced` taken from `by`, or removed where
/// `by` has none.
fn replace<V: Clone>(
    current: &BTreeMap<DocumentName, V>,
    replaced: &BTreeSet<DocumentName>,
    by: BTreeMap<DocumentName, V>,
) -> BTreeMap<DocumentName, V> {
    let mut next: BTreeMap<_, _> =
        current.iter().filter(|(name, _)| !replaced.contains(*name)).map(|(name, value)| (name.clone(), value.clone())).collect();
    next.extend(by.into_iter().filter(|(name, _)| replaced.contains(name)));
    next
}

/// The records `applied` leaves on `spec` in `state`, at `now`; or the refusal.
fn pass(spec: &RootSpec, state: &RootState, applied: &Applied, now: Timestamp) -> Result<Pass, RootRefusal> {
    let reached = names(&applied.reached);
    if applied
        .baselines
        .keys()
        .chain(applied.refusals.keys())
        .chain(applied.removals.keys())
        .any(|document| !applied.reached.contains(document))
    {
        return Err(RootRefusal::Unreached);
    }
    if applied.complete && applied.revision.is_empty() {
        return Err(RootRefusal::Empty);
    }
    let replaced_in = |keys: Vec<&DocumentName>| -> BTreeSet<DocumentName> {
        if applied.complete {
            keys.into_iter().cloned().chain(reached.iter().cloned()).collect()
        } else {
            reached.clone()
        }
    };
    let baselines = replace(
        &spec.baselines,
        &replaced_in(spec.baselines.keys().collect()),
        applied.baselines.iter().map(|(document, hash)| (DocumentName(document.clone()), hash.clone())).collect(),
    );
    // A refusal that stands for the same reason keeps the time it began.
    let refusals = replace(
        &spec.refusals,
        &replaced_in(spec.refusals.keys().collect()),
        applied
            .refusals
            .iter()
            .map(|(document, reason)| {
                let name = DocumentName(document.clone());
                let since = spec.refusals.get(&name).filter(|current| current.reason == *reason).map_or(now, |current| current.since);
                (name, RefusalRecord { reason: reason.clone(), since })
            })
            .collect(),
    );
    let removals = replace(
        &spec.removals,
        &replaced_in(spec.removals.keys().collect()),
        applied.removals.iter().map(|(document, revision)| (DocumentName(document.clone()), revision.clone())).collect(),
    );
    let (suspended, resolutions) = if applied.complete {
        (
            spec.suspended.intersection(&reached).cloned().collect(),
            spec.resolutions
                .iter()
                .filter(|(name, _)| reached.contains(*name))
                .map(|(name, record)| (name.clone(), record.clone()))
                .collect(),
        )
    } else {
        (spec.suspended.clone(), spec.resolutions.clone())
    };
    let (binding, previous) = (state.binding().cloned(), state.applied().map(str::to_owned));
    let state = RootState::Bound {
        binding,
        applied: if applied.complete { Some(applied.revision.clone()) } else { previous },
        source_error: None,
        source_failed_at: None,
    };
    Ok(Pass { baselines, refusals, suspended, resolutions, removals, state })
}

fn request(h: &Handle<'_, ManifestRoot>, document: &str) -> Option<ResolutionRecord> {
    h.spec().resolutions.get(&DocumentName(document.to_owned())).cloned()
}

fn set_resolution(h: &mut Handle<'_, ManifestRoot>, document: &str, record: ResolutionRecord) {
    let before = h.spec().resolutions.clone();
    let mut after = before.clone();
    after.insert(DocumentName(document.to_owned()), record);
    h.write(RootWrite::Resolutions { before, after });
}

impl Machine for ManifestRoot {
    const NAME: &'static str = "ManifestRoot";
    type Request = RootRequest;
    type Spec = RootSpec;
    type State = RootState;
    type Edge = RootEdge;
    type Write = RootWrite;
    type Refusal = RootRefusal;
    type World = dyn RootWorld;

    fn definition() -> MachineDef<Self> {
        use RootLabel::*;
        MachineDef::new(Bound)
            .edge(
                EdgeDef::<Self, Applied>::new()
                    .from([Bound])
                    .to([Bound])
                    .changes(["applied", "source_error", "source_failed_at"])
                    .writes(["baselines", "refusals", "suspended", "resolutions", "removals"])
                    .gate("a pass records only documents it reached, completes at a revision, and changes something", |h, applied| {
                        let next = pass(h.spec(), h.state(), applied, h.now())?;
                        let spec = h.spec();
                        if next.baselines == spec.baselines
                            && next.refusals == spec.refusals
                            && next.suspended == spec.suspended
                            && next.resolutions == spec.resolutions
                            && next.removals == spec.removals
                            && next.state == *h.state()
                        {
                            Err(RootRefusal::Unchanged)
                        } else {
                            Ok(())
                        }
                    })
                    // The effect recomputes the gate's pass: an edge carries nothing from its gate to its
                    // effect, and `pass` is a pure function of the spec, state, argument and time,
                    // all of which the two share.
                    .effect(|h, _, applied| {
                        let Ok(next) = pass(h.spec(), h.state(), applied, h.now()) else { unreachable!("the gate refused") };
                        let spec = h.spec();
                        if next.baselines != spec.baselines {
                            h.write(RootWrite::Baselines { before: spec.baselines.clone(), after: next.baselines });
                        }
                        if next.refusals != spec.refusals {
                            h.write(RootWrite::Refusals { before: spec.refusals.clone(), after: next.refusals });
                        }
                        if next.suspended != spec.suspended {
                            h.write(RootWrite::Suspended { before: spec.suspended.clone(), after: next.suspended });
                        }
                        if next.resolutions != spec.resolutions {
                            h.write(RootWrite::Resolutions { before: spec.resolutions.clone(), after: next.resolutions });
                        }
                        if next.removals != spec.removals {
                            h.write(RootWrite::Removals { before: spec.removals.clone(), after: next.removals });
                        }
                        next.state
                    }),
            )
            .edge(
                EdgeDef::<Self, SourceFailed>::new()
                    .from([Bound])
                    .to([Bound])
                    .changes(["source_error", "source_failed_at"])
                    .gate("a failure names its error, and a new one", |h, failed| {
                        if failed.error.is_empty() {
                            Err(RootRefusal::Empty)
                        } else if h.state().source_error() == Some(failed.error.as_str()) {
                            Err(RootRefusal::Unchanged)
                        } else {
                            Ok(())
                        }
                    })
                    .effect(|h, state, failed| RootState::Bound {
                        binding: state.binding().cloned(),
                        applied: state.applied().map(str::to_owned),
                        source_error: Some(failed.error.clone()),
                        source_failed_at: Some(h.now()),
                    }),
            )
            .edge(
                EdgeDef::<Self, Suspend>::new()
                    .from([Bound])
                    .to([Bound])
                    .writes(["suspended"])
                    .gate("the document is not suspended", |h, suspend| {
                        if h.spec().suspended.contains(&DocumentName(suspend.document.clone())) {
                            Err(RootRefusal::Unchanged)
                        } else {
                            Ok(())
                        }
                    })
                    .effect(|h, state, suspend| {
                        let before = h.spec().suspended.clone();
                        let mut after = before.clone();
                        after.insert(DocumentName(suspend.document.clone()));
                        h.write(RootWrite::Suspended { before, after });
                        state
                    }),
            )
            .edge(
                EdgeDef::<Self, Unsuspend>::new()
                    .from([Bound])
                    .to([Bound])
                    .writes(["suspended"])
                    .gate("the document is suspended", |h, unsuspend| {
                        if h.spec().suspended.contains(&DocumentName(unsuspend.document.clone())) {
                            Ok(())
                        } else {
                            Err(RootRefusal::Unchanged)
                        }
                    })
                    .effect(|h, state, unsuspend| {
                        let before = h.spec().suspended.clone();
                        let mut after = before.clone();
                        after.remove(&DocumentName(unsuspend.document.clone()));
                        h.write(RootWrite::Suspended { before, after });
                        state
                    }),
            )
            .edge(
                EdgeDef::<Self, RequestResolution>::new()
                    .from([Bound])
                    .to([Bound])
                    .writes(["resolutions"])
                    .gate("a request names a token, other than the current one", |h, requested| {
                        if requested.token.is_empty() {
                            Err(RootRefusal::Empty)
                        } else if request(h, &requested.document).is_some_and(|current| current.token == requested.token) {
                            Err(RootRefusal::Unchanged)
                        } else {
                            Ok(())
                        }
                    })
                    .effect(|h, state, requested| {
                        let record = ResolutionRecord {
                            act: requested.act,
                            token: requested.token.clone(),
                            by: requested.by.clone(),
                            outcome: Outcome::Requested,
                        };
                        set_resolution(h, &requested.document, record);
                        state
                    }),
            )
            .edge(
                EdgeDef::<Self, Resolve>::new()
                    .from([Bound])
                    .to([Bound])
                    .writes(["resolutions"])
                    .gate("the current request is claimed once, then settled once", |h, resolve| {
                        let Some(current) = request(h, &resolve.document).filter(|current| current.token == resolve.token) else {
                            return Err(RootRefusal::NoRequest);
                        };
                        if current.outcome.settled() {
                            return Err(RootRefusal::Settled);
                        }
                        match &resolve.outcome {
                            Outcome::Requested => Err(RootRefusal::Empty),
                            Outcome::Started if current.outcome == Outcome::Started => Err(RootRefusal::Unchanged),
                            Outcome::Started => Ok(()),
                            Outcome::Failed { reason } if reason.is_empty() => Err(RootRefusal::Empty),
                            _ if current.outcome != Outcome::Started => Err(RootRefusal::Unclaimed),
                            _ => Ok(()),
                        }
                    })
                    .effect(|h, state, resolve| {
                        let Some(mut record) = request(h, &resolve.document) else { unreachable!("the gate found the request") };
                        record.outcome = resolve.outcome.clone();
                        set_resolution(h, &resolve.document, record);
                        state
                    }),
            )
            .edge(
                EdgeDef::<Self, Rebind>::new()
                    .from([Bound])
                    .to([Bound])
                    .changes(["binding"])
                    .gate("the binding changes", |h, rebind| {
                        if h.state().binding() == rebind.binding.as_ref() {
                            Err(RootRefusal::Unchanged)
                        } else {
                            Ok(())
                        }
                    })
                    .effect(|_, state, rebind| RootState::Bound {
                        binding: rebind.binding.clone(),
                        applied: state.applied().map(str::to_owned),
                        source_error: state.source_error().map(str::to_owned),
                        source_failed_at: state.source_failed_at(),
                    }),
            )
            .edge(
                EdgeDef::<Self, Retire>::new()
                    .from([Bound])
                    .to([Retired])
                    .ungated("only the root's home retires it, when its source is removed")
                    .effect(|_, state, _| RootState::Retired {
                        binding: state.binding().cloned(),
                        applied: state.applied().map(str::to_owned),
                        source_error: state.source_error().map(str::to_owned),
                        source_failed_at: state.source_failed_at(),
                    }),
            )
            .end(Retired, "the root's source was removed")
    }

    /// A root is declared with no records yet.
    fn admit(_: &mut Admission<'_, ManifestRoot>, request: RootRequest) -> Result<(RootSpec, RootState), RootRefusal> {
        let spec = RootSpec {
            host: request.host,
            path: request.path,
            source: request.source,
            baselines: BTreeMap::new(),
            refusals: BTreeMap::new(),
            suspended: BTreeSet::new(),
            resolutions: BTreeMap::new(),
            removals: BTreeMap::new(),
        };
        Ok((spec, RootState::Bound { binding: request.binding, applied: None, source_error: None, source_failed_at: None }))
    }
}

/// A document's phase: the judgement views and the reconciler compute from the root's records and
/// the document's live and desired hashes; never stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Applied,
    Drifted,
    Pending,
    Refused,
    Suspended,
    Deleted,
    Orphaned,
}

/// The spec's `phase`: a recorded removal first (Orphaned where a refusal records why its resource
/// was kept, else Deleted), then suspension, then a recorded refusal (the last pass's verdict),
/// then agreement, then drift from the baseline. A missing hash reads as none.
pub fn phase(spec: &RootSpec, document: &DocumentName, live: Option<&str>, desired: Option<&str>) -> Phase {
    if spec.removals.contains_key(document) {
        if spec.refusals.contains_key(document) {
            Phase::Orphaned
        } else {
            Phase::Deleted
        }
    } else if spec.suspended.contains(document) {
        Phase::Suspended
    } else if spec.refusals.contains_key(document) {
        Phase::Refused
    } else if live == desired {
        Phase::Applied
    } else if spec.baselines.get(document).is_some_and(|baseline| Some(baseline.as_str()) != live) {
        Phase::Drifted
    } else {
        Phase::Pending
    }
}

/// Registers ManifestRoot into `registry` under code `mr`.
pub fn register(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    registry.register::<Resource<ManifestRoot>>()
}

/// ManifestRoot alone, for its DDL snapshot.
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    register(&mut registry)?;
    Ok(registry)
}
