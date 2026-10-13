//! Serialized watch delivery preserves shared live trees and owned replay trees.
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use flotilla_resources::*;
use futures::{stream::BoxStream, Stream};
use serde_json::Value;

pub trait WatchObject: Send + Sync {
    fn object(&self) -> &Value;
}
pub enum WatchPayload {
    Shared(Arc<dyn WatchObject>),
    Owned(Value),
    /// An object a storage worker already decoded and validated: a typed row
    /// for [`crate::Store::watch_decoded`] consumers, or a serialized row for
    /// raw watches. Name-only tombstones stay `Owned`.
    Decoded(crate::DecodedObject),
}
#[derive(Clone, Copy)]
pub enum WatchAction {
    Added,
    Modified,
    Deleted,
}
pub struct SerializedWatchEvent {
    pub action: WatchAction,
    pub payload: WatchPayload,
}
impl SerializedWatchEvent {
    pub fn decode<T: Resource>(self) -> Result<WatchEvent<T>, ResourceError> {
        let value = match &self.payload {
            WatchPayload::Shared(object) => Some(object.object()),
            WatchPayload::Owned(value) => Some(value),
            WatchPayload::Decoded(_) => None,
        };
        if let Some(value) = value.filter(|value| matches!(self.action, WatchAction::Deleted) && value.get("spec").is_none()) {
            return crate::watch::decode_watch_tombstone(value).map(WatchEvent::DeletedByName);
        }
        let object = match self.payload {
            WatchPayload::Shared(object) => crate::watch::decode_watch_object(object.object()),
            WatchPayload::Owned(value) => crate::watch::decode_watch_object(value),
            WatchPayload::Decoded(object) => object.into_typed(),
        }?;
        Ok(match self.action {
            WatchAction::Added => WatchEvent::Added(object),
            WatchAction::Modified => WatchEvent::Modified(object),
            WatchAction::Deleted => WatchEvent::Deleted(object),
        })
    }
    pub fn from_raw(kind: crate::ResourceKind, event: WatchEvent<crate::SerializedResource>) -> Result<Self, ResourceError> {
        let (action, value) = match event {
            WatchEvent::Added(object) => (WatchAction::Added, kind.encode_object(&object)?),
            WatchEvent::Modified(object) => (WatchAction::Modified, kind.encode_object(&object)?),
            WatchEvent::Deleted(object) => (WatchAction::Deleted, kind.encode_object(&object)?),
            WatchEvent::DeletedByName(t) => (
                WatchAction::Deleted,
                serde_json::json!({"metadata":{"name":t.name,"namespace":t.namespace,"resourceVersion":t.resource_version,"annotations":t.annotations}}),
            ),
        };
        Ok(Self { action, payload: WatchPayload::Owned(value) })
    }
    /// Decode with a runtime descriptor for untyped HTTP stand-ins.
    pub fn decode_raw(self, kind: crate::ResourceKind) -> Result<WatchEvent<crate::SerializedResource>, ResourceError> {
        let value = match self.payload {
            WatchPayload::Shared(object) => object.object().clone(),
            WatchPayload::Owned(value) => value,
            WatchPayload::Decoded(object) => {
                let object = object.into_serialized()?;
                return Ok(match self.action {
                    WatchAction::Added => WatchEvent::Added(object),
                    WatchAction::Modified => WatchEvent::Modified(object),
                    WatchAction::Deleted => WatchEvent::Deleted(object),
                });
            }
        };
        if matches!(self.action, WatchAction::Deleted) && value.get("spec").is_none() {
            return crate::watch::decode_watch_tombstone(&value).map(WatchEvent::DeletedByName);
        }
        let object = kind.decode_object(value)?;
        Ok(match self.action {
            WatchAction::Added => WatchEvent::Added(object),
            WatchAction::Modified => WatchEvent::Modified(object),
            WatchAction::Deleted => WatchEvent::Deleted(object),
        })
    }
}
pub struct SerializedReadWatchEvent {
    pub event: SerializedWatchEvent,
    pub provenance: ResourceProvenance,
}
impl SerializedReadWatchEvent {
    pub fn decode<T: Resource>(self) -> Result<ReadWatchEvent<T>, ResourceError> {
        let provenance = self.provenance;
        Ok(match self.event.decode()? {
            WatchEvent::Added(object) => ReadWatchEvent::Added(ReadResourceObject { object, provenance }),
            WatchEvent::Modified(object) => ReadWatchEvent::Modified(ReadResourceObject { object, provenance }),
            WatchEvent::Deleted(object) => ReadWatchEvent::Deleted(ReadResourceObject { object, provenance }),
            WatchEvent::DeletedByName(tombstone) => ReadWatchEvent::DeletedByName { tombstone, provenance },
        })
    }
}
pub struct SerializedWatchStream {
    generation: Option<String>,
    stream: BoxStream<'static, Result<SerializedWatchEvent, ResourceError>>,
}
impl SerializedWatchStream {
    pub fn new(generation: Option<String>, stream: BoxStream<'static, Result<SerializedWatchEvent, ResourceError>>) -> Self {
        Self { generation, stream }
    }
    pub fn generation(&self) -> Option<&str> {
        self.generation.as_deref()
    }
}
impl Stream for SerializedWatchStream {
    type Item = Result<SerializedWatchEvent, ResourceError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(cx)
    }
}
impl WatchObject for crate::replica::StoredReplicaEvent {
    fn object(&self) -> &Value {
        &self.object
    }
}
impl From<crate::replica::StoredReplicaEventKind> for WatchAction {
    fn from(kind: crate::replica::StoredReplicaEventKind) -> Self {
        match kind {
            crate::replica::StoredReplicaEventKind::Added => Self::Added,
            crate::replica::StoredReplicaEventKind::Modified => Self::Modified,
            crate::replica::StoredReplicaEventKind::Deleted => Self::Deleted,
        }
    }
}
