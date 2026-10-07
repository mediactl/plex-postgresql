use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::db_interpose_conn_utils::{cstr_to_string_or, log_error};
use crate::ffi_types::sqlite3_stmt;

use super::platform_print_backtrace;

const FINALIZED_RING_SIZE: usize = 2048;
const FINALIZED_RECENT_MS: u64 = 2000;
const PREPARED_RING_SIZE: usize = 4096;

static FINALIZED_RING_IDX: AtomicU32 = AtomicU32::new(0);
static PREPARED_RING_IDX: AtomicU32 = AtomicU32::new(0);
static CLEAR_BINDINGS_COUNTER: AtomicU64 = AtomicU64::new(0);

static SKIP_CLEAR_BINDINGS_CACHED: AtomicI32 = AtomicI32::new(-1);
static TRACE_CLEAR_BINDINGS_CACHED: AtomicI32 = AtomicI32::new(-1);

#[repr(C)]
#[derive(Copy, Clone)]
struct FinalizedEntry {
    stmt: *mut sqlite3_stmt,
    ts_ns: u64,
    tid: u64,
    is_pg: c_int,
    sql: [c_char; 256],
}

// SAFETY: FinalizedEntry is only accessed while the FINALIZED_RING mutex is held.
// The raw pointer `stmt` is used as an opaque identifier (compared by address),
// never dereferenced through this ring.
unsafe impl Send for FinalizedEntry {}

