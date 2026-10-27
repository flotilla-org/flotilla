//! The shared tables every kind's tables reference.

use std::collections::BTreeSet;

use crate::{quote_ident, render_ddl_with, render_views, DdlMode, StoreError, CODES_DDL, LOG_EDGE_FIELD, LOG_LABEL_FIELD};
use flotilla_schema::{ColumnType, KindRegistry, Table, TableSet};
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
CREATE TABLE IF NOT EXISTS strings (
  id INTEGER PRIMARY KEY,
  domain TEXT NOT NULL CHECK (length(domain) > 0),
  s TEXT NOT NULL,
  UNIQUE (domain, s)
) STRICT;
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
  topic TEXT NOT NULL DEFAULT 'all' CHECK (length(topic) BETWEEN 1 AND 8 AND topic NOT GLOB '*[^a-z0-9_]*' AND substr(topic, 1, 1) GLOB '[a-z]'),
  PRIMARY KEY (home, home_seq),
  CHECK ((resource IS NULL) = (cause = 'expunge')),
  CHECK (CASE WHEN cause = 'edge' THEN edge IS NOT NULL AND \"from\" IS NOT NULL AND \"to\" IS NOT NULL
    ELSE edge IS NULL AND \"from\" IS NULL AND \"to\" IS NULL AND about IS NULL END)) STRICT;
CREATE INDEX IF NOT EXISTS log_by_resource ON log (resource, home_seq);
CREATE INDEX IF NOT EXISTS log_about ON log (about, at) WHERE about IS NOT NULL;
CREATE INDEX IF NOT EXISTS log_expunges ON log (home, home_seq) WHERE cause = 'expunge';
CREATE TABLE IF NOT EXISTS stream_watermark (
  home INTEGER PRIMARY KEY REFERENCES strings(id), home_seq INTEGER NOT NULL CHECK (home_seq >= 0), applied_through INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS stream_history (
  home INTEGER PRIMARY KEY REFERENCES strings(id), home_seq INTEGER NOT NULL CHECK (home_seq >= 0)
) STRICT, WITHOUT ROWID;
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
        "\"l\".\"topic\"".to_owned(),
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
    ("home_mark_reset", HOME_MARK_RESET),
    ("home_seq_reset", HOME_SEQ_RESET),
    ("home_ids_reset", HOME_IDS_RESET),
    ("home_log_clear", HOME_LOG_CLEAR),
    ("identities_page", IDENTITIES_PAGE),
    ("removed_max", REMOVED_MAX),
    ("floor_raise", FLOOR_RAISE),
    ("floor_get", FLOOR_GET),
    ("watermark_put", WATERMARK_PUT),
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
                                     payload, effect, topic) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)";
