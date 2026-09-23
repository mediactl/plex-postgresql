use super::*;

// ═════════════════════════════════════════════════════════════════════════
// Pool Manager
// ═════════════════════════════════════════════════════════════════════════

unsafe fn alloc_fake_pg_connection() -> *mut PgConnection {
    let conn = libc::calloc(1, std::mem::size_of::<PgConnection>()) as *mut PgConnection;
    assert!(!conn.is_null());
    conn
}

#[test]
fn pool_manager_creates_slots() {
    let pm = PoolManager::new(10, 64);
    assert_eq!(pm.pool_size(), 10);
    // slots.len() tracks runtime max for auto-grow support
    assert_eq!(pm.slots.len(), 64);
    for slot in &pm.slots {
        assert_eq!(slot.state.load(Ordering::Relaxed), SLOT_FREE);
    }
}

#[test]
fn pool_manager_validate_connection_found() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = 0xBEEF as *mut c_void;
    pm.slots[2].conn.store(fake_conn, Ordering::Relaxed);
    pm.slots[2].state.store(SLOT_READY, Ordering::Relaxed);
    assert!(pm.validate_connection(fake_conn));
}

#[test]
fn pool_manager_validate_connection_not_found() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = 0xBEEF as *mut c_void;
    assert!(!pm.validate_connection(fake_conn));
}

#[test]
fn pool_manager_validate_connection_not_ready() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = 0xBEEF as *mut c_void;
    pm.slots[2].conn.store(fake_conn, Ordering::Relaxed);
    pm.slots[2].state.store(SLOT_FREE, Ordering::Relaxed); // not READY
    assert!(!pm.validate_connection(fake_conn));
}

#[test]
fn pool_manager_clear_streaming_active_returns_false_for_unknown_conn() {
    let pm = PoolManager::new(5, 64);
    assert!(!pm.clear_streaming_active(0xBEEF as *const c_void));
}

#[test]
fn pool_manager_clear_streaming_active_releases_non_ready_slot() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = unsafe { alloc_fake_pg_connection() };
    unsafe {
        (*fake_conn).streaming_active.store(1, Ordering::Release);
    }
    pm.slots[1]
        .conn
        .store(fake_conn as *mut c_void, Ordering::Relaxed);
    pm.slots[1].state.store(SLOT_RESERVED, Ordering::Relaxed);

    assert!(pm.clear_streaming_active(fake_conn as *const c_void));
    unsafe {
        assert_eq!((*fake_conn).streaming_active.load(Ordering::Acquire), 0);
        libc::free(fake_conn as *mut c_void);
    }
}

#[test]
fn pool_manager_clear_streaming_active_releases_ready_slot() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = unsafe { alloc_fake_pg_connection() };
    unsafe {
        (*fake_conn).streaming_active.store(1, Ordering::Release);
    }
    pm.slots[2]
        .conn
        .store(fake_conn as *mut c_void, Ordering::Relaxed);
    pm.slots[2].state.store(SLOT_READY, Ordering::Relaxed);

    assert!(pm.clear_streaming_active(fake_conn as *const c_void));
    unsafe {
        assert_eq!((*fake_conn).streaming_active.load(Ordering::Acquire), 0);
        libc::free(fake_conn as *mut c_void);
    }
}

#[test]
fn pool_manager_clear_streaming_active_null_returns_false() {
    let pm = PoolManager::new(5, 64);
    assert!(!pm.clear_streaming_active(std::ptr::null()));
}

#[test]
fn pool_manager_live_pool_connection_registry_tracks_membership() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = 0xBEEF as *const c_void;
    assert!(!pm.is_live_pool_connection(fake_conn));
    pm.note_live_pool_connection(fake_conn);
    assert!(pm.is_live_pool_connection(fake_conn));
    pm.forget_live_pool_connection(fake_conn);
    assert!(!pm.is_live_pool_connection(fake_conn));
}

#[test]
fn pool_manager_is_live_pool_connection_rejects_unknown_conn() {
    let pm = PoolManager::new(5, 64);
    assert!(!pm.is_live_pool_connection(0xBEEF as *const c_void));
}

#[test]
fn pool_manager_is_tracked_connection_accepts_live_pool_conn() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = 0xBEEF as *const c_void;
    pm.note_live_pool_connection(fake_conn);
    assert!(pm.is_tracked_connection(fake_conn));
}

#[test]
fn pool_manager_is_tracked_connection_accepts_registered_conn() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = 0xBEEF as *const c_void;
    pm.registry.register(0x100, fake_conn as usize);
    assert!(pm.is_tracked_connection(fake_conn));
}

#[test]
fn pool_manager_is_tracked_connection_rejects_unknown_conn() {
    let pm = PoolManager::new(5, 64);
    assert!(!pm.is_tracked_connection(0xBEEF as *const c_void));
}

#[test]
fn pool_manager_touch_connection() {
    let pm = PoolManager::new(5, 64);
    let fake_conn = 0xBEEF as *mut c_void;
    pm.slots[1].conn.store(fake_conn, Ordering::Relaxed);
    pm.slots[1].last_used.store(100, Ordering::Relaxed);

    pm.touch_connection(fake_conn, 999);
    assert_eq!(pm.slots[1].last_used.load(Ordering::Relaxed), 999);
}

#[test]
fn pool_manager_touch_unknown_conn_is_noop() {
    let pm = PoolManager::new(5, 64);
    pm.touch_connection(0xBEEF as *const c_void, 999);
    // Should not panic or modify anything
}

#[test]
fn pool_manager_slot_held_by_a_database_handle_is_referenced() {
    // The zombie reclaim decided a slot was abandoned from its idle time and
    // whether the thread that opened it was still alive. Neither says
    // anything about whether Plex still holds the handle: it opens twenty at
    // startup and keeps them for the life of the process, while the worker
    // threads that used them come and go.
    //
    // Reclaiming one of those handed a live PGconn to a second thread, and
    // libpq is not thread-safe per connection, so Plex corrupted its heap and
    // died with no dump and no segfault left behind to explain it.
    let pm = PoolManager::new(5, 64);
    assert!(!pm.slot_is_referenced(3));

    pm.db_to_pool.assign(0x100, 3);
    assert!(pm.slot_is_referenced(3), "a handle holds this slot");
    assert!(!pm.slot_is_referenced(4), "and only this slot");

    pm.db_to_pool.release(0x100);
    assert!(
        !pm.slot_is_referenced(3),
        "once Plex closes the handle the slot is genuinely free"
    );
}
