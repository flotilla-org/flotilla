//! Rows decoded once, straight into the caller's resource type.
//!
//! A typed read asks the object-safe [`crate::Store`] for rows decoded by its
//! kind's typed decoder (see [`crate::ResourceKind::decode_typed_text`]). The
//! adapter runs that decoder where it reads the stored body, so each body is
//! parsed once into `ResourceObject<T>` and that same decode is the quarantine
//! check. The rows cross the object-safe boundary type-erased and the typed
//! extension layer unwraps them with [`DecodedObject::into_typed`].
//!
//! Raw (`*_raw`) reads are unchanged: replication and resource HTTP still
//! receive the stored `spec` and `status` trees verbatim, so a read never
//! applies defaults or drops unknown fields on their behalf.
use std::any::Any;

use flotilla_resources::{ReadResourceObject, Resource, ResourceError, ResourceObject, ResourcePosition, ResourceProvenance};
use serde::Deserialize;

use crate::SerializedResource;

/// One object decoded by a kind's typed decoder, or a serialized object that
/// an adapter without a typed decode path returned instead.
pub struct DecodedObject {
    object: Box<dyn Any + Send>,
    name: fn(&(dyn Any + Send)) -> &str,
}

impl std::fmt::Debug for DecodedObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedObject").field("name", &self.name()).finish_non_exhaustive()
    }
}

fn object_name<T: Resource>(object: &(dyn Any + Send)) -> &str {
    // `DecodedObject::typed` pairs this function with the `T` it boxes, so the
    // downcast cannot fail.
    let name = object.downcast_ref::<ResourceObject<T>>().map(|object| object.metadata.name.as_str());
    debug_assert!(name.is_some(), "decoded row name function does not match its object type");
    name.unwrap_or_default()
}

impl DecodedObject {
    pub fn typed<T: Resource>(object: ResourceObject<T>) -> Self {
        Self { object: Box::new(object), name: object_name::<T> }
    }

    /// Wrap an object already validated by its kind (`ResourceKind::decode_object`
    /// or `from_k8s`). [`Self::into_typed`] deserializes its spec and status once.
    pub fn serialized(object: ResourceObject<SerializedResource>) -> Self {
        Self::typed(object)
    }

    pub fn name(&self) -> &str {
        (self.name)(self.object.as_ref())
    }

    pub fn into_typed<T: Resource>(self) -> Result<ResourceObject<T>, ResourceError> {
        let object = match self.object.downcast::<ResourceObject<T>>() {
            Ok(object) => return Ok(*object),
            Err(object) => object,
        };
        match object.downcast::<ResourceObject<SerializedResource>>() {
            Ok(object) => typed_object(*object),
            Err(_) => Err(ResourceError::other(format!("decoded row is not a {}", T::API_PATHS.kind))),
        }
    }

    /// Raw form of a row decoded for a raw caller. Typed rows cannot be turned
    /// back into stored trees without re-encoding, so they are refused.
    ///
    /// Which rows are typed: the `Store::*_decoded` defaults wrap `*_raw`
    /// results and always return serialized rows; the SQLite and in-memory
    /// overrides return typed rows, except raw watch replay, which carries
    /// serialized rows for `SerializedWatchEvent::decode_raw`. Raw callers
    /// therefore read through `*_raw`.
    pub fn into_serialized(self) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.object
            .downcast::<ResourceObject<SerializedResource>>()
            .map(|object| *object)
            .map_err(|_| ResourceError::other("typed decoded row has no serialized form"))
    }
}

/// A decoded row and where it came from.
#[derive(Debug)]
pub struct DecodedReadObject {
    pub object: DecodedObject,
    pub provenance: ResourceProvenance,
}

impl DecodedReadObject {
    pub fn local(object: DecodedObject) -> Self {
        Self { object, provenance: ResourceProvenance::Local }
    }
    pub fn serialized(item: ReadResourceObject<SerializedResource>) -> Self {
        Self { object: DecodedObject::serialized(item.object), provenance: item.provenance }
    }
    pub fn into_typed<T: Resource>(self) -> Result<ReadResourceObject<T>, ResourceError> {
        Ok(ReadResourceObject { object: self.object.into_typed()?, provenance: self.provenance })
    }
}

