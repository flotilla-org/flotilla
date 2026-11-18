//! The `[primitives]` switch's Convoy path, as old-store code reaches it
//! (#3587 layer 6).
//!
//! With the switch on, the engine Convoy decides (owner ruling, 2026-10-22,
//! layer 6 question 1): crew verbs and observers write the engine through
//! [`EngineConvoys`] and return its refusals. Readers see a convoy through
//! [`viewed_read`] (layer 8): the engine's promises in the old shape, read keyed
//! at the convoy's home, when the engine holds it, else the stored status's.
//! One set of views serves the switch on or off, and no old status is
//! written for the engine's promises.
//!
//! The port speaks the old shape: a crew is `(vessel, role)` and a promise is
//! its old id, so callers keep their validation and the implementation owns
//! the mapping onto seats and labels. A convoy the engine does not hold (one
//! admitted before the switch, until the layer 9 import) stays on the old
//! path: [`EngineConvoys::holds`] is the one question callers ask first.
//! Absent ([`EngineConvoyPath`] unset), nothing changes.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, OnceLock},
};

use async_trait::async_trait;

use super::{promises::PromiseOperation, Convoy};
use crate::{ReadResourceObject, ResourceObject, ResourceProvenance};

/// Who makes a promise write, which the engine's authority rules read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    /// The owning crew, through its own verb: the owner seat.
    Crew,
    /// An observer recording what it saw: the kind's evidence, or branch
    /// discovery on the owner's behalf.
    Observer,
    /// An operator's attributed verdict.
    Operator { principal: String },
}

/// Which seats an operator's verdict covers: one crew, or every crew of a
/// vessel (the old `convoy … work … complete --force`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictScope {
    pub vessel: String,
    pub role: Option<String>,
}

/// The engine-first Convoy writer, in the old shape.
#[async_trait]
pub trait EngineConvoys: Send + Sync {
    /// Whether the engine decides this convoy. An invalid engine identity also
    /// selects this path so an operation reports its error instead of using the old writer.
    fn holds(&self, convoy: &ResourceObject<Convoy>) -> bool;

    /// Reserves the next launch number of `launch_of` at this home and returns the launch's name
    /// (`<launch_of>#<n>`), so admission keys what it writes by the launch before creating it.
    async fn reserve(&self, launch_of: &str) -> Result<String, String>;

    /// Admits `convoy`, named by a launch [`EngineConvoys::reserve`] returned, as admission froze
    /// it: its spec, the status admission seeds (workflow snapshot, placement, ensure, naming) and
    /// the annotations naming what admission wrote for it. The machine's refusal is the `Err`.
    async fn admit_reserved(&self, convoy: &ResourceObject<Convoy>) -> Result<(), String>;

    /// One promise operation by `(vessel, role)`'s seat, as the old patch
    /// would have carried it. Refusals come back as `Err`.
    async fn promise(
        &self,
        convoy: &ResourceObject<Convoy>,
        vessel: &str,
        role: &str,
        operation: &PromiseOperation,
        actor: Actor,
    ) -> Result<(), String>;

    /// Handoff from crew `from` to crew `to`, each `(vessel, role)`: the open promises of `from`
    /// named by their ids in `promises` move to `to`, and no other. A named promise that is not
    /// `from`'s, or not open, refuses the whole reassignment.
    async fn reassign(
        &self,
        convoy: &ResourceObject<Convoy>,
        from: (&str, &str),
        to: (&str, &str),
        promises: &[String],
    ) -> Result<(), String>;

    /// An operator keeps every open promise in `scope` by verdict, with a
    /// reason; returns the labels kept.
    async fn keep_by_verdict(
        &self,
        convoy: &ResourceObject<Convoy>,
        scope: &VerdictScope,
        principal: &str,
        reason: &str,
    ) -> Result<Vec<String>, String>;

    /// An operator keeps or retracts one promise by its label, with a reason.
    async fn verdict(&self, convoy: &ResourceObject<Convoy>, label: &str, keep: bool, principal: &str, reason: &str) -> Result<(), String>;

    /// Landing's delivery as `disposition`, the exit-table entry settlement chose (claim, merged,
    /// closed-unmerged or a workflow's declared name): refused while anything is open.
    async fn deliver(&self, convoy: &ResourceObject<Convoy>, disposition: &str) -> Result<(), String>;

    /// The disposition the engine delivered `convoy` with, once it has: the convoy's outcome is
    /// decided, and nothing the old path observes afterwards (a work failing as its vessels find
    /// the convoy ended) changes it. `None` while it is open, or not held here.
    fn delivered(&self, convoy: &ResourceObject<Convoy>) -> Option<String>;

    /// An operator's abandonment, with a reason.
    async fn abandon(&self, convoy: &ResourceObject<Convoy>, principal: &str, reason: &str) -> Result<(), String>;

    /// Ends `convoy` as failed, as `principal` judged its work, with a reason. An ended convoy is
    /// left as it is.
    async fn fail(&self, convoy: &ResourceObject<Convoy>, principal: &str, reason: &str) -> Result<(), String>;

