//! State values, stable codes and typed edge dispatch.
//!
//! A machine's state is a value: a label plus the data that label carries, as
//! in the Quint specs (`Open { promises, quota }`). [`states!`](crate::states)
//! declares the state enum with named data on each variant and derives the
//! fieldless label enum with strum's `EnumDiscriminants`. The label is what
//! listings, indexes and diagrams use; the data is what gates read and edges
//! change. [`StateValue::data`] names every datum, so the engine can check
//! that an edge changes only the data its declaration names.
//!
//! Labels and edge kinds each carry an explicit code next to their name. Both
//! macros derive `flotilla_schema::Stored` on the derived enum, so each is a
//! schema code enum (`flotilla_schema::CodeEnum`) whose codes are the ones
//! declared here, and the derive refuses a repeated code at compile time. A
//! rename changes the name, never the code, so anything keyed by code (stored
//! state columns and edge logs) survives it. Codes are pinned by tests.
//!
//! [`edges!`](crate::edges) declares an edge enum whose variants each wrap
//! their own argument struct. It derives the fieldless kind enum the same way,
//! so edge names come from the kind, and it implements [`EdgeArg`] for every
//! argument struct. [`EdgeDef`](crate::EdgeDef) is typed on that struct, so
//! each gate receives `&Deliver` with no re-match.

use std::{any::Any, collections::BTreeSet, fmt::Debug, hash::Hash};

use flotilla_schema::CodeEnum;

/// A fieldless enum whose variants carry explicit, stable codes: a state
/// label or an edge kind. The code is [`CodeEnum::code`], unique within the
/// enum and never reused; this trait adds what the schema trait lacks.
pub trait Coded: CodeEnum + Copy + Eq + Ord + Hash + Debug + 'static {
    /// Every variant, in declaration order.
    const ALL: &'static [Self];
    /// The display name: the variant name for labels, snake_case for edge kinds.
    fn name(self) -> &'static str;
}

/// One named datum of a state value, compared without knowing its type.
pub trait Datum: Debug {
    /// Whether `other` is a datum of the same type and value.
    fn same(&self, other: &dyn Datum) -> bool;
    fn as_any(&self) -> &dyn Any;
}

impl<T: Eq + Debug + 'static> Datum for T {
    fn same(&self, other: &dyn Datum) -> bool {
        other.as_any().downcast_ref::<T>() == Some(self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A machine's state: a label and the named data it carries.
pub trait StateValue: Clone + Debug + PartialEq + 'static {
    type Label: Coded;
    fn label(&self) -> Self::Label;
    /// Every datum this value carries, by name, in declaration order.
    fn data(&self) -> Vec<(&'static str, &dyn Datum)>;
    /// The names of the data a label carries, in declaration order.
    fn fields(label: Self::Label) -> &'static [&'static str];
}

/// The data a move from `previous` to `next` changes: every datum `next`
/// carries that `previous` lacks or holds with another value. Data a label
/// change leaves behind goes with the label (`Realised { identity }` to
/// `Gone` changes nothing).
pub fn changed<S: StateValue>(previous: &S, next: &S) -> BTreeSet<&'static str> {
    let before = previous.data();
    next.data()
        .into_iter()
        .filter(|(name, value)| !before.iter().any(|(old, held)| old == name && held.same(*value)))
        .map(|(name, _)| name)
        .collect()
}

/// An edge enum: one variant per edge, each wrapping its argument struct.
pub trait Edge: Clone + Debug + 'static {
    type Kind: Coded;
    fn kind(&self) -> Self::Kind;
}

/// The argument struct of exactly one edge variant.
pub trait EdgeArg: Clone + Debug + Sized + 'static {
    type Edge: Edge;
    const KIND: <Self::Edge as Edge>::Kind;
    /// This argument when `edge` is its variant.
    fn project(edge: &Self::Edge) -> Option<&Self>;
}

