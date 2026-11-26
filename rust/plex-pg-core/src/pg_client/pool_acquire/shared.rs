use std::cell::Cell;
use std::os::raw::{c_char, c_void};

use super::super::PoolManager;

thread_local! {
    static POOL_RETRY_COUNT: Cell<i32> = const { Cell::new(0) };
}

#[derive(Clone, Copy)]
pub(super) struct AcquireCtx<'a> {
    pub(super) pm: &'a PoolManager,
    pub(super) current_thread: u64,
    pub(super) now: i64,
    pub(super) pool_size: usize,
    pub(super) db_path: *const c_char,
    pub(super) exclude_conn: *const c_void,
    /// Set by the first connect that fails in this pass over the pool. The
    /// server is then not answering, and every further connect in the pass
    /// would wait out its own timeout (five seconds each) for the same
    /// answer: the pass makes no more and adds no slot, and goes to its timed
    /// retry instead (upstream 61e037b: 1,826 connects and a pool grown to 78
    /// slots for eight prepares during an outage).
    pub(super) connect_failed: &'a Cell<bool>,
}

pub(super) enum AcquireDecision {
    Continue,
    Return(*mut c_void),
}

#[inline]
pub(super) fn retry_count_get() -> i32 {
    POOL_RETRY_COUNT.try_with(|c| c.get()).unwrap_or(0)
}

#[inline]
pub(super) fn retry_count_set(v: i32) {
    let _ = POOL_RETRY_COUNT.try_with(|c| c.set(v));
}