pub(crate) const LOG_CAUSE: &str = "SELECT cause FROM log WHERE home = ?1 AND home_seq = ?2";
/// The columns an entry is read back with, in `LogRow` order: its strings through `strings`' primary key.
macro_rules! log_select {
    ($($rest:literal),*) => {
        concat!(
            "SELECT l.rowid, l.home_seq, l.resource, l.prev_seq, l.cause, l.edge, l.\"from\", l.\"to\", l.about, ",
            "(SELECT s FROM strings WHERE id = l.writer), l.at, l.payload, l.effect, (SELECT s FROM strings WHERE id = l.home), l.topic ",
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
pub(crate) const STRING_ID: &str = "SELECT id FROM strings WHERE domain = ?1 AND s = ?2";
pub(crate) const STRING_INSERT: &str = "INSERT INTO strings (domain, s) VALUES (?1, ?2)";
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
    render_schema(kinds, mode, SHARED_DDL, &log_view())
}

/// Shared renderer for the runtime baseline and the isolated schema-review proposal.
pub(crate) fn render_schema(kinds: &KindRegistry, mode: DdlMode, shared: &str, log_view: &str) -> String {
    let shared = format!("{shared}{CODES_DDL}");
    let mut ddl = match mode {
        DdlMode::IfNotExists => shared,
        DdlMode::Create => shared.replace(" IF NOT EXISTS", ""),
    };
    for set in kinds.iter() {
        ddl.push_str(&render_ddl_with(set, mode));
    }
    if mode == DdlMode::Create {
        ddl.push_str(log_view);
        ddl.push_str(";\n");
        for (_, view) in progress_views() {
            ddl.push_str(&view);
            ddl.push_str(";\n");
        }
    }
    ddl
}

/// Brings every kind's readable views and `log_view` up to date on open: a view whose stored
/// statement differs (or that is missing) is dropped and created again, and an unchanged one is
/// left alone, so opening an unchanged schema neither writes nor bumps the schema cookie other
/// connections re-prepare on.
pub(crate) fn sync_views(conn: &Connection, kinds: &KindRegistry) -> rusqlite::Result<()> {
    let log = (LOG_VIEW.to_owned(), log_view());
    for (name, sql) in kinds.iter().flat_map(render_views).chain([log]).chain(progress_views()) {
        let stored: Option<String> = conn.query_row(VIEW_SQL, [&name], |row| row.get(0)).optional()?;
        if stored.as_deref() != Some(sql.as_str()) {
            conn.execute_batch(&format!("DROP VIEW IF EXISTS {};\n{sql};", quote_ident(&name)))?;
        }
    }
    Ok(())
}

/// Startup-only metadata guard: fail clearly before a pre-domain dictionary reaches a writer.
pub(crate) fn has_string_domain(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row("SELECT EXISTS (SELECT 1 FROM pragma_table_info('strings') WHERE name = 'domain')", [], |row| row.get(0))
}

/// Refuses a kind table whose columns differ from its kind's: the backend's own columns (`id` and
/// `version` on the root, the parent key below it), and the key and data columns. Order is not
/// compared: statements name their columns, and an additive upgrade (`ADD COLUMN`) appends.
pub(crate) fn check_kind_columns(conn: &Connection, kinds: &KindRegistry) -> Result<(), StoreError> {
    let mut statement = conn.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?;
    for set in kinds.iter() {
        for table in &set.tables {
            let mut expected: Vec<String> = match table.parent {
                None => vec!["id".into(), "version".into()],
                Some(_) => vec![set.parent_key_column().to_owned()],
            };
            expected.extend(table.key.iter().chain(&table.columns).map(|column| column.name.clone()));
            let mut found = statement.query_map([&table.name], |row| row.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
            // Explicit ADR 0047 N→N+1 defaults only: no broad migration policy.
            // Remove each column's default/upgrade eligibility one roll after its
            // field lands; once no eligible columns remain this check only reads.
            // The open transaction rolls this back if any other drift remains.
            for column in &table.columns {
                if !found.contains(&column.name) && column.previous_generation_default().is_some() {
                    conn.execute_batch(&format!("ALTER TABLE {} ADD COLUMN {};", quote_ident(&table.name), crate::ddl::column(column)))?;
                    found.push(column.name.clone());
                }
            }
            if found.iter().collect::<BTreeSet<_>>() != expected.iter().collect::<BTreeSet<_>>() {
                return Err(StoreError::KindTableDrift { table: table.name.clone(), expected, found });
            }
        }
    }
    Ok(())
}

/// Brings each kind table's stored code CHECKs up to its kind's codes (#3972). SQLite cannot change
/// a CHECK in place, so a table whose stored code lists are each a subset of its kind's, at least
/// one strictly, and which is otherwise exactly the table this build renders, is rebuilt with the
/// current definition by [`widen_table`]. "Otherwise exactly" is an exact match of the stored
/// `CREATE TABLE` text, less only the columns [`check_kind_columns`] may add (those declaring a
/// previous-generation default), which the rebuild adds with their default; it runs first, so a
/// table needing both is widened rather than refused. A table whose text an earlier `ALTER TABLE`
/// changed is not widened: it refuses with [`StoreError::KindCodeDrift`].
///
/// Any other difference refuses: a stored code the kind no longer declares, a missing code in a
/// table that differs in more than its codes, or a coded column whose CHECK cannot be read, which
/// is never taken as current.
///
/// Rebuilding drops the old table, so the caller runs this with foreign keys off (inside a
/// transaction `PRAGMA foreign_keys` cannot change): with them on, the drop would delete each
/// child table's rows through their `ON DELETE CASCADE`.
pub(crate) fn check_kind_codes(conn: &Connection, kinds: &KindRegistry) -> Result<(), StoreError> {
    let mut statement = conn.prepare("SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1")?;
    let mut columns = conn.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?;
    for set in kinds.iter() {
        for table in &set.tables {
            let Some(sql) = statement.query_row([&table.name], |row| row.get::<_, String>(0)).optional()? else { continue };
            let present = columns.query_map([&table.name], |row| row.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
            // The table as this build renders it with the stored code lists and without the
            // columns opening may add, and the first column missing codes.
            let mut stored_table = table.clone();
            stored_table.columns.retain(|column| present.contains(&column.name) || column.previous_generation_default().is_none());
            let mut missing = None;
            for column in stored_table.key.iter_mut().chain(stored_table.columns.iter_mut()) {
                let ColumnType::Code(codes) = &mut column.ty else { continue };
                if !present.contains(&column.name) {
                    continue;
                }
                let Some(stored) = stored_codes(&sql, &column.name) else {
                    return Err(StoreError::KindCodeUnreadable { table: table.name.clone(), column: column.name.clone() });
                };
                let removed: Vec<String> = stored.iter().filter(|code| !codes.contains(code)).cloned().collect();
                if !removed.is_empty() {
                    return Err(StoreError::KindCodeRemoved { table: table.name.clone(), column: column.name.clone(), removed });
                }
                let lacking: Vec<String> = codes.iter().filter(|code| !stored.contains(code)).cloned().collect();
                if missing.is_none() && !lacking.is_empty() {
                    missing = Some((column.name.clone(), lacking));
                }
                *codes = stored;
            }
            let Some((column, missing)) = missing else { continue };
            if crate::ddl::create_table(set, &stored_table, "") != sql {
                return Err(StoreError::KindCodeDrift { table: table.name.clone(), column, missing });
            }
            // Every coded column is rebuilt with its kind's codes, not only the first one missing some.
            widen_table(conn, set, table, &present)?;
        }
    }
    Ok(())
}

/// The codes a stored `CHECK (<column> IN ('a', 'b'))` accepts, in stored order, with the column
/// quoted (as `ddl::column` renders it; keep the two in step) or bare (a hand-written upgrade).
fn stored_codes(sql: &str, column: &str) -> Option<Vec<String>> {
    let start = [format!("CHECK ({} IN (", quote_ident(column)), format!("CHECK ({column} IN (")]
        .iter()
        .find_map(|prefix| sql.find(prefix.as_str()).map(|at| at + prefix.len()))?;
    let list = &sql[start..start + sql[start..].find(')')?];
    Some(list.split(',').map(|code| code.trim().trim_matches('\'').to_owned()).collect())
}

/// Rebuilds `table` with its current definition, following SQLite's procedure for other table
/// schema changes (https://sqlite.org/lang_altertable.html#otheralter): create it under a
/// temporary name, copy every row, drop the old table, rename the new one to the old name, and
/// recreate the indexes and triggers the drop removed and the views that name the table. Tables
/// whose foreign keys reference it name it, not its storage, so they reference the new table.
/// `present` are the old table's columns, which are copied; a column it lacks takes its default.
///
/// Dependent views are found by the quoted table name, as `ddl.rs` and `shared.rs` write every
/// view. A view naming the table unquoted, and a trigger on another table naming it, are not
/// found; the rename then fails with "no such table" and the open transaction rolls back.
fn widen_table(conn: &Connection, set: &TableSet, table: &Table, present: &[String]) -> Result<(), StoreError> {
    let name = quote_ident(&table.name);
    let temporary = Table { name: format!("{}_widening", table.name), ..table.clone() };
    let mut dependents = conn.prepare(
        "SELECT type, name, sql FROM sqlite_schema WHERE sql IS NOT NULL AND name <> ?1 \
         AND ((type IN ('index', 'trigger') AND tbl_name = ?1) OR (type = 'view' AND instr(sql, ?2) > 0)) ORDER BY rowid",
    )?;
    let dependents = dependents
        .query_map([&table.name, &name], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut batch = String::new();
    for (_, view, _) in dependents.iter().filter(|(kind, _, _)| kind == "view") {
        batch.push_str(&format!("DROP VIEW {};\n", quote_ident(view)));
    }
    batch.push_str(&format!(
        "{};\nINSERT INTO {temporary_name} ({copied}) SELECT {copied} FROM {name};\nDROP TABLE {name};\nALTER TABLE {temporary_name} RENAME TO {name};\n",
        crate::ddl::create_table(set, &temporary, ""),
        temporary_name = quote_ident(&temporary.name),
        copied = present.iter().map(|column| quote_ident(column)).collect::<Vec<_>>().join(", "),
    ));
    for (_, _, sql) in &dependents {
        batch.push_str(&format!("{sql};\n"));
    }
    conn.execute_batch(&batch)?;
    let violations: usize = conn.query_row("SELECT count(*) FROM pragma_foreign_key_check(?1)", [&table.name], |row| row.get(0))?;
    if violations > 0 {
        return Err(StoreError::KindCodeWidening { table: table.name.clone(), violations });
    }
    Ok(())
}

/// Shared progress views use the same sync-on-open policy as kind views.
fn progress_views() -> Vec<(String, String)> {
    [("stream_watermark", "home_seq, applied_through"), ("stream_history", "home_seq")]
        .into_iter()
        .map(|(table, columns)| {
            (
                format!("{table}_view"),
                format!("CREATE VIEW {table}_view AS SELECT (SELECT s FROM strings WHERE id = home) AS home, {columns} FROM {table}"),
            )
        })
        .collect()
}

pub(crate) const WATERMARK_PUT: &str = "INSERT INTO stream_watermark (home, home_seq, applied_through) VALUES (?1, ?2, ?3) ON CONFLICT(home) DO UPDATE SET home_seq = excluded.home_seq, applied_through = excluded.applied_through";
pub(crate) const FLOOR_GET: &str = "SELECT home_seq FROM stream_history WHERE home = ?1";
pub(crate) const FLOOR_RAISE: &str = "INSERT INTO stream_history (home, home_seq) VALUES (?1, ?2) ON CONFLICT(home) DO UPDATE SET home_seq = max(home_seq, excluded.home_seq)";
pub(crate) const REMOVED_MAX: &str = "SELECT max(home_seq) FROM log WHERE resource = ?1";
pub(crate) const IDENTITIES_PAGE: &str = "SELECT id FROM resource_identity WHERE home = ?1 AND kind = ?2 AND id > ?3 ORDER BY id LIMIT ?4";
pub(crate) const HOME_LOG_CLEAR: &str = "DELETE FROM log WHERE home = ?1";
pub(crate) const HOME_IDS_RESET: &str = "UPDATE resource_identity SET seq = 0 WHERE home = ?1";
pub(crate) const HOME_SEQ_RESET: &str = "DELETE FROM home_sequence WHERE home = ?1";
pub(crate) const HOME_MARK_RESET: &str = "DELETE FROM stream_watermark WHERE home = ?1";

/// Startup-only resident input load; runtime reads use the committed in-memory map.
pub(crate) const WATERMARK_STARTUP: &str =
    "SELECT s.s, w.home_seq, w.applied_through FROM stream_watermark w JOIN strings s ON s.id = w.home";

#[cfg(test)]
mod tests {
    use super::stored_codes;
    use flotilla_schema::{ColumnDef, ColumnType};

    // Behaviour (#3972): the codes `ddl::column` writes into a coded column's CHECK read back in
    // order, beside other CHECKs in the table, and a hand-written bare-column CHECK reads too.
    #[test]
    fn stored_codes_reads_back_what_the_column_renders() {
        let codes: Vec<String> = ["as", "co", "pe2", "x_y"].into_iter().map(String::from).collect();
        let column = ColumnDef {
            name: "receiver_kind".into(),
            ty: ColumnType::Code(codes.clone()),
            nullable: true,
            categories: Vec::new(),
            reference: None,
            properties: Default::default(),
        };
        let sql =
            format!("CREATE TABLE \"t\" (\n  {},\n  \"done\" INTEGER CHECK (\"done\" IN (0, 1))\n) STRICT", crate::ddl::column(&column));
        assert_eq!(stored_codes(&sql, "receiver_kind"), Some(codes));
        assert_eq!(
            stored_codes("CREATE TABLE t (activity TEXT CHECK (activity IN ('id', 'wo')))", "activity"),
            Some(vec!["id".into(), "wo".into()])
        );
        assert_eq!(stored_codes(&sql, "missing"), None);
    }
}
