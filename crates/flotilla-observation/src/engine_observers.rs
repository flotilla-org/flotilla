//! Whether the engine serves the forge observations (`[primitives]`). Once it does, an observer
//! writes only records it observes, at its own home: no placeholder for another host's demand and
//! no convoy history, which the convoys hold. A failed read is recorded on the record, and records
//! nothing demands are swept after one demand lease.

use std::sync::{Arc, OnceLock};

use flotilla_resources::engine_records::ObservationFailures;

/// The engine's observation records, shared by the observers and set once at composition.
#[derive(Clone, Default, Debug)]
pub struct EngineObservers(Arc<OnceLock<Arc<dyn ObservationFailures>>>);

impl EngineObservers {
    pub fn install(&self, failures: Arc<dyn ObservationFailures>) -> Result<(), String> {
        self.0.set(failures).map_err(|_| "the engine observation records are already installed".to_owned())
    }

    pub(crate) fn failures(&self) -> Option<&Arc<dyn ObservationFailures>> {
        self.0.get()
    }

    pub(crate) fn installed(&self) -> bool {
        self.0.get().is_some()
    }

    /// Records a failed read of `kind`'s record `name` at this home, when the engine serves it.
    pub(crate) async fn failed(&self, kind: &str, name: &str) {
        if let Some(failures) = self.failures() {
            if let Err(error) = failures.failing(kind, name).await {
                tracing::warn!(kind, name, %error, "record a failing observation");
            }
        }
    }
}