impl FinalizedEntry {
    const fn empty() -> Self {
        Self {
            stmt: ptr::null_mut(),
            ts_ns: 0,
            tid: 0,
            is_pg: 0,
            sql: [0; 256],
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
struct PreparedEntry {
    stmt: *mut sqlite3_stmt,
    ts_ns: u64,
    tid: u64,
    sql: [c_char; 256],
}

// SAFETY: Same as FinalizedEntry — accessed only under PREPARED_RING mutex,
// raw pointer used as opaque key.
unsafe impl Send for PreparedEntry {}

impl PreparedEntry {
    const fn empty() -> Self {
        Self {
            stmt: ptr::null_mut(),
            ts_ns: 0,
            tid: 0,
            sql: [0; 256],
        }
    }
}

// Use vec![].into_boxed_slice() to allocate directly on the heap.
// Box::new([T; N]) would place the array on the stack first (~560KB for
// FinalizedEntry × 2048), exceeding Plex's 544K worker thread stacks.
static FINALIZED_RING: LazyLock<Mutex<Box<[FinalizedEntry]>>> = LazyLock::new(|| {
    Mutex::new(vec![FinalizedEntry::empty(); FINALIZED_RING_SIZE].into_boxed_slice())
});

static PREPARED_RING: LazyLock<Mutex<Box<[PreparedEntry]>>> = LazyLock::new(|| {
    Mutex::new(vec![PreparedEntry::empty(); PREPARED_RING_SIZE].into_boxed_slice())
});

pub(super) fn skip_clear_bindings_on_finalized() -> bool {
    let cached = SKIP_CLEAR_BINDINGS_CACHED.load(Ordering::Relaxed);
    if cached != -1 {
        return cached == 1;
    }
    let name = b"PLEX_PG_SKIP_CLEAR_BINDINGS_FINALIZED\0";
    let val = unsafe {
        let env = libc::getenv(name.as_ptr() as *const c_char);
        if env.is_null() {
            1
        } else {
            crate::db_interpose_helpers::rust_env_truthy(env)
        }
    };
    let flag = if val != 0 { 1 } else { 0 };
    SKIP_CLEAR_BINDINGS_CACHED.store(flag, Ordering::Relaxed);
    flag == 1
}

fn trace_clear_bindings_enabled() -> bool {
    let cached = TRACE_CLEAR_BINDINGS_CACHED.load(Ordering::Relaxed);
    if cached != -1 {
        return cached == 1;
    }
    let name = b"PLEX_PG_TRACE_CLEAR_BINDINGS\0";
    let val = unsafe {
        let env = libc::getenv(name.as_ptr() as *const c_char);
        crate::db_interpose_helpers::rust_env_truthy(env)
    };
    let flag = if val != 0 { 1 } else { 0 };
    TRACE_CLEAR_BINDINGS_CACHED.store(flag, Ordering::Relaxed);
    flag == 1
}

fn now_monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    (ts.tv_sec as u64) * 1_000_000_000u64 + (ts.tv_nsec as u64)
}

unsafe fn write_sql_buf(buf: &mut [c_char; 256], sql: *const c_char) {
    buf[0] = 0;
    if sql.is_null() || *sql == 0 {
        return;
    }
    libc::strncpy(buf.as_mut_ptr(), sql, buf.len() - 1);
    buf[buf.len() - 1] = 0;
}

pub(super) unsafe fn remember_finalized_stmt(
    stmt: *mut sqlite3_stmt,
    sql: *const c_char,
    is_pg: c_int,
) {
    if stmt.is_null() {
        return;
    }
    let idx = FINALIZED_RING_IDX.fetch_add(1, Ordering::Relaxed) as usize % FINALIZED_RING_SIZE;
    let mut ring = FINALIZED_RING.lock().unwrap_or_else(|e| e.into_inner());
    let entry = &mut ring[idx];
    entry.stmt = stmt;
    entry.ts_ns = now_monotonic_ns();
    entry.tid = libc::pthread_self() as u64;
    entry.is_pg = is_pg;
    write_sql_buf(&mut entry.sql, sql);
}

pub(super) unsafe fn remember_prepared_stmt(stmt: *mut sqlite3_stmt, sql: *const c_char) {
    if stmt.is_null() {
        return;
    }
    let idx = PREPARED_RING_IDX.fetch_add(1, Ordering::Relaxed) as usize % PREPARED_RING_SIZE;
    let mut ring = PREPARED_RING.lock().unwrap_or_else(|e| e.into_inner());
    let entry = &mut ring[idx];
    entry.stmt = stmt;
    entry.ts_ns = now_monotonic_ns();
    entry.tid = libc::pthread_self() as u64;
    write_sql_buf(&mut entry.sql, sql);
}

pub(super) unsafe fn is_prepared_stmt(stmt: *mut sqlite3_stmt) -> bool {
    if stmt.is_null() {
        return false;
    }
    let ring = PREPARED_RING.lock().unwrap_or_else(|e| e.into_inner());
    for i in 0..PREPARED_RING_SIZE {
        if ring[i].stmt == stmt {
            return true;
        }
    }
    false
}

pub(super) unsafe fn clear_prepared_stmt(stmt: *mut sqlite3_stmt) {
    if stmt.is_null() {
        return;
    }
    let mut ring = PREPARED_RING.lock().unwrap_or_else(|e| e.into_inner());
    for i in 0..PREPARED_RING_SIZE {
        if ring[i].stmt == stmt {
            ring[i] = PreparedEntry::empty();
            return;
        }
    }
}

/// Forgets every record of `stmt` having been finalized: a statement the shim
/// has seen prepared at that address is live. An address can hold many
/// records -- SQLite's own statements reuse one address in a loop (VACUUM's
/// copy of the schema) -- and clearing only the first left it "recently
/// finalized", so the new statement's own finalize was skipped and it leaked.
pub(super) unsafe fn clear_finalized_entry(stmt: *mut sqlite3_stmt) {
    if stmt.is_null() {
        return;
    }
    let mut ring = FINALIZED_RING.lock().unwrap_or_else(|e| e.into_inner());
    for i in 0..FINALIZED_RING_SIZE {
        if ring[i].stmt == stmt {
            ring[i] = FinalizedEntry::empty();
        }
    }
}

fn find_finalized_entry(stmt: *mut sqlite3_stmt) -> Option<FinalizedEntry> {
    if stmt.is_null() {
        return None;
    }
    let ring = FINALIZED_RING.lock().unwrap_or_else(|e| e.into_inner());
    for i in 0..FINALIZED_RING_SIZE {
        if ring[i].stmt == stmt {
            return Some(ring[i]);
        }
    }
    None
}

pub(super) unsafe fn log_clear_bindings_anomaly(reason: &str, stmt: *mut sqlite3_stmt) {
    if !trace_clear_bindings_enabled() {
        return;
    }
    let n = CLEAR_BINDINGS_COUNTER.fetch_add(1, Ordering::Relaxed);
    if n >= 5 && !n.is_multiple_of(1000) {
        return;
    }

    if let Some(entry) = find_finalized_entry(stmt) {
        if entry.ts_ns != 0 {
            let now_ns = now_monotonic_ns();
            let age_ms = if now_ns > entry.ts_ns {
                (now_ns - entry.ts_ns) / 1_000_000
            } else {
                0
            };
            let sql = if entry.sql[0] == 0 {
                "NULL".to_string()
            } else {
                cstr_to_string_or(entry.sql.as_ptr(), "NULL")
            };
            log_error(&format!(
                "CLEAR_BINDINGS anomaly: {} stmt={:p} age_ms={} finalize_tid=0x{:x} is_pg={} sql={}",
                reason,
                stmt,
                age_ms,
                entry.tid,
                entry.is_pg,
                sql
            ));
        } else {
            log_error(&format!(
                "CLEAR_BINDINGS anomaly: {} stmt={:p} (no finalize metadata)",
                reason, stmt
            ));
        }
    } else {
        log_error(&format!(
            "CLEAR_BINDINGS anomaly: {} stmt={:p} (no finalize metadata)",
            reason, stmt
        ));
    }

    if let Ok(cs) = CString::new("CLEAR_BINDINGS anomaly") {
        platform_print_backtrace(cs.as_ptr(), 2);
    }
}

pub(super) unsafe fn is_recently_finalized_stmt(stmt: *mut sqlite3_stmt) -> bool {
    let Some(entry) = find_finalized_entry(stmt) else {
        return false;
    };
    if entry.ts_ns == 0 {
        return false;
    }
    let now_ns = now_monotonic_ns();
    let age_ms = if now_ns > entry.ts_ns {
        (now_ns - entry.ts_ns) / 1_000_000
    } else {
        0
    };
    age_ms <= FINALIZED_RECENT_MS
}

#[cfg(test)]
pub(super) unsafe fn reset_test_state() {
    FINALIZED_RING_IDX.store(0, Ordering::Relaxed);
    PREPARED_RING_IDX.store(0, Ordering::Relaxed);
    CLEAR_BINDINGS_COUNTER.store(0, Ordering::Relaxed);
    SKIP_CLEAR_BINDINGS_CACHED.store(1, Ordering::Relaxed);
    TRACE_CLEAR_BINDINGS_CACHED.store(0, Ordering::Relaxed);

    {
        let mut ring = FINALIZED_RING.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..FINALIZED_RING_SIZE {
            ring[i] = FinalizedEntry::empty();
        }
    }
    {
        let mut ring = PREPARED_RING.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..PREPARED_RING_SIZE {
            ring[i] = PreparedEntry::empty();
        }
    }
}
