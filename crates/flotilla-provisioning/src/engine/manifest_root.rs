//! ManifestRoot on the engine, behind `[primitives]` (#3824 layer 2c).
//!
//! [`EngineManifestRoots`] is the [`ManifestRoots`] port over the decided store. This home writes
//! its own roots through the ManifestRoot machine; every home's are read from a resident map kept
//! current by following the log. No old-store record is written or read.
//!
//! A root is never revived (ADR 0064), so a source declared again after its root retired gets the
//! next generation of the name (`<name>.<n>`). Callers address roots by the name they declare
//! (`manifest-…`, `ops-…`); the port resolves it to the live generation.
//!
//! Phase is a judgement. The port's views carry, for a root homed here, the last pass's judgement
//! of each document (kept in memory, never stored); elsewhere, and before a first pass, a document
//! reads from its records alone: Refused for a recorded refusal, Suspended for a suspension, and
//! Applied otherwise.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use flotilla_engine_store::{
    follow::{Followed, Follower},
    open_home, StoreLog,
};
use flotilla_resources::{
    manifest_roots::{refusal_evidence, refusals, root_stall, ManifestRoots, PassReport, RootDeclaration, RootView},
    DocumentKey, DocumentPhase, DocumentState, ManifestRootSpec, ManifestRootStatus, Resolution, ResolutionAction, ResolutionOutcome,
};
use flotilla_schema::{Identity, Kind, KindCode, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
use flotilla_state_machines::{Home, Resource, WriteError};

use super::manifest_root_import::document_name;
use crate::stored::manifest_root::{
    Act, Applied, DocumentName, ManifestRoot, Outcome, Rebind, RequestResolution, Resolve, Retire, RootEdge, RootRefusal, RootRequest,
    RootState, SourceFailed, Suspend, Unsuspend,
};

const ROOT: KindCode = <Resource<ManifestRoot> as Kind>::CODE;

/// The name `name` was declared under: a generation's name without its `.<n>`.
fn base(name: &str) -> &str {
    name.split_once('.').map_or(name, |(base, _)| base)
}

/// The generation of an engine name: 0 for the declared name itself.
fn generation(name: &str) -> u64 {
    name.split_once('.').and_then(|(_, n)| n.parse().ok()).unwrap_or(0)
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

type Roots = BTreeMap<(String, String), Resource<ManifestRoot>>;

/// Every home's roots, as this store holds them.
struct Resident {
    follower: Follower,
    roots: Roots,
}

/// The ManifestRoots of the decided store at one home.
pub struct EngineManifestRoots {
    service: Arc<Service>,
    home: String,
    roots: Mutex<Home<ManifestRoot, StoreLog>>,
    resident: Mutex<Resident>,
    /// The last pass's judgement of each document of each root homed here, by engine name.
    judged: Mutex<BTreeMap<String, BTreeMap<DocumentKey, DocumentState>>>,
    clock: Arc<dyn flotilla_resources::Clock>,
}

impl EngineManifestRoots {
    /// Opens this home's root writer over `service`, restored from the store, and reads every
    /// home's roots.
    pub fn open(service: Arc<Service>, clock: Arc<dyn flotilla_resources::Clock>) -> Result<Self, String> {
        let log = StoreLog::new(service.store());
        let home = log.home().to_owned();
        let roots = open_home::<ManifestRoot>(log)?;
        let resident = Resident { follower: Follower::new([ROOT]), roots: BTreeMap::new() };
        let opened =
            Self { service, home, roots: Mutex::new(roots), resident: Mutex::new(resident), judged: Mutex::new(BTreeMap::new()), clock };
        let homes = opened.service.homes();
        {
            let mut resident = locked(&opened.resident);
            opened
                .service
                .with_store(|store| {
                    let mut read = store.read()?;
                    resident.follower.start(&mut read, &homes)?;
                    for home in &homes {
                        rescan(&mut read, &mut resident, home)?;
                    }
                    Ok(())
                })
                .map_err(|error| error.to_string())?;
        }
        Ok(opened)
    }

    /// Brings the resident map up to the store: every entry written since the last refresh.
    fn refresh(&self) -> Result<MutexGuard<'_, Resident>, String> {
        let mut resident = locked(&self.resident);
        let homes = self.service.homes();
        self.service
            .with_store(|store| {
                let mut read = store.read()?;
                for followed in resident.follower.pass(&mut read, &homes)? {
                    match followed {
                        Followed::Changed(identity) => reload(&mut read, &mut resident, &identity)?,
                        Followed::Rescan(home) => rescan(&mut read, &mut resident, &home)?,
                    }
                }
                Ok(())
            })
            .map_err(|error| error.to_string())?;
        Ok(resident)
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_datetime(&self.clock.now())
    }

    /// The engine name of root `name`'s live generation homed here, if any.
    fn live(&self, name: &str) -> Option<String> {
        let roots = locked(&self.roots);
        roots
            .resources()
            .filter(|root| base(root.name()) == name && matches!(root.state(), RootState::Bound { .. }))
            .map(|root| root.name().to_owned())
            .max_by_key(|engine| generation(engine))
    }

    fn live_or_missing(&self, name: &str) -> Result<String, String> {
        self.live(name).ok_or_else(|| format!("ManifestRoot {name} is not declared here"))
    }

    /// Applies `edge` to this home's root `engine`; a write that changes nothing is done.
    fn apply(&self, engine: &str, edge: impl Into<RootEdge>) -> Result<bool, String> {
        match locked(&self.roots).apply(engine, edge, &(), self.now()) {
            Ok(_) => Ok(true),
            Err(WriteError::Refused(RootRefusal::Unchanged)) => Ok(false),
            Err(WriteError::Refused(refusal)) => Err(format!("ManifestRoot {engine} refused: {refusal:?}")),
            Err(other) => Err(format!("ManifestRoot {engine}: {other:?}")),
        }
    }

    /// The view of `root`, homed at `home`.
    fn view(&self, home: &str, root: &Resource<ManifestRoot>) -> RootView {
        let judged = (home == self.home).then(|| locked(&self.judged).get(root.name()).cloned()).flatten();
        view(base(root.name()), home == self.home, root, judged.as_ref(), self.clock.now())
    }

    /// Root `name`'s live generation at each home. A retired generation keeps its records for
    /// attribution but is no longer declared, so no view shows it, as the old store's deleted root.
    fn views(&self, resident: &Resident, name: Option<&str>) -> Vec<RootView> {
        let mut chosen = BTreeMap::<(String, String), &Resource<ManifestRoot>>::new();
        for ((home, engine), root) in &resident.roots {
            if name.is_some_and(|name| base(engine) != name) || !matches!(root.state(), RootState::Bound { .. }) {
                continue;
            }
            let slot = chosen.entry((home.clone(), base(engine).to_owned())).or_insert(root);
            if generation(root.name()) > generation(slot.name()) {
                *slot = root;
            }
        }
        chosen.into_iter().map(|((home, _), root)| self.view(&home, root)).collect()
    }
}

/// Reads `identity` again into the resident map: its current rows, or none once deleted.
fn reload(read: &mut Read<'_>, resident: &mut Resident, identity: &Identity) -> Result<(), StoreError> {
    if identity.kind != ROOT {
        return Ok(());
    }
    let key = (identity.home.clone(), identity.name.clone());
    match read.resolve(ROOT, &identity.name, &identity.home)?.map(|id| read.load::<Resource<ManifestRoot>>(id)).transpose()?.flatten() {
        Some(loaded) => resident.roots.insert(key, loaded.value.with_identity(&loaded.name, loaded.version)),
        None => resident.roots.remove(&key),
    };
    Ok(())
}

/// Reads every root homed at `home` into the resident map, in place of what it held.
fn rescan(read: &mut Read<'_>, resident: &mut Resident, home: &str) -> Result<(), StoreError> {
    resident.roots.retain(|(owner, _), _| owner != home);
    let mut after = None;
    loop {
        let page = read.scan::<Resource<ManifestRoot>>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<Resource<ManifestRoot>>(id)? {
                resident.roots.insert((home.to_owned(), loaded.name.clone()), loaded.value.with_identity(&loaded.name, loaded.version));
            }
        }
        after = page.next;
        if after.is_none() {
            return Ok(());
        }
    }
}

