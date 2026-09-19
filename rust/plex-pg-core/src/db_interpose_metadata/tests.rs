use crate::db_interpose_common::{
    get_orig_sqlite3_close, get_orig_sqlite3_exec, get_orig_sqlite3_open,
    rust_common_load_sqlite_symbols,
};
use crate::ffi_types::sqlite3;
use crate::pg_client::rust_set_global_last_insert_rowid;
use libc::{RTLD_DEFAULT, RTLD_LAZY};
use std::ffi::CString;

/// Opens a throwaway in-memory database through the SQLite the shim resolved,
/// or None when this build has none to talk to.
///
/// It stands in for the database Plex keeps its statistics in: opened through
/// the interposer, but left on SQLite rather than redirected to PostgreSQL.
fn open_passthrough_db() -> Option<*mut sqlite3> {
    let names = if cfg!(target_os = "macos") {
        vec!["libsqlite3.dylib", "/usr/lib/libsqlite3.dylib"]
    } else {
        vec!["libsqlite3.so.0", "libsqlite3.so"]
    };
    let mut handle = std::ptr::null_mut();
    for name in names {
        let name = CString::new(name).unwrap();
        unsafe {
            handle = libc::dlopen(name.as_ptr(), RTLD_LAZY);
        }
        if !handle.is_null() {
            break;
        }
    }
    if handle.is_null() {
        handle = RTLD_DEFAULT;
    }
    rust_common_load_sqlite_symbols(handle);

    let open = get_orig_sqlite3_open()?;
    let path = CString::new(":memory:").unwrap();
    let mut db: *mut sqlite3 = std::ptr::null_mut();
    let rc = unsafe { open(path.as_ptr(), &mut db) };
    if rc != 0 || db.is_null() {
        return None;
    }
    Some(db)
}

fn exec(db: *mut sqlite3, sql: &str) {
    let exec = get_orig_sqlite3_exec().expect("sqlite3_exec");
    let sql = CString::new(sql).unwrap();
    let rc = unsafe {
        exec(
            db,
            sql.as_ptr(),
            None,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "sqlite3_exec failed for {:?}", sql);
}

#[test]
fn metadata_calls_on_a_passthrough_database_answer_from_sqlite() {
    // Plex keeps its statistics in an in-memory database that the shim opens
    // but deliberately does not redirect -- `OPEN: :memory: (redirect=0)`.
    // `sqlite3_last_insert_rowid` and `sqlite3_changes` are interposed all the
    // same, and on such a handle there is no PostgreSQL connection to answer
    // from.
    //
    // Falling back to the shim's global PostgreSQL row id hands Plex an id
    // from an entirely different database. Its insert-then-read-the-id loop
    // never agrees with itself, and it retries once a second for ever: startup
    // reaches the statistics fixups and stops there, serving 503, with the
    // shim log showing `last_insert_rowid: CALLED db=... pg_conn=NULL` once a
    // second between thousands of `DB_HANDLE` calls.
    //
    // A handle the shim did not take over has to be answered by the SQLite it
    // was left on.
    let Some(db) = open_passthrough_db() else {
        eprintln!("no SQLite available; skipping passthrough metadata test");
        return;
    };

    // A plausible row id from the PostgreSQL side, so falling back to it is
    // visible rather than coincidentally right.
    rust_set_global_last_insert_rowid(4242);

    exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
    exec(db, "INSERT INTO t (v) VALUES ('first')");

    assert_eq!(
        super::rust_my_sqlite3_last_insert_rowid(db),
        1,
        "answered with the PostgreSQL global instead of this database's row id"
    );
    assert_eq!(
        super::rust_my_sqlite3_changes(db),
        1,
        "answered 0 instead of this database's change count"
    );
    assert_eq!(
        super::rust_my_sqlite3_changes64(db),
        1,
        "answered 0 instead of this database's change count"
    );

    exec(db, "INSERT INTO t (v) VALUES ('second')");
    assert_eq!(
        super::rust_my_sqlite3_last_insert_rowid(db),
        2,
        "row id did not follow the second insert"
    );

    rust_set_global_last_insert_rowid(0);
    if let Some(close) = get_orig_sqlite3_close() {
        unsafe {
            close(db);
        }
    }
}
