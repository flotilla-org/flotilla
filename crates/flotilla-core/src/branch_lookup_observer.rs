//! Short-lived, daemon-local observations of proven branch absence.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

use tokio::time::{Duration, Instant};

use crate::providers::{
    change_request::{ChangeRequestTracker, ObservationError},
    types::ChangeRequest,
};

const ABSENCE_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_ABSENCES: usize = 1024;

struct Absence {
    provider: Weak<dyn ChangeRequestTracker>,
    expires: Instant,
}

/// Provider identity scopes observations to the current forge and credentials.
/// Replacing or retiring a provider invalidates its observations. Positive
/// results and errors are always read fresh; only a complete scan proves absence.
#[derive(Default)]
pub(crate) struct BranchLookupObserver {
    absences: Mutex<HashMap<(String, String), Absence>>,
}

impl BranchLookupObserver {
    pub(crate) async fn find(
        &self,
        repository: &str,
        provider: &Arc<dyn ChangeRequestTracker>,
        branch: &str,
    ) -> Result<Option<(String, ChangeRequest)>, ObservationError> {
        let key = (repository.to_string(), branch.to_string());
        {
            let mut absences = self.absences.lock().expect("branch absence observations");
            let now = Instant::now();
            absences.retain(|_, absence| absence.expires > now && absence.provider.strong_count() > 0);
            if absences.get(&key).is_some_and(|absence| Weak::ptr_eq(&absence.provider, &Arc::downgrade(provider))) {
                return Ok(None);
            }
        }
        // Do not hold the cache lock across forge I/O. Concurrent cold scans may
        // duplicate work, but unrelated repositories never wait on one another.
        let result = provider.find_change_request_by_branch(branch).await;
        if matches!(result, Ok(None)) {
            let mut absences = self.absences.lock().expect("branch absence observations");
            if absences.len() >= MAX_ABSENCES {
                if let Some(oldest) = absences.iter().min_by_key(|(_, absence)| absence.expires).map(|(key, _)| key.clone()) {
                    absences.remove(&oldest);
                }
            }
            absences.insert(key, Absence { provider: Arc::downgrade(provider), expires: Instant::now() + ABSENCE_TTL });
        }
        result
    }
}
