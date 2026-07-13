//! Resource identity on the wire.

use serde::{Deserialize, Serialize};

use crate::host::HostName;

/// Fully-qualified reference to a resource (or one of its subresources),
/// optionally pinned to the host whose store holds it. Rows in query result
/// sets are keyed by this.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, bon::Builder)]
pub struct ResourceRef {
    pub api_version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subresource: Option<String>,
}

impl ResourceRef {
    pub fn new(api_version: impl Into<String>, kind: impl Into<String>, namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            api_version: api_version.into(),
            kind: kind.into(),
            namespace: namespace.into(),
            name: name.into(),
            host: None,
            subresource: None,
        }
    }

    pub fn on_host(mut self, host: HostName) -> Self {
        self.host = Some(host);
        self
    }

    pub fn subresource(&self, subresource: impl Into<String>) -> Self {
        let mut reference = self.clone();
        reference.subresource = Some(subresource.into());
        reference
    }
}
