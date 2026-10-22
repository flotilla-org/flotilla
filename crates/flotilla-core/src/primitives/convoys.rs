//! Convoy on the engine behind the `[primitives]` switch (#3587 layer 6).
//!
//! [`Convoys`] is this home's Convoy writer: one engine [`Home`] over the
//! decided store's log, restored from the store on open, and the world
//! snapshot its gates read. The caller records what the gates take as given
//! (a project's existence, an authorisation, a reference's durability) in
//! the world before it applies the edge, the way the Message gates read
//! [`super::GateWorld`].

use std::sync::{Arc, Mutex};

use flotilla_engine_store::StoreLog;
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::{
    machines::convoy::{Convoy, ConvoyEdge, ConvoyRefusal, ConvoyRequest, MemoryConvoyWorld},
    Edge as _, Home, Resource, Version, WriteError,
};

/// This home's Convoy records and the world their gates read.
///
/// Lock order: a write takes `home`, then `world`; [`Convoys::world`] takes
/// `world` alone. A new path that holds both takes them in that order.
pub struct Convoys {
    home: Mutex<Home<Convoy, StoreLog>>,
    world: Mutex<MemoryConvoyWorld>,
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
        Ok(Self { home: Mutex::new(convoys), world: Mutex::new(MemoryConvoyWorld::default()) })
    }

    /// One keyed read; absence means no Convoy of that name is homed here.
    pub fn get(&self, name: &str) -> Option<Resource<Convoy>> {
        self.home.lock().expect("convoy home").get(name).cloned()
    }

    /// Edit the world the gates read; the next admission or edge reads the result.
    pub fn world<T>(&self, edit: impl FnOnce(&mut MemoryConvoyWorld) -> T) -> T {
        edit(&mut self.world.lock().expect("convoy world"))
    }

    /// Admits `name`, unless a Convoy of that name is already homed here
    /// (`Ok(false)`). A refusal is the machine's, as `Err`.
    pub fn admit(&self, name: &str, request: ConvoyRequest, now: Timestamp) -> Result<bool, WriteError<Convoy>> {
        let mut home = self.home.lock().expect("convoy home");
        if home.get(name).is_some() {
            return Ok(false);
        }
        let world = self.world.lock().expect("convoy world");
        home.create(name, request, &*world, now)?;
        Ok(true)
    }

    /// Applies `edge` to `name` through the store's log.
    pub fn apply(&self, name: &str, edge: impl Into<ConvoyEdge>, now: Timestamp) -> Result<Version, WriteError<Convoy>> {
        let mut home = self.home.lock().expect("convoy home");
        let world = self.world.lock().expect("convoy world");
        home.apply(name, edge, &*world, now)?;
        Ok(home.get(name).map(Resource::version).expect("an applied edge leaves its resource"))
    }
}

impl Convoys {
    /// Works out edges from `name`'s current record and the world, then
    /// applies them in order, all under the writer lock so no other write
    /// lands between the read and the edges. Returns how many applied; the
    /// first refusal stops the rest and is returned with the edge's position.
    pub fn edit(
        &self,
        name: &str,
        now: Timestamp,
        plan: impl FnOnce(&Resource<Convoy>, &mut MemoryConvoyWorld) -> Result<Vec<ConvoyEdge>, String>,
    ) -> Result<usize, EditError> {
        let mut home = self.home.lock().expect("convoy home");
        let mut world = self.world.lock().expect("convoy world");
        let current = home.get(name).ok_or_else(|| EditError::Unknown(name.to_owned()))?;
        let edges = plan(current, &mut world).map_err(EditError::Plan)?;
        let count = edges.len();
        for edge in edges {
            let kind = format!("{:?}", edge.kind());
            home.apply(name, edge, &*world, now).map_err(|error| match error {
                WriteError::Refused(refusal) => EditError::Refused { edge: kind, refusal },
                other => EditError::Write(format!("{other:?}")),
            })?;
        }
        Ok(count)
    }
}

/// Why [`Convoys::edit`] wrote nothing more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// No Convoy of that name is homed here.
    Unknown(String),
    /// The caller's plan found nothing to do with the record as it is.
    Plan(String),
    /// The machine refused an edge.
    Refused { edge: String, refusal: ConvoyRefusal },
    /// The log or a declaration fault.
    Write(String),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::Unknown(name) => write!(f, "convoy `{name}` is not on the engine at this home"),
            EditError::Plan(reason) => f.write_str(reason),
            EditError::Refused { edge, refusal } => write!(f, "the engine refused {edge}: {refusal:?}"),
            EditError::Write(error) => write!(f, "the engine did not write: {error}"),
        }
    }
}
