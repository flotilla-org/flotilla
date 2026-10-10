use flotilla_protocol::LeafAddress;
use flotilla_resources::{Artifact, ResourceError};

use crate::ResourceBackend;

/// Only the winner of the authority's compare-and-swap may attempt POST.
/// Reservations do not expire while the artifact envelope exists. Deleting it
/// removes its reservations; retention must be considered before reconciliation.
/// An absent comment cannot distinguish a refused
/// POST from an accepted request still in flight. Retry by looking up the marker,
/// never by granting another creation attempt. Revisions use PATCH instead.
pub async fn reserve_ledger_comment_creation(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    address: &LeafAddress,
) -> Result<bool, ResourceError> {
    if !matches!(address, LeafAddress::ChangeRequest { .. }) {
        return Err(ResourceError::invalid("ledger creation requires a change request"));
    }
    let resolver = backend.using::<Artifact>(namespace);
    for _ in 0..16 {
        let object = resolver.get(name).await?;
        if object.spec.kind != "decision-ledger" {
            return Err(ResourceError::invalid("ledger creation requires a decision-ledger artifact"));
        }
        let mut status = object.status.unwrap_or_default();
        if status.ledger_comment_creations.contains(address) {
            return Ok(false);
        }
        status.ledger_comment_creations.push(address.clone());
        match resolver.update_status(name, &object.metadata.resource_version, &status).await {
            Ok(_) => return Ok(true),
            Err(ResourceError::Conflict { .. }) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(ResourceError::conflict(name, "ledger creation reservation contention"))
}
