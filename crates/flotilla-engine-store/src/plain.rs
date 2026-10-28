//! Plain kinds on the engine store (#3824 layer 0): resources with no machine.
//!
//! A plain kind is a spec and nothing else: no state, no edges, no records. Its home creates it,
//! edits it whole and deletes it, each one write in its own transaction, logged as a `create`,
//! `edit` or `delete` entry, so its replicas follow it over the change stream like every engine
//! kind. Nothing but the home writes its records; other homes' rows are read as they arrived.
//!
//! Every write names a [`PendingChange`], the reason it was made (#4024). The change is opened at
//! the first write that logs anything and shared by every later write of it, so one apply of many
//! records is one change, and an apply that changes nothing records no change.

use std::sync::{Arc, Mutex};

use flotilla_schema::{Kind, KindRegistry, NewChange, RegistryError};
use flotilla_schema_sqlite::{ChangeId, Loaded, Store, StoreError, Write};

use crate::lock_store;

/// A plain resource as stored: its identity, its version (the home sequence of its last write)
/// and its spec.
#[derive(Debug, Clone, PartialEq)]
pub struct Plain<K> {
    pub name: String,
    pub home: String,
    pub version: u64,
    pub spec: K,
}

/// A plain write the store refused.
#[derive(Debug)]
pub enum PlainError {
    /// A create of a name live at this home.
    Exists {
        kind: &'static str,
        name: String,
    },
    /// No live resource of this kind and name at this home. A resource homed elsewhere is
    /// `Missing` here too: only its home writes it.
    Missing {
        kind: &'static str,
        name: String,
    },
    /// The resource moved past the version the writer read.
    Conflict {
        kind: &'static str,
        name: String,
        expected: u64,
        stored: u64,
    },
    Store(StoreError),
}

impl std::fmt::Display for PlainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exists { kind, name } => write!(f, "{kind} `{name}` already exists here"),
            Self::Missing { kind, name } => write!(f, "{kind} `{name}` is not stored here"),
            Self::Conflict { kind, name, expected, stored } => {
                write!(f, "{kind} `{name}` is at version {stored}, not {expected}")
            }
            Self::Store(error) => write!(f, "{error}"),
        }
    }
}
impl std::error::Error for PlainError {}
impl From<StoreError> for PlainError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// A change a writer is making (#4024): who, when and from what, opened at its first write that
/// logs anything and named by every later one. Share one across an apply's writes so they are one
/// change.
#[derive(Debug)]
pub struct PendingChange {
    change: NewChange,
    opened: Mutex<Option<ChangeId>>,
}

impl PendingChange {
    pub fn new(change: NewChange) -> Self {
        Self { change, opened: Mutex::new(None) }
    }

