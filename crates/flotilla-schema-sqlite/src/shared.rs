//! The shared tables every kind's tables reference.

use flotilla_schema::{quote_ident, render_ddl_with, render_views, DdlMode, KindRegistry, CODES_DDL, LOG_EDGE_FIELD, LOG_LABEL_FIELD};
use rusqlite::{Connection, OptionalExtension};

/// `strings` interns names and interned columns; `resource_identity` holds every resource's
/// (kind, name, home), with homes filtered through `resource_identity_by_home`, and its `seq`:
/// the home sequence of the last event written or applied for it, kept after a delete as a
/// tombstone, so replaying an older event is a duplicate; `objects` holds frozen documents by
/// hash, always ADR 0063 canonical CBOR, so no media column; `home_sequence.last` is each home's
/// sequence, which never decreases, so a sequence never repeats.
///
/// `log` is the per-home event log (#3392 G8): one entry per write, keyed by the home and the
/// sequence the write took, which is also the resource's version after it. Each entry is an event:
///
/// - the resource it changed and the resource's sequence before it (`prev_seq`, for gaps);
/// - its cause: `create`, `edit` (a spec edit), `edge`, `delete` or `expunge`; an edge's header
///   (`edge`, `from` and `to`, codes of the kind's edges and labels, which the writer and `apply`
///   check against the kind, since one table holds no per-kind CHECK) and its master (`about`);
/// - who wrote it (`writer`) and when (`at`);
/// - `payload`, the explanation: an edge's arguments and recorded reads, or a create's admission
///   request, as one inline CBOR document;
/// - `effect`, the authoritative change: named field-level changes in the kind's own terms, as
///   canonical CBOR, never positional row operations.
///
/// The kind tables are a view of the log, written in the same transaction; reads never replay it.
/// Nothing trims the log (G9): a delete is an event and keeps the resource's entries and identity,
/// and only an expunge removes them. An expunge entry outlives the identity row it removes, so it
/// alone has no `resource`; its effect names the identity. `log_by_resource` is a resource's
/// history in sequence order, `log_about` the entries about a master in time order, and the
/// partial `log_expunges` a home's expunge entries, the tombstones `apply` checks an event for an
/// unknown identity against, so that check visits expunges only, not every later entry.
///
/// No database created by an earlier generation of these tables exists: the first production
/// generation is the baseline, and ADR 0047's N→N+1 rule applies from then.
pub(crate) const SHARED_DDL: &str = "\
CREATE TABLE IF NOT EXISTS strings (id INTEGER PRIMARY KEY, s TEXT NOT NULL UNIQUE) STRICT;
CREATE TABLE IF NOT EXISTS resource_identity (
  id INTEGER PRIMARY KEY, kind TEXT NOT NULL,
  name INTEGER NOT NULL REFERENCES strings(id), home INTEGER NOT NULL REFERENCES strings(id),
  seq INTEGER NOT NULL DEFAULT 0,
  UNIQUE (kind, name, home)) STRICT;
