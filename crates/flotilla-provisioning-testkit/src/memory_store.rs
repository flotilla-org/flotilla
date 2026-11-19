//! In-memory decided stores for fixtures, each a copy of one this process prepared.

use std::sync::{Mutex, OnceLock};

use flotilla_schema::KindRegistry;
use flotilla_schema_sqlite::Store;
use rusqlite::{ffi, Connection};

/// The production kinds and an empty store holding their schema, prepared on first use.
struct Template {
    kinds: KindRegistry,
    connection: Mutex<Connection>,
}

fn template() -> &'static Template {
    static TEMPLATE: OnceLock<Template> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let kinds = flotilla_provisioning::stored::production_kinds().expect("registry");
        // Opening writes the schema and nothing of its home, so any home serves.
        let store = Store::open(Connection::open_in_memory().expect("memory"), "template", kinds.clone()).expect("template store");
        Template { kinds, connection: Mutex::new(store.into_connection()) }
    })
}

/// An empty in-memory decided store with the production kinds, homed at `home`. Its schema is
/// copied from a store this process prepared once, rather than created again, and the store is
/// opened as any other is, with every check opening makes.
pub fn memory_store(home: &str) -> Store {
    let template = template();
    let copy = Connection::open_in_memory().expect("memory");
    copy_database(&template.connection.lock().unwrap_or_else(|poisoned| poisoned.into_inner()), &copy);
    Store::open(copy, home, template.kinds.clone()).expect("store")
}

/// Copies `from`'s main database over `to`'s with SQLite's online backup, in one step. rusqlite
/// wraps the backup API only behind a feature the workspace's shared selections leave off.
fn copy_database(from: &Connection, to: &Connection) {
    let main = c"main";
    // SAFETY: both handles are open connections that outlive the backup, which is finished before
    // this returns, and neither is used by another thread meanwhile: `from` is held under the
    // template's lock and `to` is borrowed here.
    let (step, finish) = unsafe {
        let backup = ffi::sqlite3_backup_init(to.handle(), main.as_ptr(), from.handle(), main.as_ptr());
        assert!(!backup.is_null(), "backup did not start: SQLite error {}", ffi::sqlite3_errcode(to.handle()));
        let step = ffi::sqlite3_backup_step(backup, -1);
        (step, ffi::sqlite3_backup_finish(backup))
    };
    assert_eq!((step, finish), (ffi::SQLITE_DONE, ffi::SQLITE_OK), "backup failed");
}