    /// The change, once a write has opened it.
    pub fn opened(&self) -> Option<ChangeId> {
        *self.opened.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The change's id in `write`'s transaction, opening it there if no write has: `true` with it
    /// when this call opened it, so a write that then fails can forget it ([`PendingChange::forget`]).
    pub fn id(&self, write: &mut Write<'_>) -> Result<(ChangeId, bool), StoreError> {
        let mut opened = self.opened.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(id) = *opened {
            return Ok((id, false));
        }
        let id = write.open_change(&self.change)?;
        *opened = Some(id);
        Ok((id, true))
    }

    /// Forgets a change whose opening write was not committed.
    pub fn forget(&self, id: ChangeId) {
        let mut opened = self.opened.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if *opened == Some(id) {
            *opened = None;
        }
    }
}

/// Runs `body` in one write naming `change`'s id, and commits; a change this write opened is
/// forgotten again if the write does not commit.
fn in_change<T>(
    store: &Arc<Mutex<Store>>,
    change: &PendingChange,
    body: impl FnOnce(&mut Write<'_>, ChangeId) -> Result<T, PlainError>,
) -> Result<T, PlainError> {
    let mut store = lock_store(store);
    let mut write = store.write()?;
    let (id, opened) = change.id(&mut write)?;
    let done = body(&mut write, id).and_then(|value| write.commit().map(|()| value).map_err(PlainError::from));
    if done.is_err() && opened {
        change.forget(id);
    }
    done
}

/// Registers plain kind `K`: a stateless `#[derive(Stored)]` kind declaring no edges. It has no
/// machine, so it has no diagram.
pub fn register_plain<K: Kind>(kinds: &mut KindRegistry) -> Result<(), RegistryError> {
    kinds.register::<K>()
}

/// One home's writer and reader of plain kinds over a shared [`Store`], beside the machine kinds'
/// `StoreLog` on the same store.
#[derive(Clone)]
pub struct PlainLog {
    store: Arc<Mutex<Store>>,
    home: String,
}

impl PlainLog {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        let home = lock_store(&store).home().to_owned();
        Self { store, home }
    }

    pub fn home(&self) -> &str {
        &self.home
    }

    /// Creates `name` at this home as part of `change`; refused as `Exists` if it is live here.
    pub fn create<K: Kind>(&self, name: &str, spec: &K, change: &PendingChange) -> Result<u64, PlainError> {
        if self.get::<K>(name, &self.home)?.is_some() {
            return Err(PlainError::Exists { kind: K::KIND, name: name.to_owned() });
        }
        in_change(&self.store, change, |write, id| {
            let stamp = write.insert(name, spec, id).map_err(|error| match error {
                StoreError::Exists { .. } => PlainError::Exists { kind: K::KIND, name: name.to_owned() },
                error => PlainError::Store(error),
            })?;
            Ok(stamp.version)
        })
    }

    /// Replaces the spec of `name` homed here, read at `version`, as part of `change`. Only the
    /// changed fields and rows are written; an edit that changes nothing logs nothing, keeps the
    /// version and opens no change.
    pub fn edit<K: Kind + PartialEq>(&self, name: &str, version: u64, spec: &K, change: &PendingChange) -> Result<u64, PlainError> {
        {
            let mut store = lock_store(&self.store);
            let mut write = store.write()?;
            if self.read_at::<K>(&mut write, name, version)?.value == *spec {
                return Ok(version);
            }
        }
        in_change(&self.store, change, |write, id| {
            let current = self.read_at::<K>(write, name, version)?;
            Ok(write.update(&current, spec, id)?.version)
        })
    }

    /// Deletes `name` homed here, read at `version`, as part of `change`. Its history and identity
    /// stay (ADR 0064, #3392 G9).
    pub fn delete<K: Kind>(&self, name: &str, version: u64, change: &PendingChange) -> Result<(), PlainError> {
        in_change(&self.store, change, |write, id| {
            let current = self.read_at::<K>(write, name, version)?;
            write.delete::<K>(current.stamp(), id)?;
            Ok(())
        })
    }

    /// The live resource `name` homed at `home`, this one or another.
    pub fn get<K: Kind>(&self, name: &str, home: &str) -> Result<Option<Plain<K>>, PlainError> {
        let mut store = lock_store(&self.store);
        let mut read = store.read()?;
        let Some(id) = read.resolve(K::CODE, name, home)? else { return Ok(None) };
        Ok(read.load::<K>(id)?.map(plain))
    }

    /// Every live resource of `K` homed at `home`, in id order.
    pub fn list<K: Kind>(&self, home: &str) -> Result<Vec<Plain<K>>, PlainError> {
        let mut store = lock_store(&self.store);
        let mut read = store.read()?;
        let mut listed = Vec::new();
        let mut after = None;
        loop {
            let page = read.scan::<K>(home, after, 64)?;
            for id in page.ids {
                listed.extend(read.load::<K>(id)?.map(plain));
            }
            after = page.next;
            if after.is_none() {
                return Ok(listed);
            }
        }
    }
}

impl PlainLog {
    /// `name` homed here, refused unless it is live at `version`.
    fn read_at<K: Kind>(&self, write: &mut Write<'_>, name: &str, version: u64) -> Result<Loaded<K>, PlainError> {
        let id = write.resolve(K::CODE, name, &self.home)?.ok_or_else(|| missing::<K>(name))?;
        let current = write.load::<K>(id)?.ok_or_else(|| missing::<K>(name))?;
        if current.version != version {
            return Err(PlainError::Conflict { kind: K::KIND, name: name.to_owned(), expected: version, stored: current.version });
        }
        Ok(current)
    }
}

fn missing<K: Kind>(name: &str) -> PlainError {
    PlainError::Missing { kind: K::KIND, name: name.to_owned() }
}

fn plain<K>(loaded: Loaded<K>) -> Plain<K> {
    Plain { name: loaded.name, home: loaded.home, version: loaded.version, spec: loaded.value }
}
