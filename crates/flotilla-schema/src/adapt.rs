//! Field adapters: how generated code lays out, encodes and decodes one field for its attribute.
//!
//! A field's storage depends on its attribute as well as its type (`String` is text, but
//! `#[store(intern)] String` is a `strings` id), so generated code goes through an adapter chosen
//! by the attribute rather than through the type's own [`Columns`]. Each adapter has the shape of
//! `Columns` with the field type as a parameter.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    codec::{DecodeError, Decoder, EncodeError, Encoder},
    descriptor::{ColumnType, TableRole, RECORD_VERSION},
    frozen::{Freezable, Frozen},
    inline::{Cbor, INLINE_LIMITS},
    layout::Layout,
    records::Records,
    traits::{Columns, Element, Flat, Intern, KeyColumn},
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

/// `#[store(flatten)] Option<T>`: the struct's columns, nullable. The value is present when its
/// first required column is non-null (#3392 ruling G5: no presence flag), and one CHECK keeps its
/// columns all set or all null.
#[doc(hidden)]
pub struct AsOptionalFlat;

impl<T: Flat> Adapter<Option<T>> for AsOptionalFlat {
    const TABLES: usize = T::TABLES;

    fn layout(layout: &mut Layout, name: &str, _required: bool) {
        layout.optional(|layout| T::layout(layout, name, true));
    }

    fn encode(value: &Option<T>, e: &mut Encoder) -> Result<(), EncodeError> {
        match value {
            Some(value) => value.encode(e),
            None => T::encode_absent(e),
        }
    }

    fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
        T::encode_absent(e)
    }

    /// `Some(None)` when the struct's columns are all null: the value is absent.
    fn decode(d: &mut Decoder) -> Result<Option<Option<T>>, DecodeError> {
        T::decode(d).map(Some)
    }

    fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
        T::decode_absent(d)
    }
}

fn null(d: &mut Decoder) -> Result<(), DecodeError> {
    match d.take()? {
        Value::Null => Ok(()),
        other => Err(DecodeError::unexpected("null", &other)),
    }
}

/// `#[store(frozen)] Frozen<T>`: the content hash, with the bytes recorded for `objects`.
#[doc(hidden)]
pub struct AsFrozen;

impl<T: Freezable> Adapter<Frozen<T>> for AsFrozen {
    const TABLES: usize = 0;

    fn layout(layout: &mut Layout, name: &str, required: bool) {
        layout.column(name, ColumnType::Frozen, required);
    }

    fn encode(value: &Frozen<T>, e: &mut Encoder) -> Result<(), EncodeError> {
        e.put_object(value.hash(), value.bytes());
        Ok(())
    }

    fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
        e.put(Value::Null);
        Ok(())
    }

    fn decode(d: &mut Decoder) -> Result<Option<Frozen<T>>, DecodeError> {
        match d.take()? {
            Value::Null => Ok(None),
            Value::Object(hash) => Frozen::from_stored(hash, d.object(&hash)?).map(Some),
            other => Err(DecodeError::unexpected("object", &other)),
        }
    }

    fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
        null(d)
    }
}

/// `#[store(frozen)] Option<Frozen<T>>`: a nullable hash.
#[doc(hidden)]
pub struct AsOptionalFrozen;

impl<T: Freezable> Adapter<Option<Frozen<T>>> for AsOptionalFrozen {
    const TABLES: usize = 0;

    // Always nullable: `None` is a stored NULL, whatever the enclosing field's requirement.
    fn layout(layout: &mut Layout, name: &str, _required: bool) {
        layout.column(name, ColumnType::Frozen, false);
    }

    fn encode(value: &Option<Frozen<T>>, e: &mut Encoder) -> Result<(), EncodeError> {
        match value {
            Some(value) => AsFrozen::encode(value, e),
            None => <AsFrozen as Adapter<Frozen<T>>>::encode_absent(e),
        }
    }

    fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
        <AsFrozen as Adapter<Frozen<T>>>::encode_absent(e)
    }

    fn decode(d: &mut Decoder) -> Result<Option<Option<Frozen<T>>>, DecodeError> {
        <AsFrozen as Adapter<Frozen<T>>>::decode(d).map(Some)
    }

    fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
        null(d)
    }
}

/// `#[store(cbor)]`: the value's canonical CBOR (ADR 0063), inline in one BLOB column. Any serde
/// type, `Option` included: `None` is the CBOR document `null`, not a NULL column, so the column
/// is null only under an inactive variant.
#[doc(hidden)]
pub struct AsCbor;

impl<T: Cbor> Adapter<T> for AsCbor {
    const TABLES: usize = 0;

    fn layout(layout: &mut Layout, name: &str, required: bool) {
        layout.column(name, ColumnType::Cbor, required);
    }

