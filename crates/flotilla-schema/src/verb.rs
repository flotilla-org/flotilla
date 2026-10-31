//! Native verb declarations (#4182 ruling 2 revised, #4197): a typed operation on an ADR 0066
//! subject kind declares its name, subject kind, parameters, effect and whether it accepts an
//! invocation id. `#[derive(Verb)]` on the verb's parameter struct generates the declaration,
//! which is the verb's whole signature: there is no separate hand-written description.
//!
//! ```
//! use flotilla_schema::{VerbEffect, ParamType, Verb};
//!
//! #[derive(Verb)]
//! #[verb(name = "convoy.abandon", subject = "convoy", effect = "destructive", invocation)]
//! #[allow(dead_code)]
//! struct Abandon {
//!     #[param(text)]
//!     reason: String,
//!     #[param(options("soft", "hard"), default = "soft")]
//!     mode: Option<String>,
//! }
//!
//! let declaration = Abandon::declaration();
//! assert_eq!((declaration.name, declaration.subject), ("convoy.abandon", "convoy"));
//! assert_eq!(declaration.effect, VerbEffect::Destructive);
//! assert!(declaration.invocation && !declaration.idempotent);
//! assert_eq!(declaration.result, None);
//! assert_eq!(declaration.params[0].ty, ParamType::Text);
//! assert!(declaration.params[0].required);
//! assert_eq!(declaration.params[1].ty, ParamType::Enum);
//! assert_eq!(declaration.params[1].options, ["soft", "hard"]);
//! assert!(!declaration.params[1].required);
//! ```

/// What a verb does to its subjects (#4182 ruling 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerbEffect {
    /// Changes nothing.
    Read,
    /// Changes state recoverably.
    Write,
    /// Hard or impossible to undo.
    Destructive,
}

impl VerbEffect {
    pub const ALL: [VerbEffect; 3] = [VerbEffect::Read, VerbEffect::Write, VerbEffect::Destructive];

    pub fn as_str(self) -> &'static str {
        match self {
            VerbEffect::Read => "read",
            VerbEffect::Write => "write",
            VerbEffect::Destructive => "destructive",
        }
    }
}

/// A parameter's type (#4182 ruling 8): a flat scalar, an enum of listed options, or the address
/// of a subject of a kind, filled by picking a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParamType {
    /// One line of text. (Not `String`: a variant of that name would make every compiler message
    /// in this crate spell the std type `std::string::String`.)
    Str,
    /// Multi-line text.
    Text,
    Integer,
    Number,
    Bool,
    /// One of the declared options.
    Enum,
    /// The address of a subject of this ADR 0066 kind.
    Subject(&'static str),
}

impl ParamType {
    /// The type as the `verbs_params` table spells it: the type's name, or the subject kind.
    pub fn as_str(self) -> &'static str {
        match self {
            ParamType::Str => "string",
            ParamType::Text => "text",
            ParamType::Integer => "integer",
            ParamType::Number => "number",
            ParamType::Bool => "bool",
            ParamType::Enum => "enum",
            ParamType::Subject(kind) => kind,
        }
    }
}

/// One declared parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamDeclaration {
    pub name: &'static str,
    pub ty: ParamType,
    /// Required unless the field is an `Option`.
    pub required: bool,
    /// A literal, or `$row.<column>`.
    pub default: Option<&'static str>,
    /// The values an `enum` parameter takes.
    pub options: Vec<&'static str>,
}

/// A native verb's signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerbDeclaration {
    /// `<subject kind>.<verb>`, stable across wire changes.
    pub name: &'static str,
    /// The ADR 0066 address kind the verb acts on.
    pub subject: &'static str,
    pub effect: VerbEffect,
    /// Whether the verb takes the caller's invocation id into its effect, so a repeat with the
    /// same id runs again and changes nothing new (#4182 ruling 7).
    pub invocation: bool,
    /// Whether repeating the verb with the same arguments changes nothing more: a hint for MCP's
    /// `idempotentHint`, never relied on by the route.
    pub idempotent: bool,
    /// The address kind of the subject the verb creates, for a create verb on a parent.
    pub result: Option<&'static str>,
    pub params: Vec<ParamDeclaration>,
}

/// A native verb's parameter struct and, through it, its declaration.
pub trait Verb {
    fn declaration() -> VerbDeclaration;
}

/// A type a verb parameter may have, and the parameter type it declares.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a verb parameter type",
    note = "parameters are String, bool, integers, floats, or an Address with #[param(subject = \"kind\")], each optionally in an Option"
)]
pub trait Param {
    const TYPE: ParamType;
    const REQUIRED: bool = true;
}

macro_rules! params {
    ($($ty:ty => $param:expr),* $(,)?) => {$(impl Param for $ty { const TYPE: ParamType = $param; })*};
}
params!(
    String => ParamType::Str,
    bool => ParamType::Bool,
    i8 => ParamType::Integer, i16 => ParamType::Integer, i32 => ParamType::Integer, i64 => ParamType::Integer,
    u8 => ParamType::Integer, u16 => ParamType::Integer, u32 => ParamType::Integer, u64 => ParamType::Integer,
    f32 => ParamType::Number, f64 => ParamType::Number,
    // The kind comes from `#[param(subject = "kind")]`; the derive refuses an address without it.
    crate::Address => ParamType::Subject(""),
);

impl<T: Param> Param for Option<T> {
    const TYPE: ParamType = T::TYPE;
    const REQUIRED: bool = false;
}
