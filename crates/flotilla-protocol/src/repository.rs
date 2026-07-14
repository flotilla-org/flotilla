//! Portable repository identity types shared by resources and query scopes.

use serde::{Deserialize, Serialize};

/// Storage-safe key of a Repository resource.
///
/// The key is opaque on the wire. Its derivation and referent verification
/// remain owned by `flotilla-resources`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepositoryKey(pub String);

impl std::fmt::Display for RepositoryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
