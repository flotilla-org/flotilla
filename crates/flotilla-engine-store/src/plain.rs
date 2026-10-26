//! Plain kinds on the engine store (#3824 layer 0): resources with no machine.
//!
//! A plain kind is a spec and nothing else: no state, no edges, no records. Its home creates it,
//! edits it whole and deletes it, each one write in its own transaction, logged as a `create`,
//! `edit` or `delete` entry, so its replicas follow it over the change stream like every engine
//! kind. Nothing but the home writes its records; other homes' rows are read as they arrived.

use std::sync::{Arc, Mutex};

use flotilla_schema::{Kind, KindRegistry, Origin, RegistryError, Timestamp};
use flotilla_schema_sqlite::{Store, StoreError};

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
    /// No live resource of this kind and name at this home.
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

    /// Creates `name` at this home; refused as `Exists` if it is live here.
    pub fn create<K: Kind>(&self, name: &str, spec: &K, writer: &str, at: Timestamp) -> Result<u64, PlainError> {
        let mut store = lock_store(&self.store);
        let mut write = store.write()?;
        let stamp = write.insert(name, spec, &Origin::new(writer, at))?;
        write.commit()?;
        Ok(stamp.version)
    }

    /// Replaces the spec of `name`, read at `version`. Only the changed fields and rows are
    /// written; an edit that changes nothing logs nothing and keeps the version.
    pub fn edit<K: Kind>(&self, name: &str, version: u64, spec: &K, writer: &str, at: Timestamp) -> Result<u64, PlainError> {
        let mut store = lock_store(&self.store);
        let mut write = store.write()?;
        let id = write.resolve(K::CODE, name, &self.home)?.ok_or_else(|| missing::<K>(name))?;
        let current = write.load::<K>(id)?.ok_or_else(|| missing::<K>(name))?;
        if current.version != version {
            return Err(PlainError::Conflict { kind: K::KIND, name: name.to_owned(), expected: version, stored: current.version });
        }
        let stamp = write.update(&current, spec, &Origin::new(writer, at))?;
        write.commit()?;
        Ok(stamp.version)
    }

    /// Deletes `name`, read at `version`. Its history and identity stay (ADR 0064, #3392 G9).
    pub fn delete<K: Kind>(&self, name: &str, version: u64, writer: &str, at: Timestamp) -> Result<(), PlainError> {
        let mut store = lock_store(&self.store);
        let mut write = store.write()?;
        let id = write.resolve(K::CODE, name, &self.home)?.ok_or_else(|| missing::<K>(name))?;
        let current = write.load::<K>(id)?.ok_or_else(|| missing::<K>(name))?;
        if current.version != version {
            return Err(PlainError::Conflict { kind: K::KIND, name: name.to_owned(), expected: version, stored: current.version });
        }
        write.delete::<K>(current.stamp(), &Origin::new(writer, at))?;
        write.commit()?;
        Ok(())
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

fn missing<K: Kind>(name: &str) -> PlainError {
    PlainError::Missing { kind: K::KIND, name: name.to_owned() }
}

fn plain<K>(loaded: flotilla_schema_sqlite::Loaded<K>) -> Plain<K> {
    Plain { name: loaded.name, home: loaded.home, version: loaded.version, spec: loaded.value }
}
