//! The shared tables every kind's tables reference.

use flotilla_schema::{render_ddl_with, DdlMode, KindRegistry};

/// `strings` interns names and interned columns; `resource_identity` holds every resource's
/// (kind, name, home), with homes filtered through `resource_identity_by_home`, and its `seq`:
/// the home sequence of the last change written or applied for it, kept after a delete as a
/// tombstone, so replaying an older change is a duplicate however much `change_log` is trimmed;
/// `objects` holds frozen documents by hash (media 1 is ADR 0063 CBOR); `home_sequence.last` is each home's
/// sequence, which never decreases and does not depend on `change_log` rows, so deleting log
/// rows never lets a sequence repeat; `change_log` holds every write's row changes per home.
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
CREATE TABLE IF NOT EXISTS objects (hash BLOB PRIMARY KEY, media INTEGER NOT NULL, bytes BLOB NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS home_sequence (home INTEGER PRIMARY KEY REFERENCES strings(id), last INTEGER NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS change_log (
  home INTEGER NOT NULL REFERENCES strings(id), home_seq INTEGER NOT NULL,
  resource_id INTEGER NOT NULL REFERENCES resource_identity(id), change BLOB NOT NULL,
  PRIMARY KEY (home, home_seq)) STRICT;
CREATE INDEX IF NOT EXISTS change_log_by_resource ON change_log (resource_id, home_seq);
";

/// The objects media code for ADR 0063 canonical CBOR.
pub const MEDIA_CBOR: i64 = 1;

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
