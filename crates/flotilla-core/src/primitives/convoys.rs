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
    machines::convoy::{Convoy, ConvoyEdge, ConvoyRequest, MemoryConvoyWorld},
    Home, Resource, Version, WriteError,
};

/// This home's Convoy records and the world their gates read.
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
