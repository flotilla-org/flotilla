//! Convoy on the engine behind the `[primitives]` switch (#3587 layer 6).
//!
//! [`Convoys`] is this home's Convoy writer: one engine [`Home`] over the
//! decided store's log, restored from the store on open, and the world
//! snapshot its gates read. The caller records what the gates take as given
//! (a project's existence, an authorisation, a reference's durability) in
//! the world before it applies the edge, the way the Message gates read
//! [`flotilla_engine_messages::GateWorld`].

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use flotilla_address::Address;
use flotilla_engine_store::StoreLog;
use flotilla_resources::convoy::admission::AdmissionDocument;
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::{
    machines::convoy::{Convoy, ConvoyEdge, ConvoyLabel, ConvoyRefusal, ConvoyRequest, ConvoySpec, MemoryConvoyWorld},
    Edge as _, Home, Resource, Version, WriteError,
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

    /// Another home's newest launch of `of` in `project` as this home's decided store replicates
    /// it: one read of the `by_launch` index at that home. Absence means no such replica.
    pub fn replica(&self, of: &str, project: &Address, home: &str) -> Result<Option<Resource<Convoy>>, String> {
        self.service
            .with_store(|store| {
                let mut read = store.read()?;
                let query = ConvoySpec::by_launch(project, &of.to_owned());
                let Some((id, _)) = read.integer_index_keys_before(&query, home, i64::MAX, 1)?.first().copied() else {
                    return Ok(None);
                };
                Ok(read.load::<Resource<Convoy>>(id)?.map(|loaded| loaded.value))
            })
            .map_err(|error| error.to_string())
    }

    /// The newest launch of `of` homed here: one keyed read. Absence means none is.
    pub fn get(&self, of: &str) -> Option<Resource<Convoy>> {
        locked(&self.home).newest_launch(of, None).cloned()
    }

    /// The launch named `name` (`<of>#<n>`) homed here, whether or not it is the newest.
    pub fn launch(&self, name: &str) -> Option<Resource<Convoy>> {
        locked(&self.home).get(name).cloned()
    }

    /// The launch of `of` homed here that was admitted or imported from the old record `record`,
    /// searched newest first: a standing role's generation is among its newest launches.
    pub fn admitted_from(&self, of: &str, record: &str) -> Result<Option<String>, String> {
        let home = locked(&self.home);
        let mut before = None;
        while let Some(launch) = home.newest_launch(of, before) {
            let document =
                AdmissionDocument::decode(launch.name(), launch.spec().admission.value().bytes()).map_err(|error| error.to_string())?;
            if document.admitted_from.as_deref() == Some(record) {
                return Ok(Some(launch.name().to_owned()));
            }
            before = Some(launch.spec().launch);
        }
        Ok(None)
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

    /// Admits the first launch of `request.launch_of`, unless one is already homed here
    /// (`Ok(false)`). A refusal is the machine's, as `Err`.
    pub fn admit(&self, request: ConvoyRequest, now: Timestamp) -> Result<bool, WriteError<Convoy>> {
        self.admit_launch(request, now, |_| false)
    }

    /// Admits a standing role's next generation: a new launch of `request.launch_of`, unless its
    /// newest launch is still Open (`Ok(false)`). An ended generation stays as it ended.
    pub fn admit_standing(&self, request: ConvoyRequest, now: Timestamp) -> Result<bool, WriteError<Convoy>> {
        self.admit_launch(request, now, |newest| newest.label() != ConvoyLabel::Open)
    }

    fn admit_launch(
        &self,
        request: ConvoyRequest,
        now: Timestamp,
        follows: impl FnOnce(&Resource<Convoy>) -> bool,
    ) -> Result<bool, WriteError<Convoy>> {
        let mut home = locked(&self.home);
        let world = locked(&self.world);
        if home.newest_launch(&request.launch_of, None).is_some_and(|newest| !follows(newest)) {
            return Ok(false);
        }
        let of = request.launch_of.clone();
        home.create(of, request, &*world, now)?;
        Ok(true)
    }

    /// Applies `edge` to the newest launch of `of` through the store's log.
    pub fn apply(&self, of: &str, edge: impl Into<ConvoyEdge>, now: Timestamp) -> Result<Version, WriteError<Convoy>> {
        let mut home = locked(&self.home);
        let world = locked(&self.world);
        let name = newest_name(&home, of).ok_or_else(|| WriteError::Unknown(of.to_owned()))?;
        home.apply(&name, edge, &*world, now)?;
        Ok(home.get(&name).map(Resource::version).expect("an applied edge leaves its resource"))
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
        let launch = newest_name(&home, name).ok_or_else(|| EditError::Unknown(name.to_owned()))?;
        let current = home.get(&launch).expect("the newest launch is held");
        let planned = plan(current, &mut world);
        let result = planned.map_err(EditError::Plan).and_then(|(edges, output)| {
            let mut applied = 0;
            for edge in edges {
                let kind = format!("{:?}", edge.kind());
                home.apply(&launch, edge, &*world, now).map_err(|error| match error {
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

/// The name of the newest launch of `of` held at `home`.
fn newest_name(home: &Home<Convoy, StoreLog>, of: &str) -> Option<String> {
    home.newest_launch(of, None).map(|launch| launch.name().to_owned())
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
