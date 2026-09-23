use super::*;

// ═════════════════════════════════════════════════════════════════════════
// The pool shrinks back on its own
// ═════════════════════════════════════════════════════════════════════════

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

unsafe fn alloc_fake_pg_connection() -> *mut PgConnection {
    let conn = libc::calloc(1, std::mem::size_of::<PgConnection>()) as *mut PgConnection;
    assert!(!conn.is_null());
    conn
}

#[test]
fn an_idle_unreferenced_connection_is_destroyed_even_when_no_thread_needs_a_slot() {
    // A warm pool never shrank. Zombie reclaim and the idle reaper ran only
    // after a thread missed the phase-1 fast path, and in steady state every
    // Plex thread already owns a READY slot, so nothing missed and nothing
    // was ever reaped: three pods sat on 35-48 PostgreSQL connections, idle
    // for a quarter of an hour, until the next playback burst brought new
    // threads. Maintenance has to be a matter of time, not of demand.
    let pm = PoolManager::new(3, 64);
    pm.idle_timeout_secs.store(300, Ordering::Relaxed);
    let now = unix_now();

    // This thread already owns a good connection: the fast path, which is
    // where every acquire on a warm pool ends. PQstatus is stood in for, so
    // "good" needs no server.
    crate::libpq_helpers::PQ_STATUS_OVERRIDE.with(|c| c.set(Some(0)) /* CONNECTION_OK */);
    let mine = &pm.slots[0];
    let my_conn = unsafe { alloc_fake_pg_connection() };
    unsafe {
        (*my_conn).is_pg_active = 1;
        // Never dereferenced: status is stood in for.
        (*my_conn).conn = std::ptr::NonNull::dangling().as_ptr();
    }
    crate::ffi_types::conn_ref(my_conn);
    mine.conn.store(my_conn as *mut c_void, Ordering::Relaxed);
    mine.state.store(SLOT_READY, Ordering::Relaxed);
    mine.owner_thread
        .store(threading::current_thread_id(), Ordering::Relaxed);
    mine.last_used.store(now, Ordering::Relaxed);

    // Another thread's connection: no handle refers to it, idle past the
    // timeout, and the reaper's minute is up.
    let idle = &pm.slots[1];
    let idle_conn = unsafe { alloc_fake_pg_connection() };
    unsafe {
        (*idle_conn).is_pg_active = 1;
    }
    crate::ffi_types::conn_ref(idle_conn); // the pool slot's reference
    idle.conn.store(idle_conn as *mut c_void, Ordering::Relaxed);
    idle.state.store(SLOT_READY, Ordering::Relaxed);
    idle.owner_thread.store(0xDEAD_0001, Ordering::Relaxed);
    idle.last_used.store(now - 400, Ordering::Relaxed);
    assert!(!pm.slot_is_referenced(1));
    pm.last_reap_time.store(now - 61, Ordering::Relaxed);

    let path = c("/data/com.plexapp.plugins.library.db");
    let got = pool_acquire::acquire_on(&pm, path.as_ptr(), std::ptr::null());
    crate::libpq_helpers::PQ_STATUS_OVERRIDE.with(|c| c.set(None));
    assert_eq!(got, my_conn as *mut c_void, "the fast path handed back this thread's own connection");
    assert_eq!(mine.state.load(Ordering::Relaxed), SLOT_READY);

    assert!(
        idle.conn.load(Ordering::Relaxed).is_null(),
        "the idle connection was destroyed although no thread needed a slot"
    );
    assert_eq!(idle.state.load(Ordering::Relaxed), SLOT_FREE);
    assert!(
        pm.last_reap_time.load(Ordering::Relaxed) >= now,
        "the reaper ran on the fast path"
    );
}

#[test]
fn maintenance_is_a_matter_of_time() {
    // Due once the reaper's interval has passed since it last ran, and not
    // before, so the fast path pays one atomic load per acquire.
    let pm = PoolManager::new(3, 64);
    pm.last_reap_time.store(1_000, Ordering::Relaxed);
    assert!(!pool_acquire::maintenance_due(&pm, 1_000 + 59));
    assert!(pool_acquire::maintenance_due(&pm, 1_000 + 60));
    assert!(pool_acquire::maintenance_due(&pm, 1_000 + 3_600));
}
