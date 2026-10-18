//! Schema checkpoint for #3463: every host follows every other home's whole log.
//! `Store::open` does not install this proposal. Only schema tests execute it until owner
//! approval. See `docs/implementation-choices/change-stream.md` for workload and invariants.
//!
//! After approval, wire these declarations into the shared baseline and remove this temporary
//! public proposal API. Progress is latest observation state, never a new durable log event.

use flotilla_schema::{Code, DdlMode, KindRegistry, Timestamp};

use crate::shared::{render_schema, stream_log_view, STREAM_SHARED_DDL};

/// Reserved coarse topic for every write until whole-topic partitioning is introduced.
/// It is chosen in the event's write transaction; followers never filter on it.
pub const WHOLE_LOG_TOPIC: Code = Code::new("all");

/// A local `strings.id` in the home domain. Never appears on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HomeId(pub i64);

// Shared metadata has no resource identity or lifecycle of its own. One declaration produces
// its stored row type and review DDL; creation prefixes are emitted, not text-replaced.
macro_rules! table {
    ($row:ident, $table:literal, $doc:literal, { $($field:ident: $ty:ty => $sql:literal),+ $(,)? }, $constraints:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $row { $(pub $field: $ty),+ }
        impl $row {
            fn ddl(mode: DdlMode) -> String {
                let exists = match mode {
                    DdlMode::Create => "",
                    DdlMode::IfNotExists => "IF NOT EXISTS ",
                };
                format!(concat!(
                    "CREATE TABLE {}", $table, " (\n",
                    $("  ", stringify!($field), " ", $sql, ",\n",)+
                    $constraints, "\n) STRICT, WITHOUT ROWID;\n"
                ), exists)
            }
        }
    };
}

table!(WatermarkRow, "stream_watermark",
    "Latest fully applied prefix and home time. The follower commits this with the rows it covers; keepalives advance time only after applying the prefix.", {
    home: HomeId => "INTEGER NOT NULL REFERENCES strings(id)",
    home_seq: u64 => "INTEGER NOT NULL CHECK (home_seq >= 0)",
    applied_through: Timestamp => "INTEGER NOT NULL",
}, "  PRIMARY KEY (home)");

table!(HistoryRow, "stream_history",
    "Home-owned resume floor. The home expunge transaction advances home_seq to cover removed log entries in the same commit that deletes them.", {
    home: HomeId => "INTEGER NOT NULL REFERENCES strings(id)",
    home_seq: u64 => "INTEGER NOT NULL CHECK (home_seq >= 0)",
}, "  PRIMARY KEY (home)");

/// Complete proposed schema, including the log's coarse topic code and readable views.
/// Runtime `schema_ddl` remains unchanged until the owner approves the schema.
pub fn stream_schema_ddl(kinds: &KindRegistry, mode: DdlMode) -> String {
    let mut ddl = render_schema(kinds, mode, STREAM_SHARED_DDL, &stream_log_view());
    ddl.push_str(&WatermarkRow::ddl(mode));
    ddl.push_str(&HistoryRow::ddl(mode));
    let exists = match mode {
        DdlMode::Create => "",
        DdlMode::IfNotExists => "IF NOT EXISTS ",
    };
    for (table, columns) in [("stream_watermark", "home_seq, applied_through"), ("stream_history", "home_seq")] {
        ddl.push_str(&format!(
            "CREATE VIEW {exists}{table}_view AS SELECT\n  \
             (SELECT s FROM strings WHERE id = home) AS home, {columns}\nFROM {table};\n"
        ));
    }
    ddl
}