    /// `(vessel, role)`'s seat failed, as `principal` ruled, with a reason.
    async fn fail_seat(
        &self,
        convoy: &ResourceObject<Convoy>,
        vessel: &str,
        role: &str,
        principal: &str,
        reason: &str,
    ) -> Result<(), String>;

    /// Links `subject`, as `source` found it, on behalf of `principal`; a subject linked now is
    /// left as it is.
    async fn link(
        &self,
        convoy: &ResourceObject<Convoy>,
        subject: &flotilla_protocol::Subject,
        source: super::SubjectDiscoverySource,
        principal: &str,
    ) -> Result<(), String>;

    /// Unlinks `subject` on behalf of `principal`; a subject not linked now is left as it is.
    async fn unlink(&self, convoy: &ResourceObject<Convoy>, subject: &flotilla_protocol::Subject, principal: &str) -> Result<(), String>;

    /// The engine's promises of `convoy` in the old status's shape:
    /// `Ok(Some)` when the engine at `home` (the convoy's home; `None` for
    /// this one) holds it, `Ok(None)` when it does not, and `Err` when the
    /// read failed. One keyed read: this home's records are in memory and
    /// never fail; another home's is its replica in the decided store.
    fn promises(&self, convoy: &ResourceObject<Convoy>, home: Option<&str>) -> Result<Option<OldPromises>, String>;

    /// The names of this home's convoys as their engine records change. No
    /// old record is written for an engine edge, so the convoy reconciler
    /// follows these to run again (#3587 layer 8).
    fn changes(&self) -> tokio::sync::broadcast::Receiver<String>;

    /// This home's notes on the launches it holds: what it observes of them, in memory.
    fn memory(&self) -> &crate::convoy_records::ConvoyMemory;
}

/// Writes `patch` to this home's notes on `convoy` (#3108 C3'): the old status patch, kept only
/// where it changes what the home observes ([`ConvoyNotes::apply`](crate::convoy_records::ConvoyNotes::apply)).
/// Returns `convoy` with the notes as they now stand, which its view shows once it follows them.
/// A convoy held at another home is refused: its notes are that home's.
pub fn note<P: crate::StatusPatch<super::ConvoyStatus> + ?Sized>(
    path: &EngineConvoyPath,
    convoy: &ResourceObject<Convoy>,
    patch: &P,
) -> Result<ResourceObject<Convoy>, String> {
    let engine = path.get().ok_or(NO_ENGINE)?;
    if !engine.holds(convoy) {
        return Err(format!("convoy `{}` is not held at this home; its notes are its home's", convoy.metadata.name));
    }
    let mut status = convoy.status.clone().unwrap_or_default();
    let (notes, _) = engine.memory().update(&convoy.metadata.name, |notes| {
        notes.apply(&status, patch);
        notes.clone()
    });
    notes.write_into(&mut status);
    let mut noted = convoy.clone();
    noted.status = Some(status);
    Ok(noted)
}

/// Sets this home's notes on convoy `name` to those `status` holds: a view read before a write
/// that failed to finish, put back, or one a writer changed whole.
pub fn set_notes(path: &EngineConvoyPath, name: &str, status: &super::ConvoyStatus) -> Result<(), String> {
    let engine = path.get().ok_or(NO_ENGINE)?;
    engine.memory().update(name, |notes| notes.read_from(status));
    Ok(())
}

/// The old status's promises: by vessel, then role.
pub type OldPromises = BTreeMap<String, BTreeMap<String, Vec<super::promises::Promise>>>;

/// The composition root's slot for [`EngineConvoys`]: set once, at startup.
pub type EngineConvoyPath = Arc<OnceLock<Arc<dyn EngineConvoys>>>;

/// The refusal of a convoy read or write on a path not yet installed.
pub const NO_ENGINE: &str = "convoys are not served without the engine";

/// The engine path for `convoy`, when the switch is on and the engine holds it.
pub fn deciding<'a>(path: &'a EngineConvoyPath, convoy: &ResourceObject<Convoy>) -> Option<&'a Arc<dyn EngineConvoys>> {
    path.get().filter(|engine| engine.holds(convoy))
}

/// A convoy as its readers see it (#3587 layer 8), read at its home (a
/// replica's origin home): with the engine's promises in place of the stored
/// ones when the engine holds it, else as stored. A failed read, or one before
/// the path is installed, is an error, never the stored record: an engine-held
/// convoy's stored promises were handed over, and would read as none.
pub fn viewed_read(path: &EngineConvoyPath, read: &ReadResourceObject<Convoy>) -> Result<ResourceObject<Convoy>, String> {
    let home = match &read.provenance {
        ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.as_str()),
        _ => None,
    };
    let held = path.get().ok_or(NO_ENGINE)?.promises(&read.object, home)?;
    Ok(match held {
        Some(promises) => with_promises(&read.object, promises),
        None => read.object.clone(),
    })
}