    fn encode(value: &T, e: &mut Encoder) -> Result<(), EncodeError> {
        let bytes = flotilla_cbor::to_vec(value, &INLINE_LIMITS)
            .map_err(|error| EncodeError::Cbor { column: String::new(), kind: error.kind() })?;
        e.put(Value::Blob(bytes));
        Ok(())
    }

    fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
        e.put(Value::Null);
        Ok(())
    }

    fn decode(d: &mut Decoder) -> Result<Option<T>, DecodeError> {
        match d.take()? {
            Value::Null => Ok(None),
            Value::Blob(bytes) => flotilla_cbor::from_slice(&bytes, &INLINE_LIMITS)
                .map(Some)
                .map_err(|error| DecodeError::Cbor { column: String::new(), kind: error.kind() }),
            other => Err(DecodeError::unexpected("CBOR document", &other)),
        }
    }

    fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
        null(d)
    }
}

/// `#[store(child)]`: a child table. A `Vec` is keyed by `pos`, a `BTreeMap` by its key in column
/// `key`, a `BTreeSet` by its value in column `value`. Rows change freely through the owner's write.
///
/// A collection is never absent: an empty one and one under an inactive variant both have no rows.
#[doc(hidden)]
pub struct AsChild;

impl<T: Element> Adapter<Vec<T>> for AsChild {
    const TABLES: usize = 1 + T::TABLES;

    fn layout(layout: &mut Layout, name: &str, _required: bool) {
        layout_list::<T>(layout, name, TableRole::Child);
    }

    fn encode(value: &Vec<T>, e: &mut Encoder) -> Result<(), EncodeError> {
        encode_list(value, e)
    }

    fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
        e.table(T::TABLES, |_| Ok(()))
    }

    fn decode(d: &mut Decoder) -> Result<Option<Vec<T>>, DecodeError> {
        decode_list(d).map(Some)
    }

    fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
        d.table(T::TABLES, |d| d.no_rows())
    }
}

/// `#[store(child)]` on a datum of a label-plus-data state (`#[store(labels = L)]`): a map or set
/// table like [`AsChild`]'s, in the `state` role: its rows change one at a time, through edges.
#[doc(hidden)]
pub struct AsStateChild;

/// The map and set tables of `$adapter`, in `$role`.
macro_rules! keyed_tables {
    ($adapter:ty, $role:expr) => {
        impl<K: KeyColumn + Ord, V: Element> Adapter<BTreeMap<K, V>> for $adapter {
            const TABLES: usize = 1 + V::TABLES;

            fn layout(layout: &mut Layout, name: &str, _required: bool) {
                layout.table(name, $role, |layout| {
                    layout.key(|layout| K::layout(layout, "key", true));
                    V::layout_row(layout);
                });
            }

            fn encode(value: &BTreeMap<K, V>, e: &mut Encoder) -> Result<(), EncodeError> {
                e.table(V::TABLES, |e| {
                    for (key, row) in value {
                        e.row(|e| {
                            e.key(|e| key.encode(e).map_err(|error| error.at("key")))?;
                            row.encode_row(e)
                        })?;
                    }
                    Ok(())
                })
            }

            fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
                e.table(V::TABLES, |_| Ok(()))
            }

            fn decode(d: &mut Decoder) -> Result<Option<BTreeMap<K, V>>, DecodeError> {
                d.table(V::TABLES, |d| {
                    let mut map = BTreeMap::new();
                    while let Some((key, row)) = d.row(|d| Ok((decode_key::<K>(d, "key")?, V::decode_row(d)?)))? {
                        map.insert(key, row);
                    }
                    Ok(Some(map))
                })
            }

            fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
                d.table(V::TABLES, |d| d.no_rows())
            }
        }

        impl<K: KeyColumn + Ord> Adapter<BTreeSet<K>> for $adapter {
            const TABLES: usize = 1;

            fn layout(layout: &mut Layout, name: &str, _required: bool) {
                layout.table(name, $role, |layout| layout.key(|layout| K::layout(layout, "value", true)));
            }

            fn encode(value: &BTreeSet<K>, e: &mut Encoder) -> Result<(), EncodeError> {
                e.table(0, |e| {
                    for key in value {
                        e.row(|e| e.key(|e| key.encode(e).map_err(|error| error.at("value"))))?;
                    }
                    Ok(())
                })
            }

            fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
                e.table(0, |_| Ok(()))
            }

            fn decode(d: &mut Decoder) -> Result<Option<BTreeSet<K>>, DecodeError> {
                d.table(0, |d| {
                    let mut set = BTreeSet::new();
                    while let Some(key) = d.row(|d| decode_key::<K>(d, "value"))? {
                        set.insert(key);
                    }
                    Ok(Some(set))
                })
            }

            fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
                d.table(0, |d| d.no_rows())
            }
        }
    };
}

keyed_tables!(AsChild, TableRole::Child);
keyed_tables!(AsStateChild, TableRole::State);

/// `#[store(records)]`: rows keyed by `pos`, appended and never removed; appended and changed only
/// by edges (`Edge`), never by a spec edit.
#[doc(hidden)]
pub struct AsRecords;

