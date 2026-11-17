//! Standing roles' running values for scenarios over the old store, which has no StandingRole
//! kind: held in memory for one home.

use std::{collections::BTreeMap, sync::Mutex};

use flotilla_resources::standing_roles::{RunningValues, StandingRoles};

#[derive(Default)]
pub struct MemoryStandingRoles {
    values: Mutex<BTreeMap<String, RunningValues>>,
}

#[async_trait::async_trait]
impl StandingRoles for MemoryStandingRoles {
    async fn get(&self, ensure: &str) -> Result<Option<RunningValues>, String> {
        Ok(self.values.lock().expect("standing roles").get(ensure).cloned())
    }

    async fn put(&self, ensure: &str, values: &RunningValues) -> Result<(), String> {
        self.values.lock().expect("standing roles").insert(ensure.to_owned(), values.clone());
        Ok(())
    }
}
