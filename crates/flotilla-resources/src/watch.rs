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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(
    serialize = "T::Spec: Serialize, T::Status: Serialize",
    deserialize = "T::Spec: DeserializeOwned, T::Status: DeserializeOwned"
))]
pub enum WatchEvent<T: Resource> {
    Added(ResourceObject<T>),
    Modified(ResourceObject<T>),
    Deleted(ResourceObject<T>),
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

#[cfg(test)]
mod tests {
    use super::WatchStart;

    #[test]
    fn watch_start_roundtrips_through_serde() {
        let encoded = serde_json::to_string(&WatchStart::FromVersion("7".to_string())).expect("serialize watch start");
        let decoded: WatchStart = serde_json::from_str(&encoded).expect("deserialize watch start");
        assert_eq!(decoded, WatchStart::FromVersion("7".to_string()));
    }
}
