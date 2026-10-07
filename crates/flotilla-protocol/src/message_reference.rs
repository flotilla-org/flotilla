use serde::{Deserialize, Serialize};

use crate::ResourceRef;

/// References retain source identity and revision, rather than opaque keys or
/// hashes of message content. Visibility gates are implemented separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageReference {
    ChangeRequest { service: String, scope: String, number: u64, revision: String },
    Commit { repository: ResourceRef, revision: String },
    Ref { repository: ResourceRef, name: String, revision: String },
    Artifact { resource: ResourceRef, revision: String },
    Issue { service: String, scope: String, number: u64, revision: String },
    Comment { service: String, scope: String, id: String, revision: String },
    ControlRecord { resource: ResourceRef, revision: String },
}