/// The old document key an engine document name encodes, if it is one.
fn key_of(name: &DocumentName) -> Option<DocumentKey> {
    serde_json::from_value(serde_json::Value::String(name.0.clone())).ok()
}

fn action(act: Act) -> ResolutionAction {
    match act {
        Act::Sync => ResolutionAction::Sync,
        Act::Adopt => ResolutionAction::Adopt,
    }
}

fn outcome(outcome: &Outcome) -> Option<ResolutionOutcome> {
    match outcome {
        Outcome::Requested => None,
        Outcome::Started => Some(ResolutionOutcome::Started),
        Outcome::Synced => Some(ResolutionOutcome::Synced),
        Outcome::Adopted => Some(ResolutionOutcome::Adopted),
        Outcome::Failed { reason } => Some(ResolutionOutcome::Failed(reason.clone())),
    }
}

/// The view of root `root`, declared as `name`: its records in the old resource's vocabulary,
/// with `judged` (the last pass's judgement, at the root's home) for each document it covers.
fn view(
    name: &str,
    local: bool,
    root: &Resource<ManifestRoot>,
    judged: Option<&BTreeMap<DocumentKey, DocumentState>>,
    now: chrono::DateTime<chrono::Utc>,
) -> RootView {
    let spec = root.spec();
    let state = root.state();
    let mut documents: BTreeMap<DocumentKey, DocumentState> = judged.cloned().unwrap_or_default();
    let names: BTreeSet<&DocumentName> =
        spec.baselines.keys().chain(spec.refusals.keys()).chain(spec.suspended.iter()).chain(spec.resolutions.keys()).collect();
    for document in names {
        let Some(key) = key_of(document) else { continue };
        let entry = documents.entry(key).or_insert_with(|| {
            let refusal = spec.refusals.get(document);
            DocumentState {
                phase: if refusal.is_some() {
                    DocumentPhase::Refused
                } else if spec.suspended.contains(document) {
                    DocumentPhase::Suspended
                } else {
                    DocumentPhase::Applied
                },
                reason: refusal.map(|refusal| refusal.reason.clone()),
                live_hash: None,
                desired_hash: None,
                baseline_hash: None,
                observed_at: refusal.and_then(|refusal| refusal.since.to_datetime()).unwrap_or(now),
                resolved_token: None,
                resolution_outcome: None,
            }
        });
        // The records are the decided state; the judgement only adds what they do not hold.
        entry.baseline_hash = spec.baselines.get(document).cloned();
        if let Some(resolution) = spec.resolutions.get(document) {
            let settled = outcome(&resolution.outcome);
            entry.resolved_token = settled.is_some().then(|| resolution.token.clone());
            entry.resolution_outcome = settled;
        }
    }
    let stalled = match (state.source_error(), state.source_failed_at()) {
        (Some(error), at) => Some(root_stall(name, error.to_owned(), at.and_then(|at| at.to_datetime()).unwrap_or(now))),
        (None, _) => {
            let refused: Vec<_> = spec.refusals.iter().filter_map(|(document, refusal)| Some((key_of(document)?, refusal))).collect();
            let evidence = refusal_evidence(refused.iter().map(|(key, refusal)| (key, refusal.reason.as_str())));
            let began = refused.iter().map(|(_, refusal)| refusal.since).min();
            evidence.map(|evidence| root_stall(name, evidence, began.and_then(|at| at.to_datetime()).unwrap_or(now)))
        }
    };
    RootView {
        name: name.to_owned(),
        local,
        spec: ManifestRootSpec {
            binding: state.binding().cloned(),
            host: spec.host.clone(),
            path: spec.path.clone(),
            source: spec.source.clone(),
            suspended: spec.suspended.iter().filter_map(key_of).collect(),
            resolutions: spec
                .resolutions
                .iter()
                .filter_map(|(document, record)| {
                    Some((
                        key_of(document)?,
                        Resolution { action: action(record.act), token: record.token.clone(), requested_by: record.by.clone() },
                    ))
                })
                .collect(),
        },
        status: ManifestRootStatus {
            applied_revision: state.applied().map(str::to_owned),
            source_error: state.source_error().map(str::to_owned),
            documents,
            stalled,
        },
    }
}

