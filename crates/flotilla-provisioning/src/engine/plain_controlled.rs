//! A plain kind hosted by the engine [`Controller`] (#4415).
//!
//! A plain kind ([`flotilla_engine_store::plain`]) has no machine: its home creates, edits and
//! deletes each record whole. The controller hosts one through [`PlainControlled`], which reads
//! the records homed here as lifecycles of [`PlainLifecycle`]: each one held is realised under its
//! own name, so the start pass and each commit judge it against its holders
//! ([`Actuator::holder`]), and releasing it deletes it. A plain record is never requested, so
//! nothing realises one, and it records no attempts: a failed release is tried again at the time
//! its actuator names or when a holder changes.
//!
//! [`Controller`]: super::controller::Controller
//! [`Actuator::holder`]: super::controller::Actuator::holder

use std::{marker::PhantomData, sync::Arc};

use flotilla_engine_store::plain::{PendingChange, PlainError, PlainLog};
use flotilla_schema::{Kind, KindCode, NewChange, Timestamp};
use flotilla_schema_sqlite::StoreHandle;
use flotilla_state_machines::machines::lifecycle::{LifecycleKind, LifecycleState};

use super::{
    controller::{Controlled, ControlledRecords},
    EngineError, Step,
};

/// How often a deletion that loses a race to another writer of its name reads and tries again.
const DELETE_ATTEMPTS: usize = 3;

/// Plain kind `S` as the lifecycle kind its controller and actuator are written for: its request
/// is the record.
pub struct PlainLifecycle<S>(PhantomData<fn() -> S>);

impl<S: Kind + Clone + 'static> LifecycleKind for PlainLifecycle<S> {
    const NAME: &'static str = S::KIND;
    type Request = S;
}

/// The records of plain kind `S` homed here, as its controller reads and writes them.
pub struct PlainControlled<S> {
    log: PlainLog,
    /// The writer recorded on the controller's deletions.
    writer: &'static str,
    kind: PhantomData<fn() -> S>,
}

impl<S> PlainControlled<S> {
    /// This home's records of `S` in `store`, deleted as `writer`.
    pub fn new(store: Arc<StoreHandle>, writer: &'static str) -> Self {
        Self::over(PlainLog::new(store), writer)
    }

    /// The records of `S` homed at `log`'s home, deleted as `writer`.
    pub fn over(log: PlainLog, writer: &'static str) -> Self {
        Self { log, writer, kind: PhantomData }
    }

    /// Deletes `name` as it is now; one already gone is done.
    fn delete(&self, name: &str, at: Timestamp) -> Result<(), EngineError>
    where
        S: Kind,
    {
        let change = PendingChange::new(NewChange::new(self.writer, at));
        let mut lost = None;
        for _ in 0..DELETE_ATTEMPTS {
            let Some(current) = self.log.get::<S>(name, self.log.home()).map_err(failed)? else { return Ok(()) };
            match self.log.delete::<S>(name, current.version, &change) {
                Ok(()) | Err(PlainError::Missing { .. }) => return Ok(()),
                Err(error @ PlainError::Conflict { .. }) => lost = Some(error),
                Err(error) => return Err(failed(error)),
            }
        }
        let last = lost.map(|error| error.to_string()).unwrap_or_default();
        Err(EngineError::Failed(format!("delete {} {name}: lost {DELETE_ATTEMPTS} races with its writers, the last: {last}", S::KIND)))
    }
}

fn failed(error: PlainError) -> EngineError {
    EngineError::Failed(error.to_string())
}

impl<S: Kind + Clone + Send + Sync + 'static> ControlledRecords<PlainLifecycle<S>> for PlainControlled<S> {
    fn code(&self) -> KindCode {
        S::CODE
    }

    fn home(&self) -> &str {
        self.log.home()
    }

    fn get(&self, name: &str) -> Result<Option<Controlled<S>>, String> {
        let held = self.log.get::<S>(name, self.log.home()).map_err(|error| error.to_string())?;
        Ok(held.map(|held| Controlled {
            state: LifecycleState::Realised { identity: held.name.clone() },
            name: held.name,
            request: held.spec,
        }))
    }

    /// Every record homed here: a plain record lives until it is deleted.
    fn live(&self) -> Result<Vec<String>, String> {
        Ok(self.log.list::<S>(self.log.home()).map_err(|error| error.to_string())?.into_iter().map(|held| held.name).collect())
    }

    fn step(&self, name: &str, step: &Step, at: Timestamp) -> Result<(), EngineError> {
        match step {
            // Its release is complete, or what it stood for is gone: it goes.
            Step::Released | Step::Vanished { .. } => self.delete(name, at),
            // A record held is realised already.
            Step::Realised { .. } => Ok(()),
            Step::Attempted(_) | Step::FailedEarly { .. } => {
                Err(EngineError::Failed(format!("{} {name} is never requested, so it records no attempt", S::KIND)))
            }
        }
    }

    /// A plain record is no launch: it has no predecessor.
    fn finish_launch_request(&self, _: &str, _: Timestamp) -> Result<(), EngineError> {
        Ok(())
    }
}
