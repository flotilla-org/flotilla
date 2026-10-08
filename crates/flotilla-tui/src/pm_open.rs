//! Semantic target for the row-level focus Regard.

use flotilla_protocol::{HostName, ResourceRef};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenInPmTarget {
    pub namespace: String,
    pub convoy: String,
    pub vessel: Option<String>,
    pub host: Option<HostName>,
}

impl OpenInPmTarget {
    pub fn resource_ref(&self) -> ResourceRef {
        let mut convoy = ResourceRef::new("flotilla.work/v1", "Convoy", &self.namespace, &self.convoy);
        if let Some(host) = &self.host {
            convoy = convoy.on_host(host.clone());
        }
        self.vessel.as_ref().map_or_else(|| convoy.clone(), |vessel| convoy.subresource(format!("vessels/{vessel}")))
    }
}
