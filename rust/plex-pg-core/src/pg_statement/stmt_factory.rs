use std::os::raw::c_char;
use std::sync::atomic::Ordering;

use crate::ffi_types::{sqlite3_stmt, PgConnection, PgStmt};

pub fn rust_stmt_create(
    conn: *mut PgConnection,
    sql: *const c_char,
    shadow_stmt: *mut sqlite3_stmt,
) -> *mut PgStmt {
    let mut stmt = PgStmt::new();
    stmt.set_conn(conn);
    stmt.shadow_stmt = shadow_stmt;
    stmt.sql = if sql.is_null() {
        std::ptr::null_mut()
    } else {
        unsafe { libc::strdup(sql) }
    };
    stmt.ref_count.store(1, Ordering::Release);

    Box::into_raw(Box::new(stmt))
}
