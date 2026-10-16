//! Addresses: one way to name a thing by its natural key (ADR 0066).
//!
//! An [`Address`] names a thing whether or not flotilla keeps a record of it:
//! flotilla's own objects (`project:flotilla/convoy:demo/role:coder`) and
//! outside objects (`forge:github/repo:flotilla-org%2Fflotilla/cr:42`). It is
//! a list of typed `kind:value` segments over the closed [`Kind`] enum, each
//! value percent-encoded canonically, so one address has exactly one
//! spelling.
//!
//! - Canonical text parses with no lookups ([`Address::from_str`]) and refuses
//!   any other spelling.
//! - Typed input (short forms, URLs, hosts, mixed case) goes through
//!   [`resolve`], which may need a [`ForgeLookup`].
//! - Normalisation happens only in the constructors: a forge is named by its
//!   id, a repository takes its [`ForgeKind`]'s case, and numbers are refused
//!   with a sign or leading zero.
//! - [`record_name`] is the only join from an address to a stored record.
//! - A [`Reference`] is an address plus a [`Revision`]; a [`Condition`] and a
//!   [`Template`] compare an addressed field with a literal.
//!
//! Addresses are namespace-relative: a flotilla object is named within the
//! namespace of whatever holds the address, and outside objects have no
//! namespace. This crate depends on no other flotilla crate.
//!
//! [`Address::from_str`]: std::str::FromStr

mod address;
mod condition;
mod encoding;
mod forge;
mod kind;
mod lenient;
mod record;
mod reference;
#[cfg(test)]
mod tests;

use std::fmt;

pub use address::{Address, Segment};
pub use condition::{Condition, FieldPath, Operator, Template, Variable};
pub use encoding::{decode_canonical, encode};
pub use forge::{BuiltinForges, Forge, ForgeKind, ForgeLookup, GITHUB_FORGE_ID, GITHUB_HOSTS};
pub use kind::Kind;
pub use lenient::resolve;
pub use record::record_name;
pub use reference::{Reference, Revision};

/// Why text did not parse or a value was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No segments.
    Empty,
    /// A segment without `kind:value`.
    MalformedSegment(String),
    /// A segment kind outside the closed enum.
    UnknownKind(String),
    /// A segment whose kind may not follow the one before it.
    Shape {
        kind: Kind,
        parent: Option<Kind>,
    },
    /// A value the kind does not admit.
    InvalidValue {
        kind: Kind,
        value: String,
        reason: &'static str,
    },
    /// A truncated or non-hex escape, or one that decodes to invalid UTF-8.
    InvalidEscape(String),
    /// Valid escapes that are not the canonical spelling.
    NonCanonicalEncoding(String),
    /// `of:` anywhere but directly after an `artifact:` segment.
    OfNotAfterArtifact,
    /// A segment after an artifact's `of:` subject.
    OfNotLast,
    /// An `artifact:` segment with no `of:` subject.
    ArtifactWithoutSubject,
    /// A `repo` segment built without its forge's case rule.
    RepositoryNeedsForge,
    /// A forge id or host that no Forge resolves.
    UnresolvedForge(String),
    /// Lenient input in no recognised form.
    Unrecognised(String),
    /// A reference without `@<revision>`.
    MissingRevision(String),
    EmptyRevision,
    UnknownOperator(String),
    InvalidVariable(String),
    /// A JSON string where a bare word would do, or not as `serde_json` prints it.
    NonCanonicalQuoting(String),
    Syntax {
        input: String,
        position: usize,
        expected: &'static str,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Empty => f.write_str("an address needs at least one segment"),
            Error::MalformedSegment(segment) => write!(f, "address segment `{segment}` is not `kind:value`"),
            Error::UnknownKind(kind) => write!(f, "unknown address kind `{kind}`"),
            Error::Shape { kind, parent: None } => write!(f, "an address cannot start with `{kind}`"),
            Error::Shape { kind, parent: Some(parent) } => write!(f, "`{kind}` cannot follow `{parent}` in an address"),
            Error::InvalidValue { kind, value, reason } => write!(f, "invalid {kind} `{value}`: {reason}"),
            Error::InvalidEscape(value) => write!(f, "invalid percent-encoding in `{value}`"),
            Error::NonCanonicalEncoding(value) => {
                write!(f, "`{value}` is not canonically encoded: only `A-Z a-z 0-9 - . _ ~` stay bare, escapes use uppercase hex")
            }
            Error::OfNotAfterArtifact => f.write_str("`of:` is valid only directly after an `artifact:` segment"),
            Error::OfNotLast => f.write_str("an artifact's `of:` subject ends its address"),
            Error::ArtifactWithoutSubject => f.write_str("an artifact address ends with `of:<subject>`"),
            Error::RepositoryNeedsForge => f.write_str("a repository is built with its forge, which decides its case"),
            Error::UnresolvedForge(name) => write!(f, "`{name}` does not resolve to a Forge"),
            Error::Unrecognised(input) => write!(f, "`{input}` is not an address, short form or URL"),
            Error::MissingRevision(input) => write!(f, "reference `{input}` has no `@<revision>`"),
            Error::EmptyRevision => f.write_str("a revision is non-empty"),
            Error::UnknownOperator(operator) => write!(f, "unknown operator `{operator}`; admitted operators: ==, !=, <, <=, >, >="),
            Error::InvalidVariable(name) => write!(f, "invalid template variable `${name}`"),
            Error::NonCanonicalQuoting(spelling) => write!(f, "{spelling} is not canonically quoted"),
            Error::Syntax { input, position, expected } => write!(f, "expected {expected} at byte {position} of `{input}`"),
        }
    }
}

impl std::error::Error for Error {}
