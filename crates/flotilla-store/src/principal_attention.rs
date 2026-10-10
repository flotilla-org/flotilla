use chrono::{DateTime, Utc};
use flotilla_resources::*;

use crate::{apply_status_patch_checked, TypedResolver};

struct ResolveDemandStatusPatch {
    as_of: DateTime<Utc>,
    authority: String,
    verdict: DemandVerdict,
}

impl StatusPatch<DemandStatus> for ResolveDemandStatusPatch {
    fn apply(&self, status: &mut DemandStatus) {
        if status.state != DemandState::Acknowledged {
            status.state = DemandState::Satisfied;
            status.raised.get_or_insert_with(|| DemandTransition { as_of: self.as_of, authority: self.authority.clone() });
            status.satisfied.get_or_insert_with(|| DemandTransition { as_of: self.as_of, authority: self.authority.clone() });
            status.verdict.get_or_insert_with(|| self.verdict.clone());
        }
    }
}

pub async fn resolve_demand(
    resolver: &TypedResolver<Demand>,
    name: &str,
    verdict: DemandVerdict,
    as_of: DateTime<Utc>,
    authority: String,
) -> Result<ResourceObject<Demand>, ResourceError> {
    let patch = ResolveDemandStatusPatch { as_of, authority, verdict: verdict.clone() };
    apply_status_patch_checked(resolver, name, &patch, move |demand| {
        demand.spec.validate_verdict(&verdict)?;
        if demand.status.as_ref().is_some_and(|status| status.state == DemandState::Acknowledged) {
            return Err(ResourceError::invalid("acknowledged demand cannot be resolved"));
        }
        if demand.status.as_ref().and_then(|status| status.verdict.as_ref()).is_none_or(|current| current == &verdict) {
            Ok(())
        } else {
            Err(ResourceError::invalid("demand already has a different verdict"))
        }
    })
    .await
}
