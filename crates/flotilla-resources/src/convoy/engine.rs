//! The engine's Convoys, as the code that still speaks the old shape reaches them
//! (#3587 layer 6).
//!
//! The engine Convoy decides (owner ruling, 2026-10-22, layer 6 question 1): crew verbs and
//! observers write the engine through [`EngineConvoys`] and return its refusals. Readers see a
//! convoy through [`viewed_read`] (layer 8): the engine's promises in the old shape, read keyed
//! at the convoy's home, when the engine holds it, else the stored status's. No old status is
//! written for the engine's promises.
//!
//! The port speaks the old shape: a crew is `(vessel, role)` and a promise is
//! its old id, so callers keep their validation and the implementation owns
//! the mapping onto seats and labels. A convoy the engine does not hold (one
//! admitted before the engine took over, until the cut-over import) stays on the old
//! path: [`EngineConvoys::holds`] is the one question callers ask first.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
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

/// What a landing's settlement read before it chose to deliver: each home's log sequence before
/// the first read, and the records and inboxes read after it. Delivery is written only if none of
/// them has been written since.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettlementEvidence {
    /// Each home's log sequence before the reads ([`EngineConvoys::position`]).
    pub read_from: BTreeMap<String, u64>,
    /// The held checkouts' launches, whose readings and inspections were judged.
    pub checkouts: BTreeSet<String>,
    /// The change requests' record names.
    pub change_requests: BTreeSet<String>,
    /// The crews' inboxes, whose undelivered messages hold delivery.
    pub inboxes: BTreeSet<String>,
}

impl SettlementEvidence {
    /// Evidence read after `position`, with nothing read yet.
    pub fn after(position: BTreeMap<String, u64>) -> Self {
        Self { read_from: position, ..Self::default() }
    }
}

/// What [`EngineConvoys::deliver`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// The convoy is delivered.
    Delivered,
    /// Nothing was written: what settlement read was written after it read it, as this says.
    Moved(String),
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

    /// Each home's log sequence now: a settlement reads its evidence after it, so the delivery it
    /// chooses can be refused if any of it is written in between.
    fn position(&self) -> Result<BTreeMap<String, u64>, String>;

    /// Landing's delivery as `disposition`, the exit-table entry settlement chose (claim, merged,
    /// closed-unmerged or a workflow's declared name), on what settlement read: refused while
    /// anything is open, and not written if any of `evidence` was written after it was read.
    async fn deliver(&self, convoy: &ResourceObject<Convoy>, disposition: &str, evidence: &SettlementEvidence) -> Result<Delivery, String>;

    /// The disposition the engine delivered `convoy` with, once it has: the convoy's outcome is
    /// decided, and nothing the old path observes afterwards (a work failing as its vessels find
    /// the convoy ended) changes it. `None` while it is open, or not held here.
    fn delivered(&self, convoy: &ResourceObject<Convoy>) -> Option<String>;

    /// An operator's abandonment, with a reason.
    async fn abandon(&self, convoy: &ResourceObject<Convoy>, principal: &str, reason: &str) -> Result<(), String>;

    /// An operator's delete: `convoy` is abandoned if it is open, and retired in this call when
    /// nothing holds it, so a read after the delete does not list it. One still held retires as
    /// its holds release.
    async fn delete(&self, convoy: &ResourceObject<Convoy>, principal: &str) -> Result<(), String>;

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

    /// [`Self::promises`], with what the convoy's log records beyond its record: each
    /// submission's time, the verdict that settled it and a retraction's reason (run 3 F7). It
    /// reads the convoy's log entries, so it serves keyed reads of one convoy (explain), never
    /// listings. An engine that keeps no log gives the record's promises.
    fn recorded_promises(&self, convoy: &ResourceObject<Convoy>, home: Option<&str>) -> Result<Option<OldPromises>, String> {
        self.promises(convoy, home)
    }

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
    if !path.holds(convoy) {
        return Err(format!("convoy `{}` is not held at this home; its notes are its home's", convoy.metadata.name));
    }
    let mut status = convoy.status.clone().unwrap_or_default();
    let (notes, _) = path.memory().update(&convoy.metadata.name, |notes| {
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
pub fn set_notes(path: &EngineConvoyPath, name: &str, status: &super::ConvoyStatus) {
    path.memory().update(name, |notes| notes.read_from(status));
}

/// The old status's promises: by vessel, then role.
pub type OldPromises = BTreeMap<String, BTreeMap<String, Vec<super::promises::Promise>>>;

/// The engine's Convoys, which every convoy reader and writer goes through.
pub type EngineConvoyPath = Arc<dyn EngineConvoys>;

/// The engine's Convoys for `convoy`, when the engine holds it.
pub fn deciding<'a>(path: &'a EngineConvoyPath, convoy: &ResourceObject<Convoy>) -> Option<&'a Arc<dyn EngineConvoys>> {
    Some(path).filter(|engine| engine.holds(convoy))
}

