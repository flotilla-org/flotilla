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

mod codec;
mod ddl;
mod descriptor;
mod layout;
mod registry;
mod time;
mod traits;
mod value;

pub use codec::{DecodeError, Decoder, EncodeError, Encoder};
pub use ddl::{quote_ident, render_ddl, render_ddl_with, DdlMode};
pub use descriptor::{ColumnDef, ColumnType, DescriptorError, Index, Table, TableRole, TableSet, RESERVED_COLUMNS, SHARED_NAMES};
pub use flotilla_schema_derive::Stored;
pub use layout::{snake_case, Layout};
pub use registry::{KindRegistry, RegistryError};
pub use time::Timestamp;
pub use traits::{CodeEnum, Column, Columns, Element, Flat, Intern, Kind, SUPPORTED_ATTRIBUTES};
pub use value::{Hash, Identity, KindCode, Row, Rows, Value};
