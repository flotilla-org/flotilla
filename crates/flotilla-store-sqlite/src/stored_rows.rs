//! The old store's rows as they lie, for the cut-over import, which must never write to the store
//! it reads.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::ResourceError;

/// One record the store holds: its own, or a replica of another home's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRow {
    /// The home a replica came from; `None` for this store's own record.
    pub origin_root: Option<String>,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    /// The stored document, as written.
    pub body: String,
}

/// Every record, own and replica, of the store at `path`, read in one transaction through a
/// read-only connection, so the file is left as it was.
pub fn read_stored_rows(path: &Path) -> Result<Vec<StoredRow>, ResourceError> {
    let error = |what: &str, err: rusqlite::Error| ResourceError::other(format!("{what} {}: {err}", path.display()));
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|err| error("open", err))?;
    let transaction = connection.transaction().map_err(|err| error("read", err))?;
    let mut rows = Vec::new();
    let mut own = transaction
        .prepare("SELECT kind, namespace, name, body_json FROM resource_objects ORDER BY kind, namespace, name")
        .map_err(|err| error("read", err))?;
    let read = own
        .query_map([], |row| {
            Ok(StoredRow { origin_root: None, kind: row.get(0)?, namespace: row.get(1)?, name: row.get(2)?, body: row.get(3)? })
        })
        .map_err(|err| error("read", err))?;
    for row in read {
        rows.push(row.map_err(|err| error("read", err))?);
    }
    let mut replicas = transaction
        .prepare("SELECT origin_root, kind, namespace, name, body_json FROM replica_objects ORDER BY origin_root, kind, namespace, name")
        .map_err(|err| error("read", err))?;
    let read = replicas
        .query_map([], |row| {
            Ok(StoredRow {
                origin_root: Some(row.get(0)?),
                kind: row.get(1)?,
                namespace: row.get(2)?,
                name: row.get(3)?,
                body: row.get(4)?,
            })
        })
        .map_err(|err| error("read", err))?;
    for row in read {
        rows.push(row.map_err(|err| error("read", err))?);
    }
    Ok(rows)
}
