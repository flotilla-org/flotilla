use std::sync::Arc;

use flotilla_resources::{apply_status_patch, Clock, Demand, DemandExpiryDisposition, DemandState, DemandStatusPatch, ResourceBackend};

const EXPIRY_AUTHORITY: &str = "demand-expiry";

pub struct DemandLifecycle {
    backend: ResourceBackend,
    clock: Arc<dyn Clock>,
}

impl DemandLifecycle {
    pub fn new(backend: ResourceBackend, clock: Arc<dyn Clock>) -> Self {
        Self { backend, clock }
    }

    pub async fn expire_due(&self, namespace: &str) -> Result<(), String> {
        let resolver = self.backend.using::<Demand>(namespace);
        let now = self.clock.now();
        for demand in resolver.list().await.map_err(|error| error.to_string())?.items {
            if demand.status.as_ref().is_some_and(|status| status.state != DemandState::Raised) {
                continue;
            }
            let Some(expiry) = demand.spec.expiry else { continue };
            if now < expiry.deadline {
                continue;
            }
            match expiry.disposition {
                DemandExpiryDisposition::Escalate => {
                    apply_status_patch(&resolver, &demand.metadata.name, &DemandStatusPatch::Escalate {
                        as_of: now,
                        authority: EXPIRY_AUTHORITY.to_string(),
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                }
            }
        }
        Ok(())
    }
}