fn names(keys: impl IntoIterator<Item = DocumentKey>) -> Result<BTreeSet<String>, String> {
    keys.into_iter().map(|key| document_name(&key).map(|name| name.0)).collect()
}

#[async_trait::async_trait]
impl ManifestRoots for EngineManifestRoots {
    async fn root(&self, _namespace: &str, name: &str) -> Result<Option<RootView>, String> {
        let resident = self.refresh()?;
        let mut views = self.views(&resident, Some(name));
        // A root is declared by one host; prefer this home's copy where two hosts hold one.
        views.sort_by_key(|view| !view.local);
        Ok(views.into_iter().next())
    }

    async fn roots(&self, _namespace: &str) -> Result<Vec<RootView>, String> {
        let resident = self.refresh()?;
        Ok(self.views(&resident, None))
    }

    async fn declare(&self, namespace: &str, name: &str, declaration: RootDeclaration) -> Result<RootView, String> {
        if let Some(engine) = self.live(name) {
            let current = locked(&self.roots).get(&engine).cloned().expect("live root is held");
            let spec = current.spec();
            let moved = spec.host != declaration.host || spec.path != declaration.path || spec.source != declaration.source;
            if moved && !declaration.ops {
                return Err(if spec.host != declaration.host {
                    format!("ManifestRoot {name} belongs to another host")
                } else {
                    format!("ManifestRoot {name} has a conflicting declaration")
                });
            }
            if !moved {
                self.apply(&engine, Rebind { binding: declaration.binding.clone() })?;
                return self.root(namespace, name).await?.ok_or_else(|| format!("ManifestRoot {name} vanished"));
            }
            // An ops binding that moved its path or source is a new declaration: the request is
            // fixed, so the old root retires and the next generation opens.
            self.apply(&engine, Retire {})?;
        }
        let next = {
            let roots = locked(&self.roots);
            let used: Vec<u64> = roots.resources().filter(|root| base(root.name()) == name).map(|root| generation(root.name())).collect();
            match used.iter().max() {
                None => name.to_owned(),
                Some(last) => format!("{name}.{}", last + 1),
            }
        };
        let request = RootRequest {
            host: declaration.host.clone(),
            path: declaration.path.clone(),
            source: declaration.source.clone(),
            binding: declaration.binding.clone(),
        };
        locked(&self.roots)
            .create(next.clone(), request, &(), self.now())
            .map_err(|error| format!("declare ManifestRoot {name}: {error:?}"))?;
        self.root(namespace, name).await?.ok_or_else(|| format!("ManifestRoot {name} vanished"))
    }

