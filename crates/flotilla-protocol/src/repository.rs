//! Portable repository identity types shared by resources and query scopes.

use facet::Facet;
use serde::{Deserialize, Serialize};

pub const UNKNOWN_REPOSITORY_LABEL: &str = "Unknown repository";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryRelation {
    Fork,
}

impl std::fmt::Display for RepositoryRelation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fork => f.write_str("fork"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryUpstream {
    pub url: String,
    pub relation: RepositoryRelation,
}

/// Storage-safe key of a Repository resource.
///
/// The key is opaque on the wire. Its derivation and referent verification
/// remain owned by `flotilla-resources`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Facet)]
#[serde(transparent)]
#[facet(transparent)]
pub struct RepositoryKey(pub String);

impl std::fmt::Display for RepositoryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
