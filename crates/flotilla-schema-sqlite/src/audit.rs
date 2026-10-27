//! Explicit integrity diagnostics, usable on a read-only SQLite connection.

use flotilla_schema::{Code, ColumnType, KindRegistry, HOME_DOMAIN, RESOURCE_NAME_DOMAIN, WRITER_DOMAIN};
use rusqlite::Connection;

use crate::StoreError;

/// Checks every registered interned column and the shared identity, sequence and log columns.
/// Missing ids and wrong domains fail with the real table/column. This scans tables and never
/// writes. The caller must register all kinds it wants audited, including application extensions.
pub fn audit_domains(conn: &Connection, kinds: &KindRegistry) -> Result<(), StoreError> {
    if !crate::shared::has_string_domain(conn)? {
        return Err(StoreError::LegacyDictionarySchema);
    }
    let check = |table: &str, column: &str, domain: Code| -> Result<(), StoreError> {
        let sql = format!(
            "SELECT EXISTS (SELECT 1 FROM {} AS t LEFT JOIN strings AS s ON s.id = t.{} \
                 WHERE t.{} IS NOT NULL AND (s.id IS NULL OR s.domain != ?1))",
            crate::quote_ident(table),
            crate::quote_ident(column),
            crate::quote_ident(column),
        );
        if conn.query_row(&sql, [domain.as_str()], |row| row.get::<_, bool>(0))? {
            return Err(StoreError::Corrupt { table: table.to_owned(), column: column.to_owned() });
        }
        Ok(())
    };
    for (table, column, domain) in [
        ("resource_identity", "name", RESOURCE_NAME_DOMAIN),
        ("resource_identity", "home", HOME_DOMAIN),
        ("home_sequence", "home", HOME_DOMAIN),
        ("stream_history", "home", HOME_DOMAIN),
        ("stream_watermark", "home", HOME_DOMAIN),
        ("log", "home", HOME_DOMAIN),
        ("change", "home", HOME_DOMAIN),
        ("change_items", "change_home", HOME_DOMAIN),
        ("change", "writer", WRITER_DOMAIN),
    ] {
        check(table, column, domain)?;
    }
    for set in kinds.iter() {
        for table in &set.tables {
            for column in table.key.iter().chain(&table.columns) {
                if let ColumnType::Interned(domain) = column.ty {
                    check(&table.name, &column.name, domain)?;
                }
            }
        }
    }
    Ok(())
}