    async fn retire(&self, _namespace: &str, name: &str) -> Result<(), String> {
        match self.live(name) {
            Some(engine) => self.apply(&engine, Retire {}).map(drop),
            None => Ok(()),
        }
    }

    async fn record_pass(&self, _namespace: &str, name: &str, pass: PassReport) -> Result<(), String> {
        let engine = self.live_or_missing(name)?;
        let refused = refusals(&pass);
        let applied = Applied {
            revision: pass.revision.clone(),
            complete: pass.complete,
            reached: names(pass.documents.keys().cloned())?,
            baselines: pass
                .documents
                .iter()
                .filter_map(|(key, document)| document.baseline_hash.as_ref().map(|hash| (key, hash)))
                .map(|(key, hash)| document_name(key).map(|name| (name.0, hash.clone())))
                .collect::<Result<_, _>>()?,
            refusals: refused
                .iter()
                .map(|(key, reason)| document_name(key).map(|name| (name.0, reason.clone())))
                .collect::<Result<_, _>>()?,
        };
        self.apply(&engine, applied)?;
        // Settle each resolution the pass claimed and finished.
        let resolutions = locked(&self.roots).get(&engine).map(|root| root.spec().resolutions.clone()).unwrap_or_default();
        for (key, document) in &pass.documents {
            let (Some(token), Some(finished)) = (&document.resolved_token, &document.resolution_outcome) else { continue };
            let settled = match finished {
                ResolutionOutcome::Started => continue,
                ResolutionOutcome::Synced => Outcome::Synced,
                ResolutionOutcome::Adopted => Outcome::Adopted,
                ResolutionOutcome::Failed(reason) => Outcome::Failed { reason: reason.clone() },
            };
            let document_key = document_name(key)?;
            let claimed = resolutions.get(&document_key).is_some_and(|record| record.token == *token && record.outcome == Outcome::Started);
            if claimed {
                self.apply(&engine, Resolve { document: document_key.0, token: token.clone(), outcome: settled })?;
            }
        }
        locked(&self.judged).insert(engine, pass.documents);
        Ok(())
    }