CREATE INDEX IF NOT EXISTS resource_identity_by_home ON resource_identity (home, kind);
CREATE TABLE IF NOT EXISTS objects (hash BLOB PRIMARY KEY, bytes BLOB NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS home_sequence (home INTEGER PRIMARY KEY REFERENCES strings(id), last INTEGER NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS log (
  home INTEGER NOT NULL REFERENCES strings(id), home_seq INTEGER NOT NULL,
  resource INTEGER REFERENCES resource_identity(id), prev_seq INTEGER NOT NULL,
  cause TEXT NOT NULL CHECK (cause IN ('create', 'edit', 'edge', 'delete', 'expunge')),
  edge TEXT, \"from\" TEXT, \"to\" TEXT,
  about INTEGER REFERENCES resource_identity(id) ON DELETE SET NULL,
  writer INTEGER NOT NULL REFERENCES strings(id), at INTEGER NOT NULL,
  payload BLOB, effect BLOB NOT NULL,
  PRIMARY KEY (home, home_seq),
  CHECK ((resource IS NULL) = (cause = 'expunge')),
  CHECK (CASE WHEN cause = 'edge' THEN edge IS NOT NULL AND \"from\" IS NOT NULL AND \"to\" IS NOT NULL
    ELSE edge IS NULL AND \"from\" IS NULL AND \"to\" IS NULL AND about IS NULL END)) STRICT;
CREATE INDEX IF NOT EXISTS log_by_resource ON log (resource, home_seq);
CREATE INDEX IF NOT EXISTS log_about ON log (about, at) WHERE about IS NOT NULL;
CREATE INDEX IF NOT EXISTS log_expunges ON log (home, home_seq) WHERE cause = 'expunge';
";

/// The readable view of `log` (#3392 D4): each entry with its home, writer and resource named by
/// their text, its kind's code, and its edge and labels named through `codes` by the entry's kind
/// (the rows each kind declaring edges has under `log:edge` and `log:label`). `about` is the
/// master as `kind:name@home`. An expunge entry's resource is gone, so its kind and name are null:
/// its effect names the identity. The payload and effect stay CBOR.
pub(crate) fn log_view() -> String {
    let named = |id: &str| format!("(SELECT \"s\" FROM \"strings\" WHERE \"id\" = {id})");
    let code = |field: &str, column: &str| {
        format!(
            "(SELECT \"name\" FROM \"codes\" WHERE \"kind\" = \"i\".\"kind\" AND \"field\" = '{field}' AND \"code\" = \"l\".{}) AS {}",
            quote_ident(column),
            quote_ident(column)
        )
    };
    let about = format!(
        "(SELECT \"a\".\"kind\" || ':' || {} || '@' || {} FROM \"resource_identity\" AS \"a\" WHERE \"a\".\"id\" = \"l\".\"about\") AS \"about\"",
        named("\"a\".\"name\""),
        named("\"a\".\"home\"")
    );
    let columns = [
        format!("{} AS \"home\"", named("\"l\".\"home\"")),
        "\"l\".\"home_seq\"".to_owned(),
        "\"i\".\"kind\"".to_owned(),
        format!("{} AS \"name\"", named("\"i\".\"name\"")),
        "\"l\".\"prev_seq\"".to_owned(),
        "\"l\".\"cause\"".to_owned(),
        code(LOG_EDGE_FIELD, "edge"),
        code(LOG_LABEL_FIELD, "from"),
        code(LOG_LABEL_FIELD, "to"),
        about,
        format!("{} AS \"writer\"", named("\"l\".\"writer\"")),
        "\"l\".\"at\"".to_owned(),
        "\"l\".\"payload\"".to_owned(),
        "\"l\".\"effect\"".to_owned(),
    ];
    format!(
        "CREATE VIEW \"{LOG_VIEW}\" AS SELECT\n  {}\nFROM \"log\" AS \"l\" LEFT JOIN \"resource_identity\" AS \"i\" ON \"i\".\"id\" = \"l\".\"resource\"",
        columns.join(",\n  ")
    )
}

/// The name of [`log_view`].
pub(crate) const LOG_VIEW: &str = "log_view";

/// Every statement the runtime prepares outside the per-kind statements. `Store::statements` lists
/// them for the query-plan tests, so a statement added here is covered; `store.rs` prepares no
/// literal SQL (a unit test there checks).
pub(crate) const SHARED_STATEMENTS: &[(&str, &str)] = &[
    ("log_append", LOG_APPEND),
    ("log_cause", LOG_CAUSE),
    ("log_by_resource", LOG_BY_RESOURCE),
    ("log_by_home", LOG_BY_HOME),
    ("log_about_first", LOG_ABOUT_FIRST),
    ("log_about_after", LOG_ABOUT_AFTER),
    ("log_expunge", LOG_EXPUNGE),
    ("log_expunged_after", LOG_EXPUNGED_AFTER),
    ("string_id", STRING_ID),
    ("string_insert", STRING_INSERT),
    ("identity_resolve", IDENTITY_RESOLVE),
    ("identity_insert", IDENTITY_INSERT),
    ("identity_of", IDENTITY_OF),
    ("identity_delete", IDENTITY_DELETE),
    ("sequence_next", SEQUENCE_NEXT),
    ("object_put", OBJECT_PUT),
    ("identity_seq", IDENTITY_SEQ),
    ("identity_set_seq", IDENTITY_SET_SEQ),
    ("sequence_raise", SEQUENCE_RAISE),
    ("object_exists", OBJECT_EXISTS),
    ("sequence_last", SEQUENCE_LAST),
    ("object_bytes", OBJECT_BYTES),
];

pub(crate) const LOG_APPEND: &str =
    "INSERT INTO log (home, home_seq, resource, prev_seq, cause, edge, \"from\", \"to\", about, writer, at, \
                                     payload, effect) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)";
pub(crate) const LOG_CAUSE: &str = "SELECT cause FROM log WHERE home = ?1 AND home_seq = ?2";
/// The columns an entry is read back with, in `LogRow` order: its strings through `strings`' primary key.
macro_rules! log_select {
    ($($rest:literal),*) => {
        concat!(
            "SELECT l.rowid, l.home_seq, l.resource, l.prev_seq, l.cause, l.edge, l.\"from\", l.\"to\", l.about, ",
            "(SELECT s FROM strings WHERE id = l.writer), l.at, l.payload, l.effect, (SELECT s FROM strings WHERE id = l.home) ",
            "FROM log AS l ",
            $($rest),*
        )
    };
}
pub(crate) const LOG_BY_RESOURCE: &str = log_select!("WHERE l.resource = ?1 AND l.home_seq > ?2 ORDER BY l.home_seq LIMIT ?3");
pub(crate) const LOG_BY_HOME: &str = log_select!("WHERE l.home = ?1 AND l.home_seq > ?2 ORDER BY l.home_seq LIMIT ?3");
pub(crate) const LOG_ABOUT_FIRST: &str = log_select!("WHERE l.about = ?1 ORDER BY l.at, l.rowid LIMIT ?2");
pub(crate) const LOG_ABOUT_AFTER: &str = log_select!("WHERE l.about = ?1 AND (l.at, l.rowid) > (?2, ?3) ORDER BY l.at, l.rowid LIMIT ?4");
pub(crate) const LOG_EXPUNGE: &str = "DELETE FROM log WHERE resource = ?1";
/// A home's expunge entries after a sequence: the tombstones an event of an unknown identity is
/// checked against.
pub(crate) const LOG_EXPUNGED_AFTER: &str = "SELECT effect FROM log WHERE home = ?1 AND home_seq > ?2 AND cause = 'expunge'";
pub(crate) const STRING_ID: &str = "SELECT id FROM strings WHERE s = ?1";
pub(crate) const STRING_INSERT: &str = "INSERT INTO strings (s) VALUES (?1)";
pub(crate) const IDENTITY_RESOLVE: &str = "SELECT id FROM resource_identity WHERE kind = ?1 AND name = ?2 AND home = ?3";
pub(crate) const IDENTITY_INSERT: &str = "INSERT INTO resource_identity (kind, name, home) VALUES (?1, ?2, ?3)";
pub(crate) const IDENTITY_OF: &str =
    "SELECT i.kind, n.s, h.s FROM resource_identity i JOIN strings n ON n.id = i.name JOIN strings h ON h.id = i.home WHERE i.id = ?1";
pub(crate) const IDENTITY_DELETE: &str = "DELETE FROM resource_identity WHERE id = ?1";
pub(crate) const SEQUENCE_NEXT: &str =
    "INSERT INTO home_sequence (home, last) VALUES (?1, 1) ON CONFLICT (home) DO UPDATE SET last = last + 1 RETURNING last";
pub(crate) const OBJECT_PUT: &str = "INSERT OR IGNORE INTO objects (hash, bytes) VALUES (?1, ?2)";
pub(crate) const IDENTITY_SEQ: &str = "SELECT seq FROM resource_identity WHERE id = ?1";
pub(crate) const IDENTITY_SET_SEQ: &str = "UPDATE resource_identity SET seq = ?1 WHERE id = ?2";
pub(crate) const SEQUENCE_RAISE: &str =
    "INSERT INTO home_sequence (home, last) VALUES (?1, ?2) ON CONFLICT (home) DO UPDATE SET last = max(last, excluded.last)";
pub(crate) const OBJECT_EXISTS: &str = "SELECT 1 FROM objects WHERE hash = ?1";
pub(crate) const SEQUENCE_LAST: &str = "SELECT last FROM home_sequence WHERE home = ?1";
pub(crate) const OBJECT_BYTES: &str = "SELECT bytes FROM objects WHERE hash = ?1";
/// Read once per view on open, outside the runtime's statements: it reads the schema table, which
/// has no index to search by key.
pub(crate) const VIEW_SQL: &str = "SELECT sql FROM sqlite_schema WHERE type = 'view' AND name = ?1";

/// The whole schema a store with these kinds holds: shared tables and `codes`, then each kind in
/// code order, with its `codes` rows and readable views, then the readable `log_view`. Like the
/// kinds' views, the `IfNotExists` form omits it: [`sync_views`] brings it up to date on open.
pub fn schema_ddl(kinds: &KindRegistry, mode: DdlMode) -> String {
    let shared = format!("{SHARED_DDL}{CODES_DDL}");
    let mut ddl = match mode {
        DdlMode::IfNotExists => shared,
        DdlMode::Create => shared.replace(" IF NOT EXISTS", ""),
    };
    for set in kinds.iter() {
        ddl.push_str(&render_ddl_with(set, mode));
    }
    if mode == DdlMode::Create {
        ddl.push_str(&log_view());
        ddl.push_str(";\n");
    }
    ddl
}

/// Brings every kind's readable views and `log_view` up to date on open: a view whose stored
/// statement differs (or that is missing) is dropped and created again, and an unchanged one is
/// left alone, so opening an unchanged schema neither writes nor bumps the schema cookie other
/// connections re-prepare on.
pub(crate) fn sync_views(conn: &Connection, kinds: &KindRegistry) -> rusqlite::Result<()> {
    let log = (LOG_VIEW.to_owned(), log_view());
    for (name, sql) in kinds.iter().flat_map(render_views).chain([log]) {
        let stored: Option<String> = conn.query_row(VIEW_SQL, [&name], |row| row.get(0)).optional()?;
        if stored.as_deref() != Some(sql.as_str()) {
            conn.execute_batch(&format!("DROP VIEW IF EXISTS {};\n{sql};", quote_ident(&name)))?;
        }
    }
    Ok(())
}