/// `#[store(history)]`: rows keyed by `pos`, appended only and never changed.
#[doc(hidden)]
pub struct AsHistory;

macro_rules! records_adapter {
    ($adapter:ty, $role:expr) => {
        impl<T: Element> Adapter<Records<T>> for $adapter {
            const TABLES: usize = 1 + T::TABLES;

            fn layout(layout: &mut Layout, name: &str, _required: bool) {
                layout.table(name, $role, |layout| {
                    layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
                    layout.column(RECORD_VERSION, ColumnType::Integer, true);
                    T::layout_row(layout);
                });
            }

            fn encode(value: &Records<T>, e: &mut Encoder) -> Result<(), EncodeError> {
                encode_records(value, e)
            }

            fn encode_absent(e: &mut Encoder) -> Result<(), EncodeError> {
                e.table(T::TABLES, |_| Ok(()))
            }

            fn decode(d: &mut Decoder) -> Result<Option<Records<T>>, DecodeError> {
                decode_records(d).map(Some)
            }

            fn decode_absent(d: &mut Decoder) -> Result<(), DecodeError> {
                d.table(T::TABLES, |d| d.no_rows())
            }
        }
    };
}

records_adapter!(AsRecords, TableRole::Records);
records_adapter!(AsHistory, TableRole::History);

fn layout_list<T: Element>(layout: &mut Layout, name: &str, role: TableRole) {
    layout.table(name, role, |layout| {
        layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
        T::layout_row(layout);
    });
}

/// Rows keyed by their index.
fn encode_list<T: Element>(rows: &[T], e: &mut Encoder) -> Result<(), EncodeError> {
    e.table(T::TABLES, |e| {
        for (pos, row) in rows.iter().enumerate() {
            e.row(|e| {
                e.key(|e| {
                    // A list long enough to overflow i64 cannot be in memory.
                    e.put(Value::Int(i64::try_from(pos).expect("list positions fit i64")));
                    Ok(())
                })?;
                row.encode_row(e)
            })?;
        }
        Ok(())
    })
}

/// Rows keyed by their position, each with its version stamp before the record's columns.
fn encode_records<T: Element>(records: &Records<T>, e: &mut Encoder) -> Result<(), EncodeError> {
    e.table(T::TABLES, |e| {
        for row in records {
            e.row(|e| {
                e.key(|e| {
                    e.put(Value::Int(i64::from(row.position.0)));
                    Ok(())
                })?;
                row.version.encode(e).map_err(|error| error.at(RECORD_VERSION))?;
                row.record.encode_row(e)
            })?;
        }
        Ok(())
    })
}

/// Rows in position order, refusing a gap or a negative position: positions are dense indexes.
fn decode_list<T: Element>(d: &mut Decoder) -> Result<Vec<T>, DecodeError> {
    d.table(T::TABLES, |d| {
        let mut rows = Vec::new();
        while let Some(row) = d.row(|d| {
            let expected = rows.len();
            match d.key(Decoder::take)? {
                Value::Int(found) if usize::try_from(found).is_ok_and(|found| found == expected) => {}
                Value::Int(found) => return Err(DecodeError::Position { column: "pos".to_owned(), expected, found }),
                other => return Err(DecodeError::unexpected("integer", &other).at("pos")),
            }
            T::decode_row(d)
        })? {
            rows.push(row);
        }
        Ok(rows)
    })
}

/// Records in position order with their stored version stamps. A position outside `u32` is
/// `OutOfRange` (before the density check, so it is reported as such wherever it is stored); a gap
/// is `Position`.
fn decode_records<T: Element>(d: &mut Decoder) -> Result<Records<T>, DecodeError> {
    d.table(T::TABLES, |d| {
        let mut records = Records::new();
        while let Some((version, record)) = d.row(|d| {
            let expected = records.len();
            match d.key(Decoder::take)? {
                Value::Int(found) if u32::try_from(found).is_err() => {
                    return Err(DecodeError::OutOfRange { column: "pos".to_owned(), value: found })
                }
                Value::Int(found) if usize::try_from(found).is_ok_and(|found| found == expected) => {}
                Value::Int(found) => return Err(DecodeError::Position { column: "pos".to_owned(), expected, found }),
                other => return Err(DecodeError::unexpected("integer", &other).at("pos")),
            }
            let version = u64::decode(d).map_err(|error| error.at(RECORD_VERSION))?.ok_or_else(|| DecodeError::missing(RECORD_VERSION))?;
            Ok((version, T::decode_row(d)?))
        })? {
            records.append(version, record);
        }
        Ok(records)
    })
}

fn decode_key<K: KeyColumn>(d: &mut Decoder, column: &str) -> Result<K, DecodeError> {
    d.key(K::decode).map_err(|error| error.at(column))?.ok_or_else(|| DecodeError::missing(column))
}
