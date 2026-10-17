//! The shared tables every kind's tables reference.

use flotilla_schema::{render_ddl_with, DdlMode, KindRegistry};

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
/// history in sequence order, and `log_about` the entries about a master in time order.
///
/// No database created by an earlier generation of these tables exists: the first production
/// generation is the baseline, and ADR 0047's N→N+1 rule applies from then.
pub(crate) const SHARED_DDL: &str = "\
CREATE TABLE IF NOT EXISTS strings (id INTEGER PRIMARY KEY, s TEXT NOT NULL UNIQUE) STRICT;
CREATE TABLE IF NOT EXISTS resource_identity (
  id INTEGER PRIMARY KEY, kind INTEGER NOT NULL,
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
  edge INTEGER, \"from\" INTEGER, \"to\" INTEGER,
  about INTEGER REFERENCES resource_identity(id) ON DELETE SET NULL,
  writer INTEGER NOT NULL REFERENCES strings(id), at INTEGER NOT NULL,
  payload BLOB, effect BLOB NOT NULL,
  PRIMARY KEY (home, home_seq),
  CHECK ((resource IS NULL) = (cause = 'expunge')),
  CHECK (CASE WHEN cause = 'edge' THEN edge IS NOT NULL AND \"from\" IS NOT NULL AND \"to\" IS NOT NULL
    ELSE edge IS NULL AND \"from\" IS NULL AND \"to\" IS NULL AND about IS NULL END)) STRICT;
CREATE INDEX IF NOT EXISTS log_by_resource ON log (resource, home_seq);
CREATE INDEX IF NOT EXISTS log_about ON log (about, at) WHERE about IS NOT NULL;
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

/// The whole schema a store with these kinds holds: shared tables, then each kind in code order.
pub fn schema_ddl(kinds: &KindRegistry, mode: DdlMode) -> String {
    let mut ddl = match mode {
        DdlMode::IfNotExists => SHARED_DDL.to_owned(),
        DdlMode::Create => SHARED_DDL.replace(" IF NOT EXISTS", ""),
    };
    for set in kinds.iter() {
        ddl.push_str(&render_ddl_with(set, mode));
    }
    ddl
}
