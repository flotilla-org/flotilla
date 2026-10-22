//! The `[primitives]` switch's Convoy path, as old-store code reaches it
//! (#3587 layer 6).
//!
//! With the switch on, the engine Convoy decides (owner ruling, 2026-10-22,
//! layer 6 question 1): crew verbs and observers write the engine through
//! [`EngineConvoys`] and return its refusals. Readers see a convoy through
//! [`viewed`] (layer 8): the engine's promises in the old shape, read keyed
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
    borrow::Cow,
    collections::BTreeMap,
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

/// What engine admission made of an old convoy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// The engine holds it, admitted now or before.
    Held,
    /// Not the engine's to admit; it stays on the old path.
    Skipped(SkipReason),
    /// The engine refused it: the machine's refusal, by name
    /// (`NoObligations`, `UnknownProject`, ...).
    Refused(String),
}

/// Why engine admission left a convoy on the old path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Bootstrap has not frozen its workflow yet.
    NotBootstrapped,
    /// A standing convoy: the seat-growth item on #3587 gates those, with the import.
    Standing,
    /// Something is decided already; the import maps a convoy with history.
    Decided(String),
}

/// The engine-first Convoy writer, in the old shape.
#[async_trait]
pub trait EngineConvoys: Send + Sync {
    /// Whether the engine holds (and so decides) this convoy.
    fn holds(&self, convoy: &ResourceObject<Convoy>) -> bool;

    /// Admits a freshly bootstrapped convoy, unless the engine holds it. Only
    /// a convoy with nothing decided yet is admitted here: one with history
    /// waits for the import pass.
    async fn admit(&self, convoy: &ResourceObject<Convoy>) -> Result<Admission, String>;

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

    /// Handoff: every open promise of `from` moves to `to` in the same vessel.
    async fn reassign(&self, convoy: &ResourceObject<Convoy>, vessel: &str, from: &str, to: &str) -> Result<(), String>;

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

    /// Landing's delivery: refused while anything is open.
    async fn deliver(&self, convoy: &ResourceObject<Convoy>) -> Result<(), String>;

    /// An operator's abandonment, with a reason.
    async fn abandon(&self, convoy: &ResourceObject<Convoy>, principal: &str, reason: &str) -> Result<(), String>;

    /// The engine's promises of `convoy` in the old status's shape, when the
    /// engine at `home` (the convoy's home; `None` for this one) holds it:
    /// one keyed read, of this home's record or of the replica of another's.
    fn promises(&self, convoy: &ResourceObject<Convoy>, home: Option<&str>) -> Option<OldPromises>;

    /// The names of this home's convoys as their engine records change. No
    /// old record is written for an engine edge, so the convoy reconciler
    /// follows these to run again (#3587 layer 8).
    fn changes(&self) -> tokio::sync::broadcast::Receiver<String>;
}

/// The old status's promises: by vessel, then role.
pub type OldPromises = BTreeMap<String, BTreeMap<String, Vec<super::promises::Promise>>>;

/// The composition root's slot for [`EngineConvoys`]: set once, at startup,
/// when `[primitives]` is on.
pub type EngineConvoyPath = Arc<OnceLock<Arc<dyn EngineConvoys>>>;

/// The engine path for `convoy`, when the switch is on and the engine holds it.
pub fn deciding<'a>(path: &'a EngineConvoyPath, convoy: &ResourceObject<Convoy>) -> Option<&'a Arc<dyn EngineConvoys>> {
    path.get().filter(|engine| engine.holds(convoy))
}

/// `convoy` as its readers see it (#3587 layer 8): with the engine's promises
/// in place of the stored ones when the engine at its home holds it, else as
/// stored. `home` is the convoy's home, `None` for this one.
pub fn viewed<'a>(path: &EngineConvoyPath, convoy: &'a ResourceObject<Convoy>, home: Option<&str>) -> Cow<'a, ResourceObject<Convoy>> {
    match path.get().and_then(|engine| engine.promises(convoy, home)) {
        Some(promises) => {
            let mut viewed = convoy.clone();
            viewed.status.get_or_insert_with(Default::default).promises = promises;
            Cow::Owned(viewed)
        }
        None => Cow::Borrowed(convoy),
    }
}

/// [`viewed`] for a read that knows where its record came from.
pub fn viewed_read(path: &EngineConvoyPath, read: &ReadResourceObject<Convoy>) -> ResourceObject<Convoy> {
    let home = match &read.provenance {
        ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.as_str()),
        _ => None,
    };
    viewed(path, &read.object, home).into_owned()
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
        async fn admit(&self, _: &ResourceObject<Convoy>) -> Result<Admission, String> {
            Ok(Admission::Held)
        }
        async fn promise(&self, _: &ResourceObject<Convoy>, _: &str, _: &str, _: &PromiseOperation, _: Actor) -> Result<(), String> {
            Ok(())
        }
        async fn reassign(&self, _: &ResourceObject<Convoy>, _: &str, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn keep_by_verdict(&self, _: &ResourceObject<Convoy>, _: &VerdictScope, _: &str, _: &str) -> Result<Vec<String>, String> {
            Ok(Vec::new())
        }
        async fn verdict(&self, _: &ResourceObject<Convoy>, _: &str, _: bool, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn deliver(&self, _: &ResourceObject<Convoy>) -> Result<(), String> {
            Ok(())
        }
        async fn abandon(&self, _: &ResourceObject<Convoy>, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn promises(&self, convoy: &ResourceObject<Convoy>, home: Option<&str>) -> Option<OldPromises> {
            self.asked.lock().expect("asked").push(home.map(str::to_owned));
            (convoy.metadata.name != "unheld").then(kept)
        }
        fn changes(&self) -> tokio::sync::broadcast::Receiver<String> {
            tokio::sync::broadcast::channel(1).1
        }
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
    // the switch is on and the engine holds the convoy, asked at the convoy's own home; with the
    // switch off, or for a convoy the engine does not hold, they see the record as stored.
    #[test]
    fn readers_see_the_engines_promises_only_where_it_holds_the_convoy() {
        let off = EngineConvoyPath::default();
        assert!(matches!(viewed(&off, &convoy("c"), None), Cow::Borrowed(_)), "switch off: as stored");

        let engine = Arc::new(Holding { asked: Mutex::new(Vec::new()) });
        let on = EngineConvoyPath::default();
        let shared: Arc<dyn EngineConvoys> = engine.clone();
        assert!(on.set(shared).is_ok());
        let stored = convoy("c");
        let seen = viewed(&on, &stored, None);
        assert_eq!(seen.status.as_ref().expect("status").promises, kept());
        assert!(matches!(viewed(&on, &convoy("unheld"), None), Cow::Borrowed(_)), "not held: as stored");

        let replica = ReadResourceObject {
            object: convoy("c"),
            provenance: ResourceProvenance::Replica { origin_root: flotilla_protocol::NodeId::new("home-b"), last_synced_at: Utc::now() },
        };
        assert_eq!(viewed_read(&on, &replica).status.expect("status").promises, kept());
        let local = ReadResourceObject { object: convoy("c"), provenance: ResourceProvenance::Local };
        viewed_read(&on, &local);
        assert_eq!(*engine.asked.lock().expect("asked"), vec![None, None, Some("home-b".to_owned()), None]);
    }
}