/// A convoy as its readers see it (#3587 layer 8), read at its home (a
/// replica's origin home): with the engine's promises in place of the stored
/// ones when the engine holds it, else as stored. A failed read is an error,
/// never the stored record: an engine-held convoy's stored promises were
/// handed over, and would read as none.
pub fn viewed_read(path: &EngineConvoyPath, read: &ReadResourceObject<Convoy>) -> Result<ResourceObject<Convoy>, String> {
    viewed(path, read, false)
}

/// [`viewed_read`] for a keyed read of one convoy: its promises carry what the log records of
/// them ([`EngineConvoys::recorded_promises`]).
pub fn recorded_read(path: &EngineConvoyPath, read: &ReadResourceObject<Convoy>) -> Result<ResourceObject<Convoy>, String> {
    viewed(path, read, true)
}

fn viewed(path: &EngineConvoyPath, read: &ReadResourceObject<Convoy>, recorded: bool) -> Result<ResourceObject<Convoy>, String> {
    let home = match &read.provenance {
        ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.as_str()),
        _ => None,
    };
    let held = if recorded { path.recorded_promises(&read.object, home)? } else { path.promises(&read.object, home)? };
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

/// The address a message refers to convoy record `name` by, under the project it belongs to: a
/// launch has one address, its [`launch_address`], which its seats are under (#3108 R4).
pub fn convoy_reference_address(name: &str, project: &str) -> Result<flotilla_address::Address, String> {
    flotilla_address::Address::project(project)
        .map_err(|error| error.to_string())
        .and_then(|project| convoy_address_in(&project, name))
        .map_err(|error| format!("convoy `{name}`: {error}"))
}

/// The address a message refers to `convoy` by: [`convoy_reference_address`] under its project,
/// as its seats are.
pub fn convoy_address(convoy: &ResourceObject<Convoy>) -> Result<flotilla_address::Address, String> {
    convoy_reference_address(&convoy.metadata.name, &convoy.spec.project_ref)
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
    standing_name(&convoy.spec.project_ref, role)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::Utc;

    use super::*;
    use crate::{
        promises::{Promise, PromiseKind, PromiseSource, PromiseState},
        ConvoySpec, ConvoyStatus, ObjectMeta,
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
        fn position(&self) -> Result<BTreeMap<String, u64>, String> {
            Ok(BTreeMap::new())
        }
        async fn deliver(&self, _: &ResourceObject<Convoy>, _: &str, _: &SettlementEvidence) -> Result<Delivery, String> {
            Ok(Delivery::Delivered)
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
        async fn delete(&self, _: &ResourceObject<Convoy>, _: &str) -> Result<(), String> {
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
            spec: ConvoySpec::builder().workflow_ref("w".to_string()).project_ref("flotilla".to_string()).build(),
            status: Some(ConvoyStatus::default()),
        }
    }

    // Behaviour (#3587 layer 8): readers see the engine's promises in place of the stored ones when
    // the engine holds the convoy, asked at the convoy's own home; for a convoy the engine does not
    // hold, they see the record as stored.
    #[test]
    fn readers_see_the_engines_promises_only_where_it_holds_the_convoy() {
        let local = |name: &str| ReadResourceObject { object: convoy(name), provenance: ResourceProvenance::Local };
        let engine = Arc::new(Holding { asked: Mutex::new(Vec::new()), memory: Default::default() });
        let on: EngineConvoyPath = engine.clone();
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
}
