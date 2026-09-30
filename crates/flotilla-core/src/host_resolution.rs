//! Shared canonical host resolution for placement and fleet projections.

use flotilla_protocol::PlacementTargetHost;
use flotilla_resources::{Host as ResourceHost, ReadResourceObject};

pub(crate) fn canonical_placement_host_ref_from_sources(
    hosts: &[ReadResourceObject<ResourceHost>],
    host_ref: &str,
) -> Result<Option<PlacementTargetHost>, String> {
    let canonical = flotilla_resources::canonical_host_id(hosts.iter().map(|host| &host.object), host_ref)?;
    let Some(canonical) = canonical else {
        return Ok(None);
    };
    let resolved = hosts
        .iter()
        .find(|host| host.object.metadata.name == canonical.as_str())
        .expect("canonical host resolver selected an existing host");
    let display_name = if resolved.object.spec.display_name.is_empty() {
        resolved.object.metadata.name.clone()
    } else {
        resolved.object.spec.display_name.clone()
    };
    Ok(Some(PlacementTargetHost { reference: canonical, display_name }))
}
