//! Convoy on the engine behind the `[primitives]` switch (#3587 layer 6).
//!
//! [`Convoys`] is this home's Convoy writer: one engine [`Home`] over the
//! decided store's log, restored from the store on open, and the world
//! snapshot its gates read. The caller records what the gates take as given
//! (a project's existence, an authorisation, a reference's durability) in
//! the world before it applies the edge, the way the Message gates read
//! [`super::GateWorld`].

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use flotilla_engine_store::StoreLog;
use flotilla_schema::{Origin, Timestamp};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::{
    machines::convoy::{Convoy, ConvoyEdge, ConvoyLabel, ConvoyRefusal, ConvoyRequest, MemoryConvoyWorld},
    Edge as _, Home, LogError, Resource, Version, WriteError,
};

/// This home's Convoy records and the world their gates read.
///
/// Lock order: a write takes `home`, then `world`; [`Convoys::world`] takes
/// `world` alone. A new path that holds both takes them in that order.
///
/// Poisoning: a panic while a lock is held poisons it. An edit's plan only
/// reads before any edge applies, and an applied edge leaves the home as its
/// log took it, so the records are consistent either way; the locks are
/// recovered rather than turning one panic into a fault on every later write.
pub struct Convoys {
    home_name: String,
    service: Arc<Service>,
    home: Mutex<Home<Convoy, StoreLog>>,
    world: Mutex<MemoryConvoyWorld>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Convoys {
    /// Opens the Convoy home at `home` over `service`, restoring every Convoy
    /// homed here so a restart keeps its records and versions.
    pub fn open(home: &str, service: &Arc<Service>) -> Result<Self, String> {
        let mut convoys = Home::with_log(home, StoreLog::new(service.store())).map_err(|error| format!("Convoy home: {error:?}"))?;
        let stored = service
            .with_store(|store| {
                let mut read = store.read()?;
                let mut stored = Vec::new();
                let mut after = None;
                loop {
                    let page = read.scan::<Resource<Convoy>>(home, after, 64)?;
                    for id in page.ids {
                        if let Some(loaded) = read.load::<Resource<Convoy>>(id)? {
                            stored.push(loaded.value);
                        }
                    }
                    after = page.next;
                    if after.is_none() {
                        return Ok(stored);
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        for convoy in stored {
            convoys.restore(convoy).map_err(|error| format!("{error:?}"))?;
        }
        Ok(Self {
            home_name: home.to_owned(),
            service: Arc::clone(service),
            home: Mutex::new(convoys),
            world: Mutex::new(MemoryConvoyWorld::default()),
        })
    }

    /// The home these records are homed at.
    pub fn home(&self) -> &str {
        &self.home_name
    }

    /// Another home's Convoy as this home's decided store replicates it: one
    /// keyed read by kind, name and home. Absence means no such replica.
    pub fn replica(&self, name: &str, home: &str) -> Result<Option<Resource<Convoy>>, String> {
        self.service
            .with_store(|store| {
                let mut read = store.read()?;
                let Some(id) = read.resolve(<Resource<Convoy> as flotilla_schema::Kind>::CODE, name, home)? else { return Ok(None) };
                Ok(read.load::<Resource<Convoy>>(id)?.map(|loaded| loaded.value))
            })
            .map_err(|error| error.to_string())
    }

    /// One keyed read; absence means no Convoy of that name is homed here.
    pub fn get(&self, name: &str) -> Option<Resource<Convoy>> {
        locked(&self.home).get(name).cloned()
    }

    /// Edit the world the gates read; the next admission or edge reads the
    /// result. Projects stay (one entry per project admission named). The
    /// `durable` and `authority` tables are scoped to one edge: put them here
    /// for an [`Convoys::apply`] that follows, or inside an edit's plan. Any
    /// [`Convoys::edit`] clears both when it ends, so an input put here before
    /// an unrelated edit is gone after it.
    pub fn world<T>(&self, edit: impl FnOnce(&mut MemoryConvoyWorld) -> T) -> T {
        edit(&mut locked(&self.world))
    }

    /// Admits `name`, unless a Convoy of that name is already homed here
    /// (`Ok(false)`). A refusal is the machine's, as `Err`.
    pub fn admit(&self, name: &str, request: ConvoyRequest, now: Timestamp) -> Result<bool, WriteError<Convoy>> {
        self.admit_generation(name, request, now, false)
    }

    /// Admit a standing generation, retiring an ended generation's live rows.
    /// Delete and create share a transaction; the per-home log retains both.
    pub fn admit_standing(&self, name: &str, request: ConvoyRequest, now: Timestamp) -> Result<bool, WriteError<Convoy>> {
        self.admit_generation(name, request, now, true)
    }

    fn admit_generation(
        &self,
        name: &str,
        request: ConvoyRequest,
        now: Timestamp,
        replace_ended: bool,
    ) -> Result<bool, WriteError<Convoy>> {
        let mut home = locked(&self.home);
        let world = locked(&self.world);
        let Some(previous) = home.get(name) else {
            home.create(name, request, &*world, now)?;
            return Ok(true);
        };
        if !replace_ended || previous.label() == ConvoyLabel::Open {
            return Ok(false);
        }
        // Validate admission before retiring anything. A gate or store refusal
        // leaves the ended generation intact in memory and on disk.
        let mut fresh = Home::<Convoy>::new(&self.home_name)
            .map_err(|errors| WriteError::Log(LogError::new(format!("Convoy definition: {errors:?}"))))?;
        let created = fresh.create(name, request, &*world, now)?;
        let resource = created.resource.clone();
        let origin =
            Origin::new(&self.home_name, now).with_payload(&created.reads).map_err(|error| WriteError::Log(LogError::new(error)))?;
        let version = self
            .service
            .with_store(|store| {
                let loaded = {
                    let mut read = store.read()?;
                    let Some(id) = read.resolve(<Resource<Convoy> as flotilla_schema::Kind>::CODE, name, &self.home_name)? else {
                        return Ok(None);
                    };
                    let Some(loaded) = read.load::<Resource<Convoy>>(id)? else {
                        return Ok(None);
                    };
                    loaded
                };
                let mut write = store.write()?;
                write.delete::<Resource<Convoy>>(loaded.stamp(), &origin)?;
                let stamp = write.insert(name, &resource, &origin)?;
                write.commit()?;
                Ok(Some(stamp.version))
            })
            .map_err(|error| WriteError::Log(LogError::new(error)))?
            .ok_or_else(|| WriteError::Log(LogError::new(format!("resident Convoy `{name}` is missing from the store"))))?;
        home.evict(name);
        // Restore only refuses an existing name. The writer lock is still held
        // and that name was just evicted, so it cannot fail after this commit.
        home.restore(Resource::stored(name, resource.spec().clone(), resource.stored_state().clone(), Version(version)))?;
        Ok(true)
    }

    /// Applies `edge` to `name` through the store's log.
    pub fn apply(&self, name: &str, edge: impl Into<ConvoyEdge>, now: Timestamp) -> Result<Version, WriteError<Convoy>> {
        let mut home = locked(&self.home);
        let world = locked(&self.world);
        home.apply(name, edge, &*world, now)?;
        Ok(home.get(name).map(Resource::version).expect("an applied edge leaves its resource"))
    }

    /// Works out edges from `name`'s current record and the world, then
    /// applies them in order, all under the writer lock so no other write
    /// lands between the read and the edges. Returns how many applied and
    /// what the plan returned beside its edges; the first refusal stops the
    /// rest and says how many applied before it. The durability and
    /// authority inputs the plan recorded are the edit's own and are cleared
    /// once it ends, so the world does not grow with the edits.
    ///
    /// `plan` must not panic, and must only read the record and record its
    /// gate inputs: it runs under both locks, and their poisoning is
    /// recovered on the strength of that (see [`Convoys`]).
    pub fn edit<T>(
        &self,
        name: &str,
        now: Timestamp,
        plan: impl FnOnce(&Resource<Convoy>, &mut MemoryConvoyWorld) -> Result<(Vec<ConvoyEdge>, T), String>,
    ) -> Result<(usize, T), EditError> {
        let mut home = locked(&self.home);
        let mut world = locked(&self.world);
        let current = home.get(name).ok_or_else(|| EditError::Unknown(name.to_owned()))?;
        let planned = plan(current, &mut world);
        let result = planned.map_err(EditError::Plan).and_then(|(edges, output)| {
            let mut applied = 0;
            for edge in edges {
                let kind = format!("{:?}", edge.kind());
                home.apply(name, edge, &*world, now).map_err(|error| match error {
                    WriteError::Refused(refusal) => EditError::Refused { edge: kind, refusal, applied },
                    other => EditError::Write { error: format!("{other:?}"), applied },
                })?;
                applied += 1;
            }
            Ok((applied, output))
        });
        world.durable = Default::default();
        world.authority = Default::default();
        result
    }
}

/// Why [`Convoys::edit`] wrote nothing more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// No Convoy of that name is homed here.
    Unknown(String),
    /// The caller's plan found nothing to do with the record as it is.
    Plan(String),
    /// The machine refused an edge, after `applied` earlier ones.
    Refused { edge: String, refusal: ConvoyRefusal, applied: usize },
    /// The log or a declaration fault, after `applied` earlier edges.
    Write { error: String, applied: usize },
}

impl EditError {
    /// How many of the plan's edges applied before this.
    pub fn applied(&self) -> usize {
        match self {
            EditError::Refused { applied, .. } | EditError::Write { applied, .. } => *applied,
            EditError::Unknown(_) | EditError::Plan(_) => 0,
        }
    }
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::Unknown(name) => write!(f, "convoy `{name}` is not on the engine at this home"),
            EditError::Plan(reason) => f.write_str(reason),
            EditError::Refused { edge, refusal, .. } => write!(f, "the engine refused {edge}: {refusal:?}"),
            EditError::Write { error, .. } => write!(f, "the engine did not write: {error}"),
        }
    }
}