/// A decoded list of one kind's local objects and the version it was read at.
#[derive(Debug)]
pub struct DecodedList {
    pub items: Vec<DecodedObject>,
    pub resource_version: String,
    pub generation: Option<String>,
}

impl DecodedList {
    pub fn serialized(list: flotilla_resources::ResourceList<SerializedResource>) -> Self {
        Self {
            items: list.items.into_iter().map(DecodedObject::serialized).collect(),
            resource_version: list.resource_version,
            generation: list.generation,
        }
    }
    pub fn into_typed<T: Resource>(self) -> Result<flotilla_resources::ResourceList<T>, ResourceError> {
        Ok(flotilla_resources::ResourceList {
            items: self.items.into_iter().map(DecodedObject::into_typed).collect::<Result<_, _>>()?,
            resource_version: self.resource_version,
            generation: self.generation,
        })
    }
}

/// Decoded declared-query rows and the authoritative local position from the
/// same snapshot (see [`crate::QueryResult`]).
#[derive(Debug)]
pub struct DecodedQueryResult {
    pub items: Vec<DecodedReadObject>,
    pub position: ResourcePosition,
    pub local_root: Option<flotilla_protocol::NodeId>,
}

impl DecodedQueryResult {
    pub fn serialized(result: crate::QueryResult) -> Self {
        Self {
            items: result.items.into_iter().map(DecodedReadObject::serialized).collect(),
            position: result.position,
            local_root: result.local_root,
        }
    }
    pub fn into_typed<T: Resource>(self) -> Result<Vec<ReadResourceObject<T>>, ResourceError> {
        self.items.into_iter().map(DecodedReadObject::into_typed).collect()
    }
}

/// Deserialize a validated serialized object's spec and status into `T` once.
///
/// This replaces a serialize-then-deserialize round trip of the whole object:
/// metadata is already typed and moves across unchanged.
pub fn typed_object<T: Resource>(object: ResourceObject<SerializedResource>) -> Result<ResourceObject<T>, ResourceError> {
    let spec = T::Spec::deserialize(object.spec).map_err(|error| ResourceError::decode(format!("decode stored object: {error}")))?;
    let status = match object.status {
        Some(status) => {
            Option::<T::Status>::deserialize(status).map_err(|error| ResourceError::decode(format!("decode stored object: {error}")))?
        }
        None => None,
    };
    Ok(ResourceObject { metadata: object.metadata, spec, status })
}

pub fn typed_read<T: Resource>(item: ReadResourceObject<SerializedResource>) -> Result<ReadResourceObject<T>, ResourceError> {
    Ok(ReadResourceObject { object: typed_object(item.object)?, provenance: item.provenance })
}

pub fn typed_read_list<T: Resource>(
    list: flotilla_resources::ReadResourceList<SerializedResource>,
) -> Result<flotilla_resources::ReadResourceList<T>, ResourceError> {
    Ok(flotilla_resources::ReadResourceList { items: list.items.into_iter().map(typed_read).collect::<Result<_, _>>()? })
}

pub fn typed_read_watch<T: Resource>(
    event: flotilla_resources::ReadWatchEvent<SerializedResource>,
) -> Result<flotilla_resources::ReadWatchEvent<T>, ResourceError> {
    use flotilla_resources::ReadWatchEvent;
    Ok(match event {
        ReadWatchEvent::Added(item) => ReadWatchEvent::Added(typed_read(item)?),
        ReadWatchEvent::Modified(item) => ReadWatchEvent::Modified(typed_read(item)?),
        ReadWatchEvent::Deleted(item) => ReadWatchEvent::Deleted(typed_read(item)?),
        ReadWatchEvent::DeletedByName { tombstone, provenance } => ReadWatchEvent::DeletedByName { tombstone, provenance },
    })
}
