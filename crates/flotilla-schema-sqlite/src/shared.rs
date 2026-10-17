//! The shared tables every kind's tables reference.

use flotilla_schema::{render_ddl_with, DdlMode, KindRegistry, CODES_DDL};

/// `strings` interns names and interned columns; `resource_identity` holds every resource's
/// (kind, name, home), with homes filtered through `resource_identity_by_home`, and its `seq`:
/// the home sequence of the last change written or applied for it, kept after a delete as a
/// tombstone, so replaying an older change is a duplicate however much `change_log` is trimmed;
/// `objects` holds frozen documents by hash, always ADR 0063 canonical CBOR, so no media column; `home_sequence.last`
/// is each home's sequence, which never decreases and does not depend on `change_log` rows, so deleting log
/// rows never lets a sequence repeat; `change_log` holds every write's row changes per home.
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
CREATE TABLE IF NOT EXISTS change_log (
  home INTEGER NOT NULL REFERENCES strings(id), home_seq INTEGER NOT NULL,
  resource_id INTEGER NOT NULL REFERENCES resource_identity(id), change BLOB NOT NULL,
  PRIMARY KEY (home, home_seq)) STRICT;
CREATE INDEX IF NOT EXISTS change_log_by_resource ON change_log (resource_id, home_seq);
";

/// Every statement the runtime prepares outside the per-kind statements. `Store::statements` lists
/// them for the query-plan tests, so a statement added here is covered; `store.rs` prepares no
/// literal SQL (a unit test there checks).
pub(crate) const SHARED_STATEMENTS: &[(&str, &str)] = &[
    ("log_append", LOG_APPEND),
    ("log_taken", LOG_TAKEN),
    ("string_id", STRING_ID),
    ("string_insert", STRING_INSERT),
    ("identity_resolve", IDENTITY_RESOLVE),
    ("identity_insert", IDENTITY_INSERT),
    ("identity_of", IDENTITY_OF),
    ("sequence_next", SEQUENCE_NEXT),
    ("object_put", OBJECT_PUT),
    ("identity_seq", IDENTITY_SEQ),
    ("identity_set_seq", IDENTITY_SET_SEQ),
    ("sequence_raise", SEQUENCE_RAISE),
    ("object_exists", OBJECT_EXISTS),
    ("log_by_resource", LOG_BY_RESOURCE),
    ("log_by_home", LOG_BY_HOME),
    ("sequence_last", SEQUENCE_LAST),
    ("object_bytes", OBJECT_BYTES),
];

pub(crate) const LOG_APPEND: &str = "INSERT INTO change_log (home, home_seq, resource_id, change) VALUES (?1, ?2, ?3, ?4)";
pub(crate) const LOG_TAKEN: &str = "SELECT 1 FROM change_log WHERE home = ?1 AND home_seq = ?2";
pub(crate) const STRING_ID: &str = "SELECT id FROM strings WHERE s = ?1";
pub(crate) const STRING_INSERT: &str = "INSERT INTO strings (s) VALUES (?1)";
pub(crate) const IDENTITY_RESOLVE: &str = "SELECT id FROM resource_identity WHERE kind = ?1 AND name = ?2 AND home = ?3";
pub(crate) const IDENTITY_INSERT: &str = "INSERT INTO resource_identity (kind, name, home) VALUES (?1, ?2, ?3)";
pub(crate) const IDENTITY_OF: &str =
    "SELECT i.kind, n.s, h.s FROM resource_identity i JOIN strings n ON n.id = i.name JOIN strings h ON h.id = i.home WHERE i.id = ?1";
pub(crate) const SEQUENCE_NEXT: &str =
    "INSERT INTO home_sequence (home, last) VALUES (?1, 1) ON CONFLICT (home) DO UPDATE SET last = last + 1 RETURNING last";
pub(crate) const OBJECT_PUT: &str = "INSERT OR IGNORE INTO objects (hash, bytes) VALUES (?1, ?2)";
pub(crate) const IDENTITY_SEQ: &str = "SELECT seq FROM resource_identity WHERE id = ?1";
pub(crate) const IDENTITY_SET_SEQ: &str = "UPDATE resource_identity SET seq = ?1 WHERE id = ?2";
pub(crate) const SEQUENCE_RAISE: &str =
    "INSERT INTO home_sequence (home, last) VALUES (?1, ?2) ON CONFLICT (home) DO UPDATE SET last = max(last, excluded.last)";
pub(crate) const OBJECT_EXISTS: &str = "SELECT 1 FROM objects WHERE hash = ?1";
pub(crate) const LOG_BY_RESOURCE: &str =
    "SELECT change FROM change_log WHERE resource_id = ?1 AND home_seq > ?2 ORDER BY home_seq LIMIT ?3";
pub(crate) const LOG_BY_HOME: &str = "SELECT change FROM change_log WHERE home = ?1 AND home_seq > ?2 ORDER BY home_seq LIMIT ?3";
pub(crate) const SEQUENCE_LAST: &str = "SELECT last FROM home_sequence WHERE home = ?1";
pub(crate) const OBJECT_BYTES: &str = "SELECT bytes FROM objects WHERE hash = ?1";

/// The whole schema a store with these kinds holds: shared tables and `codes`, then each kind in
/// code order, with its `codes` rows and readable views.
pub fn schema_ddl(kinds: &KindRegistry, mode: DdlMode) -> String {
    let shared = format!("{SHARED_DDL}{CODES_DDL}");
    let mut ddl = match mode {
        DdlMode::IfNotExists => shared,
        DdlMode::Create => shared.replace(" IF NOT EXISTS", ""),
    };
    for set in kinds.iter() {
        ddl.push_str(&render_ddl_with(set, mode));
    }
    ddl
}
