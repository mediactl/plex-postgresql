use std::cell::Cell;
use std::ffi::CString;
use std::os::raw::{c_char, c_void};

use super::threading::current_thread_id;
use super::{pool, PoolManager};

mod maintenance;
mod pathing;
mod phases;
mod retry;
mod shared;

pub(crate) use maintenance::maintenance_due;
use maintenance::reclaim_zombies_and_reap;
use pathing::{remember_library_path, resolve_selected_pool_path};
use phases::{
    phase1_existing_ready, phase2_reuse_existing, phase3_create_empty, phase4_reclaim_error,
    phase5_autogrow,
};
use retry::phase6_retry;
use shared::{AcquireCtx, AcquireDecision};

pub(super) fn pool_get_connection_inner(db_path: *const c_char) -> *mut c_void {
    pool_get_connection_inner_excluding(db_path, std::ptr::null())
}

pub(super) fn pool_get_connection_inner_excluding(
    db_path: *const c_char,
    exclude_conn: *const c_void,
) -> *mut c_void {
    acquire_on(pool(), db_path, exclude_conn)
}

/// The acquire itself, on an explicit pool so a test can run it on one it
/// owns; production goes through `pool_get_connection_inner_excluding`.
pub(super) fn acquire_on(
    pm: &PoolManager,
    db_path: *const c_char,
    exclude_conn: *const c_void,
) -> *mut c_void {
    let selected_path = match resolve_selected_pool_path(pm, db_path, exclude_conn) {
        Some(path) => path,
        None => return std::ptr::null_mut(),
    };
    let selected_path_c =
        CString::new(selected_path.clone()).expect("selected library path cannot contain NUL");
    let db_path = selected_path_c.as_ptr();
    let current_thread = current_thread_id();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    remember_library_path(pm, &selected_path);

    let connect_failed = Cell::new(false);
    let ctx = AcquireCtx {
        pm,
        current_thread,
        now,
        pool_size: pm.pool_size(),
        db_path,
        exclude_conn,
        connect_failed: &connect_failed,
    };

    // Shrinking is a matter of time, not of demand. On a warm pool every
    // thread already owns a READY slot, so phase 1 answers every acquire and
    // the pass below it -- the only place idle connections were reclaimed and
    // reaped -- never ran again after the last burst: three pods held 35-48
    // PostgreSQL connections idle for a quarter of an hour. So the pass also
    // runs on the reaper's cadence whatever the acquire does next, at the
    // price of one atomic load per acquire when it is not due.
    if maintenance_due(pm, now) {
        reclaim_zombies_and_reap(&ctx);
    }

    if let AcquireDecision::Return(conn) = phase1_existing_ready(&ctx) {
        return conn;
    }

    // A thread that needs a slot reclaims now, whether or not the pass is due.
    reclaim_zombies_and_reap(&ctx);

    if let AcquireDecision::Return(conn) = phase2_reuse_existing(&ctx) {
        return conn;
    }
    if let AcquireDecision::Return(conn) = phase3_create_empty(&ctx) {
        return conn;
    }
    if let AcquireDecision::Return(conn) = phase4_reclaim_error(&ctx) {
        return conn;
    }
    if let AcquireDecision::Return(conn) = phase5_autogrow(&ctx) {
        return conn;
    }

    phase6_retry(&ctx)
}

#[cfg(test)]
mod tests {
    use super::super::connection_lifecycle::FAILED_CONNECTS;
    use super::super::SLOT_ERROR;
    use super::*;
    use std::sync::atomic::Ordering;

    fn ctx<'a>(pm: &'a PoolManager, failed: &'a Cell<bool>) -> AcquireCtx<'a> {
        AcquireCtx {
            pm,
            current_thread: 0x7001,
            now: 1_000,
            pool_size: pm.pool_size(),
            db_path: std::ptr::null(),
            exclude_conn: std::ptr::null(),
            connect_failed: failed,
        }
    }

    /// The server does not answer for the length of `f`; what it returns is
    /// how many connects were made.
    fn while_the_server_is_down(f: impl FnOnce()) -> usize {
        FAILED_CONNECTS.with(|c| c.set(Some(0)));
        f();
        FAILED_CONNECTS.with(|c| c.replace(None)).unwrap_or(0)
    }

    #[test]
    fn one_failed_connect_ends_the_pass_over_the_empty_slots() {
        // Eight empty slots: the pass used to try each, at a connect timeout
        // apiece, and then grow the pool to try one more.
        let pm = PoolManager::new(8, 16);
        let failed = Cell::new(false);
        let c = ctx(&pm, &failed);

        let connects = while_the_server_is_down(|| {
            assert!(matches!(phase3_create_empty(&c), AcquireDecision::Continue));
            assert!(matches!(
                phase4_reclaim_error(&c),
                AcquireDecision::Continue
            ));
            assert!(matches!(phase5_autogrow(&c), AcquireDecision::Continue));
        });

        assert_eq!(connects, 1, "one connect told the pass the server is down");
        assert!(failed.get());
        assert_eq!(
            pm.configured_size.load(Ordering::Relaxed),
            8,
            "no slot added"
        );
        for slot in pm.slots.iter().take(8) {
            assert!(slot.conn.load(Ordering::Relaxed).is_null());
            assert_eq!(
                slot.owner_thread.load(Ordering::Relaxed),
                0,
                "nothing kept claimed"
            );
        }
    }

    #[test]
    fn one_failed_connect_ends_the_pass_over_the_slots_in_error() {
        // A pool with no empty slot left to try: three of its slots failed
        // earlier and are waiting to be connected again.
        let pm = PoolManager::new(3, 3);
        for slot in pm.slots.iter() {
            slot.state.store(SLOT_ERROR, Ordering::Relaxed);
        }
        let failed = Cell::new(false);
        let c = ctx(&pm, &failed);

        let connects = while_the_server_is_down(|| {
            assert!(matches!(
                phase4_reclaim_error(&c),
                AcquireDecision::Continue
            ));
        });

        assert_eq!(connects, 1);
        assert!(failed.get());
        let still_in_error = pm
            .slots
            .iter()
            .filter(|s| s.state.load(Ordering::Relaxed) == SLOT_ERROR)
            .count();
        assert_eq!(still_in_error, 2, "the other two wait for the next pass");
    }

    #[test]
    fn a_pass_with_no_failed_connect_still_tries_to_grow() {
        // The flag is what stops growth, not the failure of growth itself: a
        // full pool whose every slot is busy grows by one and connects once.
        let pm = PoolManager::new(2, 4);
        for (i, slot) in pm.slots.iter().take(2).enumerate() {
            slot.state
                .store(super::super::SLOT_READY, Ordering::Relaxed);
            slot.owner_thread
                .store(0x9000 + i as u64, Ordering::Relaxed);
            slot.conn
                .store(std::ptr::NonNull::dangling().as_ptr(), Ordering::Relaxed);
        }
        let failed = Cell::new(false);
        let c = ctx(&pm, &failed);

        let connects = while_the_server_is_down(|| {
            assert!(matches!(phase3_create_empty(&c), AcquireDecision::Continue));
            assert!(!failed.get(), "no empty slot, so no connect was made");
            assert!(matches!(phase5_autogrow(&c), AcquireDecision::Continue));
        });

        assert_eq!(connects, 1);
        assert!(failed.get(), "the failed growth is a failed connect too");
        // The slots never held a connection this test owns.
        for slot in pm.slots.iter().take(2) {
            slot.conn.store(std::ptr::null_mut(), Ordering::Relaxed);
        }
    }
}
