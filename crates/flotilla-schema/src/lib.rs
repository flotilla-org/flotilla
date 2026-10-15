//! Backend-neutral storage schema generated from Rust types (#3320, ADR 0064 Storage).
//!
//! `#[derive(Stored)]` turns a type annotated with the closed attribute list into a [`TableSet`]
//! descriptor and a codec between values and logical [`Rows`]. Logical rows carry strings,
//! identities and hashes rather than local ids, so a backend (`flotilla-schema-sqlite`) owns
//! every id and rows compare equal across databases. Serde is not used for stored fields.
//!
//! ```
//! use flotilla_schema::{render_ddl, Decoder, Encoder, Kind, Stored};
//!
//! #[derive(Debug, PartialEq, Stored)]
//! #[store(kind = "Note", code = 9)]
//! struct Note {
//!     body: String,
//!     pinned: Option<bool>,
//! }
//!
//! let ddl = render_ddl(&Note::table_set());
//! assert!(ddl.starts_with("CREATE TABLE \"note\" ("));
//! let note = Note { body: "hi".into(), pinned: None };
//! assert_eq!(Decoder::decode::<Note>(Encoder::encode(&note)?)?, note);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
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
//! #[store(kind = "Whole", code = 1)]
//! struct Whole {
//!     part: Part,
//! }
//! ```

#[doc(hidden)]
pub mod adapt;
mod change;
mod codec;
mod ddl;
mod descriptor;
mod frozen;
mod layout;
mod query;
mod records;
mod reference;
mod registry;
mod state;
mod time;
mod traits;
mod value;

pub use change::{ChangeError, RowChange, RowOp, CHANGE_LIMITS};
pub use codec::{DecodeError, Decoder, EncodeError, Encoder};
pub use ddl::{quote_ident, render_ddl, render_ddl_with, DdlMode};
pub use descriptor::{
    ColumnDef, ColumnType, Condition, DescriptorError, Guard, Index, IndexProblem, Table, TableRole, TableSet, RESERVED_CHILD_COLUMNS,
    RESERVED_ROOT_COLUMNS, SHARED_NAMES,
};
pub use flotilla_schema_derive::Stored;
pub use frozen::{Freezable, Frozen};
pub use layout::{snake_case, Layout};
pub use query::{interned_value, key_value, IndexQuery};
pub use records::Records;
pub use reference::Ref;
pub use registry::{KindRegistry, RegistryError};
pub use state::State;
pub use time::Timestamp;
pub use traits::{CodeEnum, Column, Columns, Element, Flat, Intern, KeyColumn, Kind, SUPPORTED_ATTRIBUTES};
pub use value::{Hash, Identity, KindCode, Row, Rows, Value};
