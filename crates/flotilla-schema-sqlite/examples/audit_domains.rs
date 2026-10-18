//! Read-only domain diagnostic for the registered engine kinds.

use rusqlite::{Connection, OpenFlags};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os().nth(1).ok_or("usage: audit_domains <store.db>")?;
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let kinds = flotilla_state_machines::kinds()?;
    flotilla_schema_sqlite::audit_domains(&conn, &kinds)?;
    println!("Domain audit passed for shared tables and registered engine kinds");
    Ok(())
}