fn with_promises(convoy: &ResourceObject<Convoy>, promises: OldPromises) -> ResourceObject<Convoy> {
    let mut viewed = convoy.clone();
    viewed.status.get_or_insert_with(Default::default).promises = promises;
    viewed
}

/// The holder of `convoy`'s checkouts: the engine launch its generation became at admission
/// (`host:<home>/convoy:<launch_of>%23<n>` once stored), never a predecessor's or successor's.
/// `None` until the engine admits it, when nothing is held for it.
pub fn checkout_holder(convoy: &ResourceObject<Convoy>) -> Option<crate::checkout_records::CheckoutHolder> {
    let launch = convoy.status.as_ref()?.engine_launch.clone()?;
    Some(crate::checkout_records::CheckoutHolder::Convoy(launch))
}

/// Stable engine identity of a standing role; independent of its old generation record.
pub fn standing_name(project: &str, role: &str) -> Result<String, String> {
    flotilla_address::Address::project(project)
        .and_then(|project| project.child(flotilla_address::Kind::Convoy, role))
        .map(|address| address.to_string())
        .map_err(|error| error.to_string())
}

/// What launch `name` is a launch of, and its number; `None` for a name that is no launch.
pub fn split_launch(name: &str) -> Option<(&str, u64)> {
    let (of, n) = name.rsplit_once('#')?;
    Some((of, n.parse().ok()?))
}

/// What `name` is a launch of: the name less its `#<n>`, or the name itself when it is no launch.
pub fn launch_of(name: &str) -> &str {
    split_launch(name).map_or(name, |(of, _)| of)
}

/// Launch `launch`'s host-local, path-safe name, for worktree directories, terminal sessions and
/// other names on a host: `<launch_of, path-safe>-<n>`. Two launches at one home never share one,
/// because a home numbers its launches from one sequence. Records and addresses name the launch
/// itself; a name that is no launch is made path-safe as it is.
pub fn launch_slug(launch: &str) -> String {
    match split_launch(launch) {
        Some((of, n)) => format!("{}-{n}", crate::checkout_path::checkout_path_component(of)),
        None => crate::checkout_path::checkout_path_component(launch),
    }
}

/// The address of launch `launch` of a convoy in `project`. A standing role's launches are of its
/// address (`project:P/convoy:R`), so the launch is that role's segment with its number
/// (`project:P/convoy:R%234`); a finite convoy's is the launch under the project.
pub fn launch_address(project: &flotilla_address::Address, launch: &str) -> Result<flotilla_address::Address, String> {
    let (of, n) = split_launch(launch).ok_or_else(|| format!("`{launch}` names no launch"))?;
    let address = match of.parse::<flotilla_address::Address>() {
        Ok(standing) if standing.kind() == flotilla_address::Kind::Convoy => {
            let parent = standing.parent().ok_or_else(|| format!("standing convoy `{of}` has no project"))?;
            parent.child(flotilla_address::Kind::Convoy, &format!("{}#{n}", standing.name()))
        }
        _ => project.child(flotilla_address::Kind::Convoy, launch),
    };
    address.map_err(|error| error.to_string())
}

/// The inbox of `role` in convoy `convoy` of `project`: `project:P/convoy:C/role:R`, with a
/// launch's convoy as [`launch_address`] names it. A crew member's vessel takes no part.
pub fn seat_address(project: &flotilla_address::Address, convoy: &str, role: &str) -> Result<flotilla_address::Address, String> {
    convoy_address_in(project, convoy)?.child(flotilla_address::Kind::Role, role).map_err(|error| error.to_string())
}

/// Convoy record `convoy` under `project`: a launch at its [`launch_address`], any other name as
/// the project's child.
fn convoy_address_in(project: &flotilla_address::Address, convoy: &str) -> Result<flotilla_address::Address, String> {
    match split_launch(convoy) {
        Some(_) => launch_address(project, convoy),
        None => project.child(flotilla_address::Kind::Convoy, convoy).map_err(|error| error.to_string()),
    }
}

/// The inbox a crew role address names: `P/C/V/R` is the convoy role `project:P/convoy:C/role:R`
/// (the vessel dropped, the segment's escapes undone), and `P/R` the standing role
/// `project:P/role:R`.
pub fn role_inbox(receiver: &str) -> Result<flotilla_address::Address, String> {
    let project = |project: &str| flotilla_address::Address::project(project).map_err(|error| format!("`{receiver}`: {error}"));
    match receiver.split('/').collect::<Vec<_>>().as_slice() {
        [name, segment, _vessel, role] => {
            seat_address(&project(name)?, &crate::convoy_of_segment(segment), role).map_err(|error| format!("`{receiver}`: {error}"))
        }
        [name, role] => project(name)?.child(flotilla_address::Kind::Role, role).map_err(|error| format!("`{receiver}`: {error}")),
        _ => Err(format!("`{receiver}` is not a role address")),
    }
}