/// Declares a state enum, its strum-derived label enum and stable codes.
///
/// Each variant may carry named data in braces. The label enum is named after
/// `=>`, its names are the variant names, and its codes are the literals given
/// here: the label derives `flotilla_schema::Stored` as a code enum with
/// those codes. The state enum derives `Stored` too, as a label-plus-data
/// state (`#[store(labels = Label)]`, #3373 ruling 1), so a stored kind holds
/// it under `#[store(state)]`: one label column, `<field>_since`, and each
/// datum once, shared by name across the labels that carry it. Each datum
/// takes its storage form in the input: no attribute (its own columns),
/// `#[store(child)]` (a `BTreeMap` or `BTreeSet`, spelled out, changed row by
/// row), `#[store(intern)]` or `#[store(flatten)]`; a datum has one type and
/// form in every label carrying it. The expansion uses strum's and the schema
/// crate's derives, so the calling crate depends on `strum` and
/// `flotilla-schema`, and every datum type must be `Clone + Debug + Eq +
/// 'static` and storable in its form.
///
/// ```
/// flotilla_state_machines::states! {
///     pub enum Door => DoorLabel { Shut = 1, Open { #[store(intern)] by: String } = 2 }
/// }
/// use flotilla_schema::CodeEnum;
/// use flotilla_state_machines::{Coded, StateValue};
/// let open = Door::Open { by: "ann".into() };
/// assert_eq!((open.label().name(), open.label().code()), ("Open", 2));
/// assert_eq!(Door::fields(DoorLabel::Open), ["by"]);
/// ```
#[macro_export]
macro_rules! states {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident => $label:ident {
            $(
                $(#[$vmeta:meta])*
                $variant:ident $({ $( $(#[$fmeta:meta])* $field:ident : $ty:ty ),* $(,)? })? = $code:literal
            ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, strum::EnumDiscriminants, ::flotilla_schema::Stored)]
        #[strum_discriminants(
            name($label),
            derive(PartialOrd, Ord, Hash, strum::IntoStaticStr, strum::VariantArray, ::flotilla_schema::Stored)
        )]
        #[store(labels = $label)]
        $vis enum $name {
            $(
                $(#[$vmeta])*
                #[strum_discriminants(store(code = $code))]
                #[store(code = $code)]
                $variant $({ $( $(#[$fmeta])* $field: $ty ),* })?
            ),+
        }

        impl $crate::Coded for $label {
            const ALL: &'static [Self] = <Self as strum::VariantArray>::VARIANTS;
            fn name(self) -> &'static str {
                self.into()
            }
        }

        impl $crate::StateValue for $name {
            type Label = $label;
            fn label(&self) -> $label {
                self.into()
            }
            fn data(&self) -> Vec<(&'static str, &dyn $crate::Datum)> {
                match self {
                    $( Self::$variant { $($($field),*)? } => vec![$($( (stringify!($field), $field as &dyn $crate::Datum) ),*)?], )+
                }
            }
            fn fields(label: $label) -> &'static [&'static str] {
                match label {
                    $( $label::$variant => &[$($(stringify!($field)),*)?], )+
                }
            }
        }
    };
}

/// Declares an edge enum, its strum-derived kind enum and the typed dispatch.
///
/// Each variant wraps its own argument struct, and each argument struct
/// belongs to one variant. The kind enum is named after `=>`, its names are
/// the snake_case variant names, and its codes are the literals given here:
/// the kind derives `flotilla_schema::Stored` as a code enum with those codes.
/// The edge enum implements serde's `Serialize` and `Deserialize` as
/// `[code, arguments]`, so it can be an [`EdgePayload`](crate::EdgePayload)'s
/// document whose shape does not depend on variant names, and every argument
/// struct must implement both too. The expansion uses strum's, the schema
/// crate's and serde's derives, so the calling crate depends on `strum`,
/// `flotilla-schema` and `serde`.
#[macro_export]
macro_rules! edges {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident => $kind:ident {
            $( $(#[$vmeta:meta])* $variant:ident($arg:ty) = $code:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, strum::EnumDiscriminants)]
        #[strum_discriminants(
            name($kind),
            derive(PartialOrd, Ord, Hash, strum::IntoStaticStr, strum::VariantArray, ::flotilla_schema::Stored),
            strum(serialize_all = "snake_case")
        )]
        $vis enum $name {
            $( $(#[$vmeta])* #[strum_discriminants(store(code = $code))] $variant($arg) ),+
        }

        impl $crate::Coded for $kind {
            const ALL: &'static [Self] = <Self as strum::VariantArray>::VARIANTS;
            fn name(self) -> &'static str {
                self.into()
            }
        }

        impl $crate::Edge for $name {
            type Kind = $kind;
            fn kind(&self) -> $kind {
                self.into()
            }
        }

        // Serde sees the edge as `[code, arguments]`: its stable code, never its variant name, so
        // renaming an edge changes no stored payload (#3373 ruling 6).
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeTuple;
                let mut tuple = serializer.serialize_tuple(2)?;
                match self {
                    $( $name::$variant(arg) => {
                        tuple.serialize_element(&($code as i64))?;
                        tuple.serialize_element(arg)?;
                    } )+
                }
                tuple.end()
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct Visitor;
                impl<'de> serde::de::Visitor<'de> for Visitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        f.write_str(concat!("a ", stringify!($name), " as [code, arguments]"))
                    }
                    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<$name, A::Error> {
                        let code: i64 = seq.next_element()?.ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
                        match code {
                            $( $code => {
                                let arg: $arg = seq.next_element()?.ok_or_else(|| serde::de::Error::invalid_length(1, &self))?;
                                Ok($name::$variant(arg))
                            } )+
                            other => Err(serde::de::Error::custom(format_args!("unknown {} code {other}", stringify!($name)))),
                        }
                    }
                }
                deserializer.deserialize_tuple(2, Visitor)
            }
        }

        $(
            impl $crate::EdgeArg for $arg {
                type Edge = $name;
                const KIND: $kind = $kind::$variant;
                #[allow(unreachable_patterns)]
                fn project(edge: &$name) -> Option<&Self> {
                    match edge {
                        $name::$variant(arg) => Some(arg),
                        _ => None,
                    }
                }
            }

            impl From<$arg> for $name {
                fn from(arg: $arg) -> Self {
                    $name::$variant(arg)
                }
            }
        )+
    };
}
