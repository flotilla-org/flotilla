//! Field adapters: how generated code lays out, encodes and decodes one field for its attribute.
//!
//! A field's storage depends on its attribute as well as its type (`String` is text, but
//! `#[store(intern)] String` is a `strings` id), so generated code goes through an adapter chosen
//! by the attribute rather than through the type's own [`Columns`]. Each adapter has the shape of
//! `Columns` with the field type as a parameter.

use crate::{
    codec::{DecodeError, Decoder, EncodeError, Encoder},
    descriptor::ColumnType,
    layout::Layout,
    traits::{Columns, Flat, Intern},
    value::Value,
};

/// `Columns` for a field of type `F`, as stored under one attribute.
#[doc(hidden)]
pub trait Adapter<F> {
    const TABLES: usize;
    fn layout(layout: &mut Layout, name: &str, required: bool);
    fn encode(value: &F, e: &mut Encoder) -> Result<(), EncodeError>;
    fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError>;
    fn decode(d: &mut Decoder) -> Result<Option<F>, DecodeError>;
    fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError>;
}

/// The type's own columns: unattributed columns, `flatten` of a struct and `state`.
#[doc(hidden)]
pub struct AsColumns;

impl<F: Columns> Adapter<F> for AsColumns {
    const TABLES: usize = F::TABLES;

    fn layout(layout: &mut Layout, name: &str, required: bool) {
        F::layout(layout, name, required);
    }

    fn encode(value: &F, e: &mut Encoder) -> Result<(), EncodeError> {
        value.encode(e)
    }

    fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
        F::encode_absent(e)
    }

    fn decode(d: &mut Decoder) -> Result<Option<F>, DecodeError> {
        F::decode(d)
    }

    fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
        F::decode_absent(d)
    }
}

/// `#[store(intern)]` on `String` or a newtype of it: an id in `strings`.
#[doc(hidden)]
pub struct AsInterned;

impl<F: Intern> Adapter<F> for AsInterned {
    const TABLES: usize = 0;

    fn layout(layout: &mut Layout, name: &str, required: bool) {
        layout.column(name, ColumnType::Interned, required);
    }

    fn encode(value: &F, e: &mut Encoder) -> Result<(), EncodeError> {
        e.put(Value::Str(value.as_str().to_owned()));
        Ok(())
    }

    fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
        e.put(Value::Null);
        Ok(())
    }

    fn decode(d: &mut Decoder) -> Result<Option<F>, DecodeError> {
        match d.take()? {
            Value::Null => Ok(None),
            Value::Str(string) => Ok(Some(F::from_string(string))),
            other => Err(DecodeError::unexpected("interned string", &other)),
        }
    }

    fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
        null(d)
    }
}

/// `#[store(intern)] Option<T>`: a nullable `strings` id.
#[doc(hidden)]
pub struct AsOptionalInterned;

impl<T: Intern> Adapter<Option<T>> for AsOptionalInterned {
    const TABLES: usize = 0;

    fn layout(layout: &mut Layout, name: &str, _required: bool) {
        layout.column(name, ColumnType::Interned, false);
    }

    fn encode(value: &Option<T>, e: &mut Encoder) -> Result<(), EncodeError> {
        match value {
            Some(value) => AsInterned::encode(value, e),
            None => <AsInterned as Adapter<T>>::encode_absent(e),
        }
    }

    fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
        <AsInterned as Adapter<T>>::encode_absent(e)
    }

    fn decode(d: &mut Decoder) -> Result<Option<Option<T>>, DecodeError> {
        <AsInterned as Adapter<T>>::decode(d).map(Some)
    }

    fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
        null(d)
    }
}

/// `#[store(flatten)] Option<T>`: a `<field>_present` flag guarding the struct's columns.
///
/// The flag is 1 when the value is present and 0 when it is `None`; it is null only when the
/// enclosing value is itself absent (an inactive variant), so absence at each level stays distinct.
#[doc(hidden)]
pub struct AsOptionalFlat;

impl<T: Flat> Adapter<Option<T>> for AsOptionalFlat {
    const TABLES: usize = T::TABLES;

    fn layout(layout: &mut Layout, name: &str, required: bool) {
        let present = Layout::join(name, "present");
        layout.column(&present, ColumnType::Bool, required);
        layout.guard(&present, 1, |layout| T::layout(layout, name, true));
    }

    fn encode(value: &Option<T>, e: &mut Encoder) -> Result<(), EncodeError> {
        match value {
            Some(value) => {
                e.put(Value::Int(1));
                value.encode(e)
            }
            None => {
                e.put(Value::Int(0));
                T::encode_absent(e)
            }
        }
    }

    fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
        e.put(Value::Null);
        T::encode_absent(e)
    }

    fn decode(d: &mut Decoder) -> Result<Option<Option<T>>, DecodeError> {
        match d.take().map_err(|error| error.at("present"))? {
            Value::Null => T::decode_absent(d).map(|()| None),
            Value::Int(0) => T::decode_absent(d).map(|()| Some(None)),
            Value::Int(1) => Ok(Some(Some(T::decode(d)?.ok_or_else(|| DecodeError::missing(""))?))),
            Value::Int(other) => Err(DecodeError::OutOfRange { column: "present".to_owned(), value: other }),
            other => Err(DecodeError::unexpected("boolean", &other).at("present")),
        }
    }

    fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
        null(d).map_err(|error| error.at("present"))?;
        T::decode_absent(d)
    }
}

fn null(d: &mut Decoder) -> Result<(), DecodeError> {
    match d.take()? {
        Value::Null => Ok(()),
        other => Err(DecodeError::unexpected("null", &other)),
    }
}