/// The convoy records the convoy an address names may be, by name: a standing role's launch
/// (`project:P/convoy:R#n`, named by its address) first, then the convoy its segment names.
pub fn convoy_records(address: &flotilla_address::Address) -> Vec<String> {
    let Some(segment) = address.get(flotilla_address::Kind::Convoy) else { return Vec::new() };
    let standing = split_launch(segment).and_then(|(of, n)| {
        let project = address.get(flotilla_address::Kind::Project)?;
        Some(format!("{}#{n}", standing_name(project, of).ok()?))
    });
    standing.into_iter().chain([segment.to_owned()]).collect()
}

/// The address a message refers to convoy record `name` by, under `project` (the project it
/// declares, else its namespace's): a launch has one address, its [`launch_address`], which its
/// seats are under (#3108 R4).
pub fn convoy_reference_address(name: &str, project: &str) -> Result<flotilla_address::Address, String> {
    flotilla_address::Address::project(project)
        .map_err(|error| error.to_string())
        .and_then(|project| convoy_address_in(&project, name))
        .map_err(|error| format!("convoy `{name}`: {error}"))
}

/// The address a message refers to `convoy` by: [`convoy_reference_address`] under the project it
/// declares, else its namespace's, as its seats are.
pub fn convoy_address(convoy: &ResourceObject<Convoy>) -> Result<flotilla_address::Address, String> {
    convoy_reference_address(&convoy.metadata.name, convoy.spec.project_ref.as_deref().unwrap_or(&convoy.metadata.namespace))
}

/// What `convoy` is a launch of. A launch read from the engine is named by its launch. An old
/// record (the import's) keeps its name when finite; a standing one uses its ensure's frozen role
/// and project, including old generations admitted before spec.role.
pub fn engine_name(convoy: &ResourceObject<Convoy>) -> Result<String, String> {
    if let Some((of, _)) = split_launch(&convoy.metadata.name) {
        return Ok(of.to_owned());
    }
    if !super::reconcile::is_ensured(convoy) {
        return Ok(convoy.metadata.name.clone());
    }
    let role = convoy
        .status
        .as_ref()
        .and_then(|status| status.ensure_admission.as_ref())
        .map(|ensure| ensure.role.as_str())
        .filter(|role| !role.is_empty())
        .or_else(|| (!convoy.spec.role.is_empty()).then_some(convoy.spec.role.as_str()))
        .ok_or_else(|| format!("standing convoy `{}` has no recorded ensure role", convoy.metadata.name))?;
    standing_name(convoy.spec.project_ref.as_deref().unwrap_or(&convoy.metadata.namespace), role)
}

/// The launch an old convoy record imports as (#3108): launch `n` of its [`engine_name`], at the
/// home the record is homed at, in the project it is addressed under (its `project_ref`, else its
/// namespace).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedLaunch {
    pub home: String,
    pub project: String,
    pub of: String,
    pub n: u64,
}

impl ImportedLaunch {
    /// Its name, `<of>#<n>`, as the engine names a launch.
    pub fn name(&self) -> String {
        format!("{}#{}", self.of, self.n)
    }
}

/// Why an old convoy record imports as no launch: what a record naming it is refused for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoLaunch {
    /// It never bootstrapped, so the engine holds no launch of it.
    NeverBootstrapped,
    /// This host's old store holds no record of it.
    NotHeld,
    /// Its name maps to no launch name.
    Unnamed(String),
}

impl NoLaunch {
    /// The reason's class, as a tally counts it.
    pub fn class(&self) -> &'static str {
        match self {
            Self::NeverBootstrapped => "names a convoy that never bootstrapped",
            Self::NotHeld => "names a convoy not in this host's replica",
            Self::Unnamed(_) => "names a convoy whose name maps to no launch",
        }
    }

    /// The class of the reason a refusal gives, when it is refused for naming a convoy that
    /// imports as no launch: what the import tallies its refusals by.
    pub fn class_in(reason: &str) -> Option<&'static str> {
        [Self::NeverBootstrapped, Self::NotHeld, Self::Unnamed(String::new())].iter().map(Self::class).find(|class| reason.contains(class))
    }
}

impl std::fmt::Display for NoLaunch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unnamed(reason) => write!(f, "{}: {reason}", self.class()),
            _ => f.write_str(self.class()),
        }
    }
}

/// Two old records of one home that would import as the same launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchCollision {
    pub home: String,
    pub launch: String,
    pub records: Vec<String>,
}

/// The launch each old convoy record imports as, by namespace and record name: the one mapping
/// every importer names a convoy by (#3108). The number comes from the convoy itself
/// ([`Self::number`]), so any host computes any home's launches from the records it holds.
#[derive(Debug, Clone, Default)]
pub struct ImportedLaunches {
    launches: BTreeMap<(String, String), ImportedLaunch>,
    /// Every record numbered, by namespace and name, and why one imports as no launch.
    unlaunched: BTreeMap<(String, String), NoLaunch>,
    /// The highest number each home's records take, launched or not.
    highest: BTreeMap<String, u64>,
    collisions: Vec<LaunchCollision>,
}