    async fn record_source(&self, _namespace: &str, name: &str, revision: Option<String>, error: Option<&str>) -> Result<(), String> {
        let engine = self.live_or_missing(name)?;
        match (error, revision) {
            (Some(error), _) => self.apply(&engine, SourceFailed { error: error.to_owned() }).map(drop),
            // A source with no documents of its own loaded whole: a complete pass that reached none.
            (None, Some(revision)) => self
                .apply(
                    &engine,
                    Applied { revision, complete: true, reached: BTreeSet::new(), baselines: BTreeMap::new(), refusals: BTreeMap::new() },
                )
                .map(drop),
            (None, None) => Ok(()),
        }
    }

    async fn claim_resolution(&self, _namespace: &str, name: &str, key: &DocumentKey, token: &str) -> Result<bool, String> {
        let engine = self.live_or_missing(name)?;
        let edge = Resolve { document: document_name(key)?.0, token: token.to_owned(), outcome: Outcome::Started };
        match locked(&self.roots).apply(&engine, edge, &(), self.now()) {
            Ok(_) => Ok(true),
            Err(WriteError::Refused(RootRefusal::NoRequest | RootRefusal::Settled | RootRefusal::Unchanged)) => Ok(false),
            Err(WriteError::Refused(refusal)) => Err(format!("ManifestRoot {name} refused the claim: {refusal:?}")),
            Err(other) => Err(format!("ManifestRoot {name}: {other:?}")),
        }
    }

    async fn request_resolution(
        &self,
        _namespace: &str,
        name: &str,
        key: &DocumentKey,
        action: ResolutionAction,
        requested_by: &str,
    ) -> Result<String, String> {
        let engine = self.live_or_missing(name)?;
        let token = uuid::Uuid::new_v4().to_string();
        let act = match action {
            ResolutionAction::Sync => Act::Sync,
            ResolutionAction::Adopt => Act::Adopt,
        };
        let by = if requested_by.is_empty() { "unknown".to_owned() } else { requested_by.to_owned() };
        self.apply(&engine, RequestResolution { document: document_name(key)?.0, act, token: token.clone(), by })?;
        Ok(token)
    }

    async fn set_suspended(&self, _namespace: &str, name: &str, key: &DocumentKey, suspended: bool) -> Result<(), String> {
        let engine = self.live_or_missing(name)?;
        let document = document_name(key)?.0;
        if suspended {
            self.apply(&engine, Suspend { document }).map(drop)
        } else {
            self.apply(&engine, Unsuspend { document }).map(drop)
        }
    }
}
