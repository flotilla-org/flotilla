//! Backend-neutral storage schema generated from Rust types (#3320, ADR 0064 Storage).
//!
//! `#[derive(Stored)]` turns a type annotated with the closed attribute list into a [`TableSet`]
//! descriptor and a codec between values and logical [`Rows`]. Logical rows carry strings,
//! identities and hashes rather than local ids, so a backend (`flotilla-schema-sqlite`) owns
//! every id and rows compare equal across databases. Serde reaches storage only through `frozen` and `cbor` fields.
//!
//! ```
//! use flotilla_schema::{Decoder, Encoder, Stored};
//!
//! #[derive(Debug, PartialEq, Stored)]
//! #[store(kind = "Note", code = "k9")]
//! struct Note {
//!     body: String,
//!     pinned: Option<bool>,
//! }
//!
//! let note = Note { body: "hi".into(), pinned: None };
//! assert_eq!(Decoder::decode::<Note>(Encoder::encode(&note)?)?, note);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! `#[derive(Describe)]` and `#[shape(child)]`, `#[shape(flatten)]`, `#[shape(intern = "nm")]`
//! and `#[shape(code = "value")]` describe query rows through the same [`Layout`], without a
//! storage codec or write handles. `Stored` also accepts these shape attributes; existing
//! `#[store(...)]` annotations remain supported. Storage policy (`records`, `history`, `frozen`)
//! belongs only to `Stored`, and `#[shape(key)]`, a described row's key, only to `Describe`: a
//! described table below the root inherits that key as `<root>_<column>`, and a description with
//! child tables needs one. A described variant's `code` equals its Serde name. Descriptions keep
//! Rust field names, including when Serde applies `rename_all_fields`, variant-level `rename_all`
//! or field `rename`. Enum category values follow Serde's serialization-side variant renaming;
//! deserialization aliases do not change categories.
//! SQL rendering and its extension traits live in `flotilla-schema-sqlite`.
//!
//! Each [`ColumnDef`] carries enum categories, a reference target and open semantic properties.
//! Structural `roles` are arrays of strings (`key`, `ref`, `state`, `since`, `sequence`, `time`,
//! `transition-from`, `transition-to`). [`Layout::description`] starts a builder for other sources;
//! [`TableSet::log_table`] describes the shared event log for one kind, including its labels.
//!
//! A field type outside the list is refused at its `Column` bound, whose diagnostic names the
//! list. Here a struct is used without `flatten`:
//!
//! ```compile_fail
//! #[derive(flotilla_schema::Stored)]
//! struct Part {
//!     x: i64,
//! }
//!
//! #[derive(flotilla_schema::Stored)]
//! #[store(kind = "Whole", code = "k1")]
//! struct Whole {
//!     part: Part,
//! }
//! ```

// Generated code names `::flotilla_schema`; this lets the crate derive `Stored` for its own types.
extern crate self as flotilla_schema;

#[doc(hidden)]
pub mod adapt;
mod code;
mod codec;
mod describe;
mod descriptor;
pub use describe::{Describe, DescribeChild, DescribeElement};
mod edge;
mod event;
mod frozen;
mod history;
mod inline;
mod layout;
mod query;
mod records;
mod reference;
mod registry;
mod state;
mod time;
mod traits;
mod value;
mod verb;
mod view;

#[doc(hidden)]
pub use code::{code_value, decode_code};
pub use code::{Code, CodeError, ADDRESS_DOMAIN, HOME_DOMAIN, RESOURCE_NAME_DOMAIN, WRITER_DOMAIN};
pub use codec::{DecodeError, Decoder, EncodeError, Encoder};
pub use descriptor::{
    ColumnDef, ColumnType, Condition, DescriptorError, Expansion, FieldColumns, Guard, Index, IndexProblem, SchemaMode, Table, TableRole,
    TableSet, When, RECORD_VERSION, RESERVED_RECORD_COLUMNS, RESERVED_ROOT_COLUMNS, SHARED_NAMES, VIEW_SUFFIX,
};
pub use edge::{
    AppendTo, ColumnOf, Edge, EdgeAppend, EdgeCollection, EdgeError, EdgeHeader, EdgeParts, EdgeRow, EdgeSet, FieldOf, HistoryOf,
    LabelCheck, RecordsOf, RowPath, StateOf,
};
pub use event::{whole_log_topic, Cause, Change, ChangeHeader, ChangeItem, Effect, Event, EventError, Named, NewChange, Step, LOG_LIMITS};
pub use flotilla_address::Address;
pub use flotilla_schema_derive::{Describe, Stored, Verb};
pub use frozen::{Freezable, Frozen};
pub use history::{EdgeEvent, Labelled};
pub use inline::{Cbor, INLINE_LIMITS};
pub use layout::{snake_case, DatumColumns, Layout};
pub use query::{interned_value, key_value, IndexQuery};
pub use records::{Position, RecordRow, Records, RecordsFull};
pub use reference::Ref;
pub use registry::{KindRegistry, RegistryError};
#[doc(hidden)]
pub use state::StateDelta;
pub use state::{RowEdit, State};
pub use time::Timestamp;
pub use traits::{CodeEnum, Column, Columns, Element, Flat, Intern, KeyColumn, Kind, KindSpec, StateData, SUPPORTED_ATTRIBUTES};
pub use value::{Hash, Identity, KindCode, Row, Rows, Value};
pub use verb::{Param, ParamDeclaration, ParamType, Verb, VerbDeclaration, VerbEffect};
pub use view::{column_count, enum_columns, render_table_view, render_view, state_data, WIDE};

mod frictionless;