impl ImportedLaunches {
    /// Numbers `records`, each with its home. A standing role's generation is its role's launch
    /// numbered by the generation, as a live generation is; any other convoy is launch 1 of its
    /// own name. A standing generation that never bootstrapped keeps its number, with no launch.
    pub fn number<'a>(records: impl IntoIterator<Item = (&'a str, &'a ResourceObject<Convoy>)>) -> Self {
        let mut this = Self::default();
        let mut named = BTreeMap::<(String, String), BTreeSet<String>>::new();
        for (home, convoy) in records {
            let key = (convoy.metadata.namespace.clone(), convoy.metadata.name.clone());
            let of = match engine_name(convoy) {
                Ok(of) => of,
                Err(reason) => {
                    this.unlaunched.insert(key, NoLaunch::Unnamed(reason));
                    continue;
                }
            };
            let n = if super::reconcile::is_ensured(convoy) { convoy.spec.generation } else { 1 };
            let highest = this.highest.entry(home.to_owned()).or_default();
            *highest = (*highest).max(n);
            let launch = ImportedLaunch {
                home: home.to_owned(),
                project: convoy.spec.project_ref.clone().unwrap_or_else(|| convoy.metadata.namespace.clone()),
                of,
                n,
            };
            named.entry((home.to_owned(), launch.name())).or_default().insert(convoy.metadata.name.clone());
            if convoy.status.as_ref().is_some_and(|status| status.workflow_snapshot.is_some()) {
                this.launches.insert(key, launch);
            } else {
                this.unlaunched.insert(key, NoLaunch::NeverBootstrapped);
            }
        }
        this.collisions = named
            .into_iter()
            .filter(|(_, records)| records.len() > 1)
            .map(|((home, launch), records)| LaunchCollision { home, launch, records: records.into_iter().collect() })
            .collect();
        this
    }

    /// These launches with record `record` in `namespace` importing as `launch`.
    pub fn with(mut self, namespace: &str, record: &str, launch: ImportedLaunch) -> Self {
        let highest = self.highest.entry(launch.home.clone()).or_default();
        *highest = (*highest).max(launch.n);
        self.launches.insert((namespace.to_owned(), record.to_owned()), launch);
        self
    }

    /// Why record `record` in `namespace` imports as no launch; nothing when it imports as one.
    pub fn missing(&self, namespace: &str, record: &str) -> Option<NoLaunch> {
        let key = (namespace.to_owned(), record.to_owned());
        if self.launches.contains_key(&key) {
            return None;
        }
        Some(self.unlaunched.get(&key).cloned().unwrap_or(NoLaunch::NotHeld))
    }

    /// The records of one home that would import as one launch: each refuses the import.
    pub fn collisions(&self) -> &[LaunchCollision] {
        &self.collisions
    }

    /// The highest launch number `home`'s records take: a launch made there after the import
    /// takes a higher one.
    pub fn highest(&self, home: &str) -> u64 {
        self.highest.get(home).copied().unwrap_or_default()
    }

    /// The launch record `record` in `namespace` imports as.
    pub fn get(&self, namespace: &str, record: &str) -> Option<&ImportedLaunch> {
        self.launches.get(&(namespace.to_owned(), record.to_owned()))
    }

    /// [`Self::get`]'s launch name.
    pub fn name(&self, namespace: &str, record: &str) -> Option<String> {
        self.get(namespace, record).map(ImportedLaunch::name)
    }

    /// [`Self::name`], or why record `record` in `namespace` imports as no launch.
    pub fn launch(&self, namespace: &str, record: &str) -> Result<String, NoLaunch> {
        self.name(namespace, record).ok_or_else(|| self.missing(namespace, record).unwrap_or(NoLaunch::NotHeld))
    }

    /// `address`, written against the old store in `namespace`, with the convoy it names as the
    /// launch that convoy imports as, at the launch's address ([`launch_address`]) in its
    /// project, as the import writes it, whether or not the old address named the project: the
    /// convoy itself has that one address too (#3108 R4, [`convoy_reference_address`]). An
    /// artifact about that convoy is about the launch, at the project-free convoy address artifacts
    /// take ([`crate::address::artifact_address`]). An address naming no convoy is unchanged, and
    /// one naming a convoy that imports as no launch is refused: an imported record names a convoy
    /// only by its launch.
    pub fn address(&self, namespace: &str, address: &flotilla_address::Address) -> Result<flotilla_address::Address, String> {
        use flotilla_address::{Address, Kind};
        let segments = address.segments();
        let Some(at) = segments.iter().position(|segment| segment.kind() == Kind::Convoy) else { return Ok(address.clone()) };
        let record = segments[at].value();
        let launch = self.get(namespace, record).ok_or_else(|| {
            let reason = self.missing(namespace, record).unwrap_or(NoLaunch::NotHeld);
            format!("`{address}` {reason} (`{record}`)")
        })?;
        let name = launch.name();
        let unaddressable = |error: String| format!("`{address}` as launch `{name}`: {error}");
        let artifact = segments.iter().any(|segment| segment.kind() == Kind::Artifact);
        let mut mapped = match &segments[..at] {
            [] if artifact => Address::convoy(&name).map_err(|error| error.to_string()),
            [] => Address::project(&launch.project).map_err(|error| error.to_string()).and_then(|project| launch_address(&project, &name)),
            [project] if project.kind() == Kind::Project && project.value() == launch.project => {
                Address::project(project.value()).map_err(|error| error.to_string()).and_then(|project| launch_address(&project, &name))
            }
            _ => Err(format!("the convoy is not under its project `{}`", launch.project)),
        }
        .map_err(unaddressable)?;
        for segment in &segments[at + 1..] {
            mapped = match (segment.kind(), address.of()) {
                (Kind::Artifact, Some(of)) => mapped.artifact(segment.value(), if of == record { &name } else { of }),
                (kind, _) => mapped.child(kind, segment.value()),
            }
            .map_err(|error| unaddressable(error.to_string()))?;
        }
        Ok(mapped)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::Utc;

    use super::*;
    use crate::{
        promises::{Promise, PromiseKind, PromiseSource, PromiseState},
        ConvoySpec, ConvoyStatus, ObjectMeta, WorkflowSnapshot,
    };

    /// An engine holding one convoy at home `here`, and a replica of another
    /// home's, recording the homes it was asked about.
    struct Holding {
        asked: Mutex<Vec<Option<String>>>,
        memory: crate::convoy_records::ConvoyMemory,
    }

    fn kept() -> OldPromises {
        let promise = Promise {
            id: "template/pr".into(),
            vessel: "work".into(),
            role: "coder".into(),
            source: PromiseSource::Template,
            kind: PromiseKind::Pr,
            state: PromiseState::Kept,
            retraction_reason: None,
            submissions: Vec::new(),
        };
        [("work".to_owned(), [("coder".to_owned(), vec![promise])].into())].into()
    }

    #[async_trait]
    impl EngineConvoys for Holding {
        fn holds(&self, _: &ResourceObject<Convoy>) -> bool {
            true
        }
        async fn reserve(&self, launch_of: &str) -> Result<String, String> {
            Ok(format!("{launch_of}#1"))
        }
        async fn admit_reserved(&self, _: &ResourceObject<Convoy>) -> Result<(), String> {
            Ok(())
        }
        async fn promise(&self, _: &ResourceObject<Convoy>, _: &str, _: &str, _: &PromiseOperation, _: Actor) -> Result<(), String> {
            Ok(())
        }
        async fn reassign(&self, _: &ResourceObject<Convoy>, _: (&str, &str), _: (&str, &str), _: &[String]) -> Result<(), String> {
            Ok(())
        }
        async fn keep_by_verdict(&self, _: &ResourceObject<Convoy>, _: &VerdictScope, _: &str, _: &str) -> Result<Vec<String>, String> {
            Ok(Vec::new())
        }
        async fn verdict(&self, _: &ResourceObject<Convoy>, _: &str, _: bool, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn deliver(&self, _: &ResourceObject<Convoy>, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn delivered(&self, _: &ResourceObject<Convoy>) -> Option<String> {
            None
        }
        async fn fail(&self, _: &ResourceObject<Convoy>, _: &str, _: &str) -> Result<(), String> {
            unreachable!()
        }
        async fn abandon(&self, _: &ResourceObject<Convoy>, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn promises(&self, convoy: &ResourceObject<Convoy>, home: Option<&str>) -> Result<Option<OldPromises>, String> {
            self.asked.lock().expect("asked").push(home.map(str::to_owned));
            match convoy.metadata.name.as_str() {
                "unheld" => Ok(None),
                "unreadable" => Err("replica read failed".into()),
                _ => Ok(Some(kept())),
            }
        }
        fn changes(&self) -> tokio::sync::broadcast::Receiver<String> {
            tokio::sync::broadcast::channel(1).1
        }

        fn memory(&self) -> &crate::convoy_records::ConvoyMemory {
            &self.memory
        }

        async fn fail_seat(&self, _: &ResourceObject<Convoy>, _: &str, _: &str, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }

        async fn link(
            &self,
            _: &ResourceObject<Convoy>,
            _: &flotilla_protocol::Subject,
            _: crate::convoy::SubjectDiscoverySource,
            _: &str,
        ) -> Result<(), String> {
            Ok(())
        }

        async fn unlink(&self, _: &ResourceObject<Convoy>, _: &flotilla_protocol::Subject, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    // Behaviour: two launches at one home have different numbers, so their slugs differ even
    // where their launch_of slugs coincide (`project:x-convoy/convoy:y` and
    // `project:x/convoy:convoy-y` both slug as `project-x-convoy-convoy-y`).
    #[test]
    fn launches_at_one_home_never_share_a_slug() {
        let launches =
            ["project:x-convoy/convoy:y#4", "project:x/convoy:convoy-y#5", "convoy-0f3a#6", "convoy-0f3a-6#7", "fix#8", "fix-8#9"];
        let slugs: std::collections::BTreeSet<_> = launches.iter().map(|launch| launch_slug(launch)).collect();
        assert_eq!(slugs.len(), launches.len(), "{slugs:?}");
        assert_eq!(launch_slug("project:p/convoy:coder#12"), "project-p-convoy-coder-12");
        for slug in &slugs {
            assert!(slug.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')), "{slug}");
        }
    }

    // Behaviour: a standing launch's address is its role's segment with the launch number; a
    // finite one's is the launch under the project. Neither is encoded twice.
    #[test]
    fn a_launch_address_names_the_launch_once() {
        let project = flotilla_address::Address::project("p").expect("project");
        let standing = standing_name("p", "coder").expect("standing");
        assert_eq!(launch_address(&project, &format!("{standing}#4")).expect("standing").as_str(), "project:p/convoy:coder%234");
        assert_eq!(launch_address(&project, "convoy-0f3a#6").expect("finite").as_str(), "project:p/convoy:convoy-0f3a%236");
        assert!(launch_address(&project, "convoy-0f3a").is_err(), "no launch");
    }

    // Behaviour: a seat's inbox names its convoy as its launch's address, and the convoy records
    // that inbox may be under include the launch it was built from.
    #[test]
    fn a_seat_inbox_leads_back_to_its_convoy_record() {
        let project = flotilla_address::Address::project("p").expect("project");
        let standing = format!("{}#4", standing_name("p", "coder").expect("standing"));
        for convoy in [standing.as_str(), "convoy-0f3a#6", "convoy-0f3a"] {
            let seat = seat_address(&project, convoy, "coder").expect("seat");
            assert_eq!(seat.name(), "coder");
            let parent = seat.parent().expect("convoy");
            assert!(convoy_records(&parent).iter().any(|record| record == convoy), "{convoy}: {:?}", convoy_records(&parent));
        }
        assert_eq!(seat_address(&project, &standing, "coder").expect("seat").as_str(), "project:p/convoy:coder%234/role:coder");
    }

    fn convoy(name: &str) -> ResourceObject<Convoy> {
        ResourceObject {
            metadata: ObjectMeta {
                name: name.into(),
                namespace: "flotilla".into(),
                resource_version: "1".into(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: Utc::now(),
                merge: None,
            },
            spec: ConvoySpec::builder().workflow_ref("w".to_string()).build(),
            status: Some(ConvoyStatus::default()),
        }
    }

    // Behaviour (#3587 layer 8): readers see the engine's promises in place of the stored ones when
    // the engine holds the convoy, asked at the convoy's own home; for a convoy the engine does not
    // hold, they see the record as stored. Before the path is installed, a read is refused.
    #[test]
    fn readers_see_the_engines_promises_only_where_it_holds_the_convoy() {
        let local = |name: &str| ReadResourceObject { object: convoy(name), provenance: ResourceProvenance::Local };
        let uninstalled = EngineConvoyPath::default();
        assert_eq!(viewed_read(&uninstalled, &local("c")).expect_err("uninstalled: refused"), NO_ENGINE);

        let engine = Arc::new(Holding { asked: Mutex::new(Vec::new()), memory: Default::default() });
        let on = EngineConvoyPath::default();
        let shared: Arc<dyn EngineConvoys> = engine.clone();
        assert!(on.set(shared).is_ok());
        assert_eq!(viewed_read(&on, &local("c")).expect("read").status.expect("status").promises, kept());
        assert!(viewed_read(&on, &local("unheld")).expect("read").status.expect("status").promises.is_empty(), "not held: as stored");

        let replica = ReadResourceObject {
            object: convoy("c"),
            provenance: ResourceProvenance::Replica { origin_root: flotilla_protocol::NodeId::new("home-b"), last_synced_at: Utc::now() },
        };
        assert_eq!(viewed_read(&on, &replica).expect("read").status.expect("status").promises, kept());
        assert_eq!(*engine.asked.lock().expect("asked"), vec![None, None, Some("home-b".to_owned())]);
        // A failed replica read is an error, never the stored record (whose promises were handed over).
        let unreadable = ReadResourceObject { object: convoy("unreadable"), provenance: replica.provenance.clone() };
        assert_eq!(viewed_read(&on, &unreadable).expect_err("failed"), "replica read failed");
    }

    // Behaviour (#3108 R1, R2): a record's launch number comes from the convoy: an ordinary convoy
    // is #1 of its own name and a standing generation its role's launch numbered by its
    // generation, bootstrapped or not. Only a bootstrapped one names a launch; the others say why,
    // as does a convoy this host does not hold. A home's highest number counts every generation,
    // and two records of one home taking one launch collide.
    #[test]
    fn a_records_launch_is_numbered_by_the_convoy_itself() {
        let bootstrapped = |mut convoy: ResourceObject<Convoy>| {
            convoy.status.get_or_insert_with(Default::default).workflow_snapshot = Some(WorkflowSnapshot {
                cascade: None,
                exit: None,
                turn_delivery: Default::default(),
                stall_nudges: Default::default(),
                supervision: None,
                vessels: Vec::new(),
            });
            convoy
        };
        let generation = |name: &str, generation: u64| {
            let mut convoy = convoy(name);
            convoy.spec.role = "governor".into();
            convoy.spec.generation = generation;
            convoy.metadata.annotations.insert(crate::ENSURED_FROM_ANNOTATION.into(), "standing-governor".into());
            convoy
        };
        let records = [
            ("feta", bootstrapped(convoy("demo"))),
            ("feta", convoy("unstarted")),
            ("feta", bootstrapped(generation("governor-300", 300))),
            ("feta", generation("governor-301", 301)),
            ("feta", bootstrapped(generation("governor-again", 300))),
            ("udder", bootstrapped(generation("governor-elsewhere", 300))),
        ];
        let launches = ImportedLaunches::number(records.iter().map(|(home, convoy)| (*home, convoy)));
        let standing = standing_name("flotilla", "governor").expect("standing name");
        assert_eq!(launches.name("flotilla", "demo").as_deref(), Some("demo#1"));
        assert_eq!(launches.name("flotilla", "governor-300"), Some(format!("{standing}#300")));
        assert_eq!(launches.missing("flotilla", "unstarted"), Some(NoLaunch::NeverBootstrapped));
        assert_eq!(launches.missing("flotilla", "governor-301"), Some(NoLaunch::NeverBootstrapped));
        assert_eq!(launches.missing("flotilla", "elsewhere"), Some(NoLaunch::NotHeld));
        assert_eq!(launches.missing("flotilla", "demo"), None);
        assert_eq!((launches.highest("feta"), launches.highest("udder"), launches.highest("b3")), (301, 300, 0));
        assert_eq!(
            launches.collisions(),
            [LaunchCollision {
                home: "feta".into(),
                launch: format!("{standing}#300"),
                records: vec!["governor-300".into(), "governor-again".into()]
            }]
        );
    }

    // Behaviour (#3108): an imported record's address names a convoy only by the launch it imports
    // as, where the import writes it: a seat, a message under it, an artifact about the convoy and
    // a standing generation's seat each name the launch, an address naming no convoy is unchanged,
    // and one naming a convoy that imports as no launch is refused.
    #[test]
    fn an_imported_address_names_its_convoys_launch() {
        let launch = |of: &str, n| ImportedLaunch { home: "feta".into(), project: "flotilla".into(), of: of.into(), n };
        let launches = ImportedLaunches::default().with("flotilla", "demo", launch("demo", 4)).with(
            "flotilla",
            "governor-7",
            launch("project:flotilla/convoy:governor", 2),
        );
        let mapped =
            |address: &str| launches.address("flotilla", &address.parse().expect("address")).map(|address| address.as_str().to_owned());
        assert_eq!(mapped("project:flotilla/convoy:demo/role:coder").as_deref(), Ok("project:flotilla/convoy:demo%234/role:coder"));
        assert_eq!(
            mapped("project:flotilla/convoy:demo/role:coder/message:m-1").as_deref(),
            Ok("project:flotilla/convoy:demo%234/role:coder/message:m-1")
        );
        // A convoy with no project of its own is addressed under its namespace's, as it imports.
        assert_eq!(mapped("convoy:demo/role:coder").as_deref(), Ok("project:flotilla/convoy:demo%234/role:coder"));
        let artifact = crate::address::artifact_address("demo", "coder", "decision-ledger", "demo").expect("artifact");
        let expected = crate::address::artifact_address("demo#4", "coder", "decision-ledger", "demo#4").expect("artifact");
        assert_eq!(mapped(artifact.as_str()).as_deref(), Ok(expected.as_str()));
        assert_eq!(
            mapped("project:flotilla/convoy:governor-7/role:governor").as_deref(),
            Ok("project:flotilla/convoy:governor%232/role:governor")
        );
        // A convoy itself is at its launch's one address, as its seats are under (#3108 R4).
        assert_eq!(mapped("project:flotilla/convoy:demo").as_deref(), Ok("project:flotilla/convoy:demo%234"));
        assert_eq!(mapped("project:flotilla/convoy:governor-7").as_deref(), Ok("project:flotilla/convoy:governor%232"));
        let standing = convoy_reference_address("project:flotilla/convoy:governor#2", "flotilla").expect("record");
        assert_eq!(standing.as_str(), "project:flotilla/convoy:governor%232", "the engine refers to a launch at its one address");
        assert_eq!(mapped("project:flotilla/role:operator").as_deref(), Ok("project:flotilla/role:operator"));
        assert!(mapped("project:flotilla/convoy:gone/role:coder").expect_err("refused").contains("not in this host's replica"));
    }
}
