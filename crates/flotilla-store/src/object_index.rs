//! What a declared query reads of a typed object (#3108): the text at each declared path, found
//! while serializing the object in its document shape and skipping every part no path enters,
//! so a query of typed objects never builds their JSON.
//!
//! The text is what the store indexes for a path in the object's JSON document
//! ([`crate::declared_field_values`]): a string as itself, a number in its JSON form, an object
//! as its JSON text, and nothing for an array, a boolean or null. A `*` component matches every
//! element of an array and every field of an object.

use std::fmt::Display;

use serde::ser::{self, Impossible, Serialize, Serializer};
use serde_json::Value;

use crate::{Resource, ResourceError, ResourceObject};

/// The index keys of `object`, as [`crate::QueryIndex`] holds them: its labels, the values at its
/// kind's declared query paths and its owners.
pub fn object_index_keys<T: Resource>(object: &ResourceObject<T>) -> Result<Vec<(String, String)>, ResourceError> {
    let mut keys: Vec<_> = object.metadata.labels.iter().map(|(key, value)| (format!("label:{key}"), value.clone())).collect();
    let derived: Vec<_> = T::QUERYABLE_FIELDS.iter().map(|path| T::indexed_values(&object.spec, path)).collect();
    let walked: Vec<&str> =
        T::QUERYABLE_FIELDS.iter().zip(&derived).filter(|(_, derived)| derived.is_none()).map(|(path, _)| *path).collect();
    let mut walked = path_texts(object, &walked)?.into_iter();
    for (path, derived) in T::QUERYABLE_FIELDS.iter().zip(derived) {
        let values = match derived {
            Some(values) => values,
            None => walked.next().unwrap_or_default(),
        };
        keys.push((format!("{}:{path}", if values.is_empty() { "missing" } else { "present" }), String::new()));
        keys.extend(values.into_iter().map(|value| (format!("field:{path}"), value)));
    }
    for owner in &object.metadata.owner_references {
        keys.push(("owner".into(), crate::owner_key(&owner.api_version, &owner.kind, &owner.name)));
    }
    Ok(keys)
}

/// The text each of `paths` holds in `object`, one list per path in order.
pub(crate) fn path_texts<T: Resource>(object: &ResourceObject<T>, paths: &[&str]) -> Result<Vec<Vec<String>>, ResourceError> {
    let paths: Vec<Vec<&str>> = paths.iter().map(|path| components(path)).collect();
    let mut found = vec![Vec::new(); paths.len()];
    let cursors = (0..paths.len()).filter(|path| !paths[*path].is_empty()).map(|path| (path, 0)).collect();
    object
        .as_k8s()
        .serialize(Walk { paths: &paths, found: &mut found, cursors })
        .map_err(|error| ResourceError::decode(format!("{} {}: {error}", T::API_PATHS.kind, object.metadata.name)))?;
    Ok(found)
}

/// `path`'s components, as [`crate::declared_field_values`] reads the path: a quoted label or
/// annotation key is one component, and an array path's wildcard is a `*` component.
fn components(path: &str) -> Vec<&str> {
    fn plain(path: &str) -> Vec<&str> {
        for (map, prefix) in [("annotations", "metadata.annotations.\""), ("labels", "metadata.labels.\"")] {
            if let Some(key) = path.strip_prefix(prefix).and_then(|key| key.strip_suffix('"')) {
                return vec!["metadata", map, key];
            }
        }
        path.split('.').collect()
    }
    match crate::array_field_path(path) {
        Some((array, suffix)) => {
            let mut parts = plain(array);
            parts.push("*");
            if !suffix.is_empty() {
                parts.extend(suffix.split('.'));
            }
            parts
        }
        None => plain(path),
    }
}

#[derive(Debug)]
struct Error(String);

impl Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl ser::Error for Error {
    fn custom<M: Display>(message: M) -> Self {
        Self(message.to_string())
    }
}

/// Paths being followed through one part of an object: each path, by how many of its
/// components the part's place matches.
type Cursors = Vec<(usize, usize)>;

/// A serializer walking one part of an object, following only the paths that enter it.
struct Walk<'w, 'p> {
    paths: &'w [Vec<&'p str>],
    found: &'w mut Vec<Vec<String>>,
    cursors: Cursors,
}

impl Walk<'_, '_> {
    /// The paths that go on into the part at `key` (an element of an array has none).
    fn advance(&self, key: Option<&str>) -> (Cursors, Vec<usize>) {
        let mut deeper = Vec::new();
        let mut ending = Vec::new();
        for &(path, matched) in &self.cursors {
            let component = self.paths[path][matched];
            if component != "*" && Some(component) != key {
                continue;
            }
            if matched + 1 == self.paths[path].len() {
                ending.push(path);
            } else {
                deeper.push((path, matched + 1));
            }
        }
        (deeper, ending)
    }

