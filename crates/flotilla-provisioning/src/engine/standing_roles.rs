//! The StandingRole port over the decided store (#3108 retirement): one plain record per role,
//! named by its ConvoyEnsure and written at the home that admits the role.

use std::sync::Arc;

use flotilla_engine_store::plain::{PendingChange, PlainLog};
use flotilla_resources::standing_roles::{RunningValues, StandingRoles};
use flotilla_schema::{NewChange, Timestamp};
use flotilla_schema_sqlite::stream::Service;

use crate::stored::standing_role::StandingRoleRecord;

/// The writer recorded on a role's writes: its admitting home's ensure pass.
const WRITER: &str = "system:standing-role";

/// This home's StandingRole writer, and every home's reader.
pub struct EngineStandingRoles {
    service: Arc<Service>,
    log: PlainLog,
    clock: Arc<dyn flotilla_resources::Clock>,
}

impl EngineStandingRoles {
    pub fn open(service: Arc<Service>, clock: Arc<dyn flotilla_resources::Clock>) -> Self {
        Self { log: PlainLog::new(service.store()), service, clock }
    }
}

fn values(record: StandingRoleRecord) -> RunningValues {
    RunningValues {
        strikes: record.strikes,
        strike_config: record.strike_config,
        backoff_from: record.backoff_from.and_then(Timestamp::to_datetime),
        last_failure: record.last_failure,
        counted: record.counted,
    }
}

/// The record of a role's `values`: what its ensure pass writes, and the import seeds.
pub fn record(values: &RunningValues) -> StandingRoleRecord {
    StandingRoleRecord {
        strikes: values.strikes,
        strike_config: values.strike_config.clone(),
        backoff_from: values.backoff_from.as_ref().map(Timestamp::from_datetime),
        last_failure: values.last_failure.clone(),
        counted: values.counted,
    }
}

#[async_trait::async_trait]
impl StandingRoles for EngineStandingRoles {
    async fn get(&self, ensure: &str) -> Result<Option<RunningValues>, String> {
        let here = self.log.home().to_owned();
        // A role is admitted at one home; this home's copy first, else the one that admits it.
        let homes = std::iter::once(here.clone()).chain(self.service.homes().into_iter().filter(|home| *home != here));
        for home in homes {
            if let Some(found) = self.log.get::<StandingRoleRecord>(ensure, &home).map_err(|error| error.to_string())? {
                return Ok(Some(values(found.spec)));
            }
        }
        Ok(None)
    }

    async fn put(&self, ensure: &str, values: &RunningValues) -> Result<(), String> {
        let change = PendingChange::new(NewChange::new(WRITER, Timestamp::from_datetime(&self.clock.now())));
        let spec = record(values);
        let here = self.log.home().to_owned();
        match self.log.get::<StandingRoleRecord>(ensure, &here).map_err(|error| error.to_string())? {
            Some(existing) => self.log.edit(ensure, existing.version, &spec, &change).map(drop),
            None => self.log.create(ensure, &spec, &change).map(drop),
        }
        .map_err(|error| error.to_string())
    }
}
