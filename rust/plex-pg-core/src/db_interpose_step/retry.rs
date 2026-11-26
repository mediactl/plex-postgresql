use std::cell::Cell;
use std::os::raw::{c_int, c_void};

use crate::db_interpose_conn_utils::log_error;
use crate::ffi_types::{sqlite3_stmt, PgStmt};

const PG_RETRY_MAX_DELAYS: usize = 10;

thread_local! {
    static STEP_PG_CONN_ERROR: Cell<i32> = const { Cell::new(0) };
    static STEP_RETRY_COUNT: Cell<i32> = const { Cell::new(0) };
}

pub(super) fn note_pg_conn_error() {
    STEP_PG_CONN_ERROR.with(|c| c.set(1));
}

pub(super) fn maybe_retry_step(p_stmt: *mut sqlite3_stmt, rc: c_int) -> Option<c_int> {
    let mut delays = [0i32; PG_RETRY_MAX_DELAYS];
    let mut max_retries = 0i32;
    crate::pg_config::pg_config_get_retry_delays(delays.as_mut_ptr(), &mut max_retries);

    let retry_count = STEP_RETRY_COUNT.with(|c| c.get());
    if rc == super::SQLITE_ERROR && retry_count < max_retries {
        let pg_stmt = crate::pg_statement::rust_stmt_find(p_stmt as usize) as *mut PgStmt;
        let conn_error = STEP_PG_CONN_ERROR.with(|c| c.get());
        if !pg_stmt.is_null() && conn_error != 0 {
            let pg_stmt_ref = unsafe { &mut *pg_stmt };
            if pg_stmt_ref.is_pg != 0 {
                STEP_PG_CONN_ERROR.with(|c| c.set(0));
                let delay = delays[retry_count as usize];
                let new_count = retry_count + 1;
                STEP_RETRY_COUNT.with(|c| c.set(new_count));
                log_error(&format!(
                    "step: PG conn error, retry {}/{} in {}ms (thread {:p})",
                    new_count,
                    max_retries,
                    delay,
                    unsafe { libc::pthread_self() } as *mut c_void
                ));

                unsafe {
                    let _stmt_guard = PgStmt::lock_mutex(pg_stmt);
                    crate::pg_statement::rust_stmt_clear_result(pg_stmt);
                }

                let delay_ms = if delay < 0 { 0 } else { delay as u32 };
                unsafe {
                    libc::usleep(delay_ms.saturating_mul(1000));
                }
                STEP_PG_CONN_ERROR.with(|c| c.set(0));
                let retry_rc = super::rust_my_sqlite3_step(p_stmt);

                if new_count > 0 && retry_rc != super::SQLITE_ERROR {
                    log_error(&format!(
                        "step: retry succeeded after {} attempt(s)",
                        new_count
                    ));
                }
                STEP_RETRY_COUNT.with(|c| c.set(0));
                return Some(retry_rc);
            }
        }
    }

    if retry_count > 0 {
        if rc == super::SQLITE_ERROR {
            log_error("step: retries exhausted, returning SQLITE_ERROR");
        }
        STEP_RETRY_COUNT.with(|c| c.set(0));
    }

    // The flag is this step's. Left set -- after the retries ran out, or by a
    // step that noted a lost connection and was not retried -- it made the
    // next statement to fail on this thread, for any reason, a lost
    // connection too, retried on the whole delay schedule (upstream 661f865).
    STEP_PG_CONN_ERROR.with(|c| c.set(0));
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noted() -> bool {
        STEP_PG_CONN_ERROR.with(|c| c.get()) != 0
    }

    #[test]
    fn a_noted_connection_error_does_not_outlive_its_step() {
        // A step that noted a lost connection and is not retried: no
        // statement of the shim's here, as when the retries have run out.
        note_pg_conn_error();
        assert!(noted());
        assert_eq!(
            maybe_retry_step(std::ptr::null_mut(), super::super::SQLITE_ERROR),
            None
        );
        assert!(!noted(), "the next failing step would have been retried");

        // And one that noted it but did not fail.
        note_pg_conn_error();
        assert_eq!(maybe_retry_step(std::ptr::null_mut(), 101), None);
        assert!(!noted());
    }
}