    /// Walks the part `value` at `key`: records its text for the paths ending there and follows
    /// the paths that go deeper.
    fn child<V: Serialize + ?Sized>(&mut self, key: Option<&str>, value: &V) -> Result<(), Error> {
        let (deeper, ending) = self.advance(key);
        if !ending.is_empty() {
            if let Some(text) = leaf_text(value)? {
                for path in ending {
                    self.found[path].push(text.clone());
                }
            }
        }
        if deeper.is_empty() {
            return Ok(());
        }
        value.serialize(Walk { paths: self.paths, found: &mut *self.found, cursors: deeper })
    }

    /// The walk inside an enum variant written as an object of one field, `variant`. A path
    /// ending at the variant's value reads it as an array, which has no text, or an object.
    fn variant(self, variant: &str, object: bool) -> Result<Self, Error> {
        let (cursors, ending) = self.advance(Some(variant));
        if object && !ending.is_empty() {
            return Err(Error(format!("a declared path ends at the fields of variant {variant}")));
        }
        Ok(Walk { paths: self.paths, found: self.found, cursors })
    }
}

impl<'w, 'p> Serializer for Walk<'w, 'p> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Entries<'w, 'p>;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    fn serialize_bool(self, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_i8(self, _: i8) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_i16(self, _: i16) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_i32(self, _: i32) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_i64(self, _: i64) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_u8(self, _: u8) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_u16(self, _: u16) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_u32(self, _: u32) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_u64(self, _: u64) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_f32(self, _: f32) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_f64(self, _: f64) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_char(self, _: char) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_str(self, _: &str) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_bytes(self, _: &[u8]) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_none(self) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_some<V: Serialize + ?Sized>(self, value: &V) -> Result<(), Error> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, _: &'static str) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_newtype_struct<V: Serialize + ?Sized>(self, _: &'static str, value: &V) -> Result<(), Error> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<V: Serialize + ?Sized>(
        mut self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        value: &V,
    ) -> Result<(), Error> {
        self.child(Some(variant), value)
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self, Error> {
        Ok(self)
    }
    fn serialize_tuple(self, _: usize) -> Result<Self, Error> {
        Ok(self)
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self, Error> {
        Ok(self)
    }
    fn serialize_tuple_variant(self, _: &'static str, _: u32, variant: &'static str, _: usize) -> Result<Self, Error> {
        self.variant(variant, false)
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Entries<'w, 'p>, Error> {
        Ok(Entries { walk: self, key: None })
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self, Error> {
        Ok(self)
    }
    fn serialize_struct_variant(self, _: &'static str, _: u32, variant: &'static str, _: usize) -> Result<Self, Error> {
        self.variant(variant, true)
    }
}

impl ser::SerializeSeq for Walk<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<V: Serialize + ?Sized>(&mut self, value: &V) -> Result<(), Error> {
        self.child(None, value)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeTuple for Walk<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<V: Serialize + ?Sized>(&mut self, value: &V) -> Result<(), Error> {
        self.child(None, value)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeTupleStruct for Walk<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<V: Serialize + ?Sized>(&mut self, value: &V) -> Result<(), Error> {
        self.child(None, value)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeTupleVariant for Walk<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<V: Serialize + ?Sized>(&mut self, value: &V) -> Result<(), Error> {
        self.child(None, value)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeStruct for Walk<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<V: Serialize + ?Sized>(&mut self, key: &'static str, value: &V) -> Result<(), Error> {
        self.child(Some(key), value)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl ser::SerializeStructVariant for Walk<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<V: Serialize + ?Sized>(&mut self, key: &'static str, value: &V) -> Result<(), Error> {
        self.child(Some(key), value)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

/// A walk through a map's entries, keyed as their JSON document keys them.
struct Entries<'w, 'p> {
    walk: Walk<'w, 'p>,
    key: Option<String>,
}

impl ser::SerializeMap for Entries<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_key<K: Serialize + ?Sized>(&mut self, key: &K) -> Result<(), Error> {
        // A key with no text form is matched by `*` alone.
        self.key = key.serialize(Key).ok();
        Ok(())
    }
    fn serialize_value<V: Serialize + ?Sized>(&mut self, value: &V) -> Result<(), Error> {
        let key = self.key.take();
        self.walk.child(key.as_deref(), value)
    }
    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

/// The text of a part a path ends at. A string or a number is read as it is written; an
/// object's text is its JSON, read from the part alone.
fn leaf_text<V: Serialize + ?Sized>(value: &V) -> Result<Option<String>, Error> {
    match value.serialize(Leaf) {
        Ok(text) => Ok(text),
        Err(Unread::Object) => {
            let value = serde_json::to_value(value).map_err(|error| Error(error.to_string()))?;
            Ok(matches!(value, Value::Object(_)).then(|| value.to_string()))
        }
        Err(Unread::Failed(error)) => Err(Error(error)),
    }
}

/// Why [`Leaf`] did not read a part: it is an object, or it could not be serialized.
#[derive(Debug)]
enum Unread {
    Object,
    Failed(String),
}

impl Display for Unread {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Object => formatter.write_str("an object"),
            Self::Failed(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for Unread {}

impl ser::Error for Unread {
    fn custom<M: Display>(message: M) -> Self {
        Self::Failed(message.to_string())
    }
}

/// Reads a scalar part's text: none for an array, a boolean or null.
struct Leaf;

fn number(value: f64) -> Option<String> {
    serde_json::Number::from_f64(value).map(|number| number.to_string())
}

impl Serializer for Leaf {
    type Ok = Option<String>;
    type Error = Unread;
    type SerializeSeq = Skip;
    type SerializeTuple = Skip;
    type SerializeTupleStruct = Skip;
    type SerializeTupleVariant = Impossible<Option<String>, Unread>;
    type SerializeMap = Impossible<Option<String>, Unread>;
    type SerializeStruct = Impossible<Option<String>, Unread>;
    type SerializeStructVariant = Impossible<Option<String>, Unread>;

    fn serialize_bool(self, _: bool) -> Result<Option<String>, Unread> {
        Ok(None)
    }
    fn serialize_i8(self, value: i8) -> Result<Option<String>, Unread> {
        Ok(Some(value.to_string()))
    }
    fn serialize_i16(self, value: i16) -> Result<Option<String>, Unread> {
        Ok(Some(value.to_string()))
    }
    fn serialize_i32(self, value: i32) -> Result<Option<String>, Unread> {
        Ok(Some(value.to_string()))
    }
    fn serialize_i64(self, value: i64) -> Result<Option<String>, Unread> {
        Ok(Some(value.to_string()))
    }
    fn serialize_i128(self, value: i128) -> Result<Option<String>, Unread> {
        Ok(Some(value.to_string()))
    }
    fn serialize_u8(self, value: u8) -> Result<Option<String>, Unread> {
        Ok(Some(value.to_string()))
    }
    fn serialize_u16(self, value: u16) -> Result<Option<String>, Unread> {
        Ok(Some(value.to_string()))
    }
    fn serialize_u32(self, value: u32) -> Result<Option<String>, Unread> {
        Ok(Some(value.to_string()))
    }
    fn serialize_u64(self, value: u64) -> Result<Option<String>, Unread> {
        Ok(Some(value.to_string()))
    }
    fn serialize_u128(self, value: u128) -> Result<Option<String>, Unread> {
        Ok(Some(value.to_string()))
    }
    fn serialize_f32(self, value: f32) -> Result<Option<String>, Unread> {
        Ok(number(f64::from(value)))
    }
    fn serialize_f64(self, value: f64) -> Result<Option<String>, Unread> {
        Ok(number(value))
    }
    fn serialize_char(self, value: char) -> Result<Option<String>, Unread> {
        Ok(Some(value.to_string()))
    }
    fn serialize_str(self, value: &str) -> Result<Option<String>, Unread> {
        Ok(Some(value.to_owned()))
    }
    fn serialize_bytes(self, _: &[u8]) -> Result<Option<String>, Unread> {
        Ok(None)
    }
    fn serialize_none(self) -> Result<Option<String>, Unread> {
        Ok(None)
    }
    fn serialize_some<V: Serialize + ?Sized>(self, value: &V) -> Result<Option<String>, Unread> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<Option<String>, Unread> {
        Ok(None)
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<Option<String>, Unread> {
        Ok(None)
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, variant: &'static str) -> Result<Option<String>, Unread> {
        Ok(Some(variant.to_owned()))
    }
    fn serialize_newtype_struct<V: Serialize + ?Sized>(self, _: &'static str, value: &V) -> Result<Option<String>, Unread> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<V: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &V,
    ) -> Result<Option<String>, Unread> {
        Err(Unread::Object)
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Skip, Unread> {
        Ok(Skip)
    }
    fn serialize_tuple(self, _: usize) -> Result<Skip, Unread> {
        Ok(Skip)
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Skip, Unread> {
        Ok(Skip)
    }
    fn serialize_tuple_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Self::SerializeTupleVariant, Unread> {
        Err(Unread::Object)
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Unread> {
        Err(Unread::Object)
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeStruct, Unread> {
        Err(Unread::Object)
    }
    fn serialize_struct_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Self::SerializeStructVariant, Unread> {
        Err(Unread::Object)
    }
}

/// An array a path ends at: it has no text, so its elements are not read.
struct Skip;

impl ser::SerializeSeq for Skip {
    type Ok = Option<String>;
    type Error = Unread;
    fn serialize_element<V: Serialize + ?Sized>(&mut self, _: &V) -> Result<(), Unread> {
        Ok(())
    }
    fn end(self) -> Result<Option<String>, Unread> {
        Ok(None)
    }
}

impl ser::SerializeTuple for Skip {
    type Ok = Option<String>;
    type Error = Unread;
    fn serialize_element<V: Serialize + ?Sized>(&mut self, _: &V) -> Result<(), Unread> {
        Ok(())
    }
    fn end(self) -> Result<Option<String>, Unread> {
        Ok(None)
    }
}

impl ser::SerializeTupleStruct for Skip {
    type Ok = Option<String>;
    type Error = Unread;
    fn serialize_field<V: Serialize + ?Sized>(&mut self, _: &V) -> Result<(), Unread> {
        Ok(())
    }
    fn end(self) -> Result<Option<String>, Unread> {
        Ok(None)
    }
}

/// Reads a map key as its JSON document writes it.
struct Key;

impl Serializer for Key {
    type Ok = String;
    type Error = Error;
    type SerializeSeq = Impossible<String, Error>;
    type SerializeTuple = Impossible<String, Error>;
    type SerializeTupleStruct = Impossible<String, Error>;
    type SerializeTupleVariant = Impossible<String, Error>;
    type SerializeMap = Impossible<String, Error>;
    type SerializeStruct = Impossible<String, Error>;
    type SerializeStructVariant = Impossible<String, Error>;

    fn serialize_bool(self, value: bool) -> Result<String, Error> {
        Ok(value.to_string())
    }
    fn serialize_i8(self, value: i8) -> Result<String, Error> {
        Ok(value.to_string())
    }
    fn serialize_i16(self, value: i16) -> Result<String, Error> {
        Ok(value.to_string())
    }
    fn serialize_i32(self, value: i32) -> Result<String, Error> {
        Ok(value.to_string())
    }
    fn serialize_i64(self, value: i64) -> Result<String, Error> {
        Ok(value.to_string())
    }
    fn serialize_u8(self, value: u8) -> Result<String, Error> {
        Ok(value.to_string())
    }
    fn serialize_u16(self, value: u16) -> Result<String, Error> {
        Ok(value.to_string())
    }
    fn serialize_u32(self, value: u32) -> Result<String, Error> {
        Ok(value.to_string())
    }
    fn serialize_u64(self, value: u64) -> Result<String, Error> {
        Ok(value.to_string())
    }
    fn serialize_f32(self, _: f32) -> Result<String, Error> {
        Err(Error("a float key".into()))
    }
    fn serialize_f64(self, _: f64) -> Result<String, Error> {
        Err(Error("a float key".into()))
    }
    fn serialize_char(self, value: char) -> Result<String, Error> {
        Ok(value.to_string())
    }
    fn serialize_str(self, value: &str) -> Result<String, Error> {
        Ok(value.to_owned())
    }
    fn serialize_bytes(self, _: &[u8]) -> Result<String, Error> {
        Err(Error("a bytes key".into()))
    }
    fn serialize_none(self) -> Result<String, Error> {
        Err(Error("a null key".into()))
    }
    fn serialize_some<V: Serialize + ?Sized>(self, value: &V) -> Result<String, Error> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<String, Error> {
        Err(Error("a null key".into()))
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<String, Error> {
        Err(Error("a null key".into()))
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, variant: &'static str) -> Result<String, Error> {
        Ok(variant.to_owned())
    }
    fn serialize_newtype_struct<V: Serialize + ?Sized>(self, _: &'static str, value: &V) -> Result<String, Error> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<V: Serialize + ?Sized>(self, _: &'static str, _: u32, _: &'static str, _: &V) -> Result<String, Error> {
        Err(Error("a compound key".into()))
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, Error> {
        Err(Error("a compound key".into()))
    }
    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, Error> {
        Err(Error("a compound key".into()))
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeTupleStruct, Error> {
        Err(Error("a compound key".into()))
    }
    fn serialize_tuple_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Self::SerializeTupleVariant, Error> {
        Err(Error("a compound key".into()))
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Error> {
        Err(Error("a compound key".into()))
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeStruct, Error> {
        Err(Error("a compound key".into()))
    }
    fn serialize_struct_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Self::SerializeStructVariant, Error> {
        Err(Error("a compound key".into()))
    }
}
