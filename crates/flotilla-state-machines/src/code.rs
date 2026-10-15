//! Stable codes and typed edge dispatch.
//!
//! States and edge kinds each carry an explicit code next to their name. A
//! rename changes the name, never the code, so anything keyed by code (the
//! storage derive, #3208) survives it. Codes are pinned by tests.
//!
//! [`edges!`](crate::edges) declares an edge enum whose variants each wrap
//! their own argument struct. It derives the fieldless kind enum with strum's
//! `EnumDiscriminants`, so edge names come from the kind, and it implements
//! [`EdgeArg`] for every argument struct. [`EdgeDef`](crate::EdgeDef) is typed
//! on that struct, so each gate receives `&Deliver` with no re-match.

use std::{fmt::Debug, hash::Hash};

/// A fieldless enum whose variants carry explicit, stable codes.
pub trait Coded: Copy + Eq + Ord + Hash + Debug + 'static {
    /// Every variant, in declaration order.
    const ALL: &'static [Self];
    /// The stable code; unique within the enum and never reused.
    fn code(self) -> u16;
    /// The display name: the variant name for states, snake_case for edge kinds.
    fn name(self) -> &'static str;
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

/// Declares a unit state enum with stable codes.
///
/// ```
/// flotilla_state_machines::states! {
///     pub enum Door { Shut = 1, Open = 2 }
/// }
/// use flotilla_state_machines::Coded;
/// assert_eq!((Door::Open.name(), Door::Open.code()), ("Open", 2));
/// ```
#[macro_export]
macro_rules! states {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident = $code:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        $vis enum $name {
            $( $(#[$vmeta])* $variant ),+
        }

        impl $crate::Coded for $name {
            const ALL: &'static [Self] = &[$(Self::$variant),+];
            fn code(self) -> u16 {
                match self { $(Self::$variant => $code),+ }
            }
            fn name(self) -> &'static str {
                match self { $(Self::$variant => stringify!($variant)),+ }
            }
        }
    };
}

/// Declares an edge enum, its strum-derived kind enum and the typed dispatch.
///
/// Each variant wraps its own argument struct, and each argument struct
/// belongs to one variant. The kind enum is named after `=>`, its names are
/// the snake_case variant names, and its codes are the literals given here.
/// The expansion uses strum's derives, so the calling crate depends on `strum`.
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
            derive(PartialOrd, Ord, Hash, strum::IntoStaticStr, strum::VariantArray),
            strum(serialize_all = "snake_case")
        )]
        $vis enum $name {
            $( $(#[$vmeta])* $variant($arg) ),+
        }

        impl $crate::Coded for $kind {
            const ALL: &'static [Self] = <Self as strum::VariantArray>::VARIANTS;
            fn code(self) -> u16 {
                match self { $(Self::$variant => $code),+ }
            }
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
