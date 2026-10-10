use std::{
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

use futures::{stream::BoxStream, Stream};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{
    error::ResourceError,
    resource::{Resource, ResourceObject},
};

/// A collection's list/watch boundary, independent of its objects.
/// Capture before a point read and resume from this position to avoid losing
/// mutations concurrent with that read. Versions are opaque, not object versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourcePosition {
    pub resource_version: String,
    pub generation: Option<String>,
}

pub struct WatchStream<T: Resource> {
    generation: Option<String>,
    inner: BoxStream<'static, Result<WatchEvent<T>, ResourceError>>,
}

impl<T: Resource> WatchStream<T> {
    pub fn new(generation: Option<String>, inner: BoxStream<'static, Result<WatchEvent<T>, ResourceError>>) -> Self {
        Self { generation, inner }
    }

    pub fn generation(&self) -> Option<&str> {
        self.generation.as_deref()
    }
}

impl<T: Resource> fmt::Debug for WatchStream<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WatchStream").field("generation", &self.generation).finish_non_exhaustive()
    }
}

impl<T: Resource> Stream for WatchStream<T> {
    type Item = Result<WatchEvent<T>, ResourceError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchStart {
    /// Deliver future events only. No replay of current state.
    Now,
    /// Resume from a specific version, delivering all events since that point.
    FromVersion(String),
    /// Resume from a specific version within an ephemeral store generation.
    FromVersionInGeneration { generation: String, resource_version: String },
}

impl WatchStart {
    /// Resume a watch from where a list left off, carrying the list's store
    /// generation when it has one. Generational stores reject a plain
    /// `FromVersion` resume, so every list-then-watch caller must go through
    /// this instead of constructing `FromVersion` directly.
    pub fn resuming_from<T: Resource>(listed: &ResourceList<T>) -> Self {
        match &listed.generation {
            Some(generation) => {
                Self::FromVersionInGeneration { generation: generation.clone(), resource_version: listed.resource_version.clone() }
            }
            None => Self::FromVersion(listed.resource_version.clone()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(
    serialize = "T::Spec: Serialize, T::Status: Serialize",
    deserialize = "T::Spec: DeserializeOwned, T::Status: DeserializeOwned"
))]
pub enum WatchEvent<T: Resource> {
    Added(ResourceObject<T>),
    Modified(ResourceObject<T>),
    Deleted(ResourceObject<T>),
    DeletedByName(ResourceTombstone),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTombstone {
    pub name: String,
    pub namespace: String,
    pub resource_version: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub annotations: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(
    serialize = "T::Spec: Serialize, T::Status: Serialize",
    deserialize = "T::Spec: DeserializeOwned, T::Status: DeserializeOwned"
))]
pub struct ResourceList<T: Resource> {
    pub items: Vec<ResourceObject<T>>,
    pub resource_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
}
