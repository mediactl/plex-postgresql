use super::*;

// ═════════════════════════════════════════════════════════════════════════
// Reaper
// ═════════════════════════════════════════════════════════════════════════

#[test]
fn reaper_ignores_active_slots() {
    let pm = PoolManager::new(5, 64);
    let fake = 0xBEEF as *mut c_void;
    pm.slots[0].conn.store(fake, Ordering::Relaxed);
    pm.slots[0].state.store(SLOT_READY, Ordering::Relaxed);
    pm.slots[0].last_used.store(0, Ordering::Relaxed);

    // Reaper should not touch READY slots
    let to_destroy = pm.reap_idle(10000);
    assert!(to_destroy.is_empty());
    assert_eq!(pm.slots[0].state.load(Ordering::Relaxed), SLOT_READY);
}

#[test]
fn reaper_destroys_idle_free_slots() {
    let pm = PoolManager::new(5, 64);
    pm.idle_timeout_secs.store(60, Ordering::Relaxed);
    let fake = 0xBEEF as *mut c_void;

    // Slot 0: FREE with connection, last used 100 seconds ago
    pm.slots[0].conn.store(fake, Ordering::Relaxed);
    pm.slots[0].state.store(SLOT_FREE, Ordering::Relaxed);
    pm.slots[0].last_used.store(100, Ordering::Relaxed);

    let to_destroy = pm.reap_idle(200); // 200 - 100 = 100 > 60 timeout
    assert_eq!(to_destroy.len(), 1);
    assert_eq!(to_destroy[0].0, 0); // slot index
    assert_eq!(to_destroy[0].1, fake); // connection pointer

    // Slot should now be FREE with null conn
    assert_eq!(pm.slots[0].state.load(Ordering::Relaxed), SLOT_FREE);
    assert!(pm.slots[0].conn.load(Ordering::Relaxed).is_null());
}

#[test]
fn reaper_skips_recently_used() {
    let pm = PoolManager::new(5, 64);
    pm.idle_timeout_secs.store(60, Ordering::Relaxed);
    let fake = 0xBEEF as *mut c_void;

    pm.slots[0].conn.store(fake, Ordering::Relaxed);
    pm.slots[0].state.store(SLOT_FREE, Ordering::Relaxed);
    pm.slots[0].last_used.store(180, Ordering::Relaxed);

    let to_destroy = pm.reap_idle(200); // 200 - 180 = 20 < 60 timeout
    assert!(to_destroy.is_empty());
    // Connection should still be there
    assert_eq!(pm.slots[0].conn.load(Ordering::Relaxed), fake);
}

#[test]
fn reaper_skips_free_slot_without_conn() {
    let pm = PoolManager::new(5, 64);
    pm.idle_timeout_secs.store(60, Ordering::Relaxed);
    // Slot 0: FREE, no connection
    pm.slots[0].state.store(SLOT_FREE, Ordering::Relaxed);
    pm.slots[0].last_used.store(0, Ordering::Relaxed);

    let to_destroy = pm.reap_idle(10000);
    assert!(to_destroy.is_empty());
}

#[test]
fn reaper_bumps_generation_before_destroying() {
    let pm = PoolManager::new(5, 64);
    pm.idle_timeout_secs.store(60, Ordering::Relaxed);
    let fake = 0xBEEF as *mut c_void;

    pm.slots[0].conn.store(fake, Ordering::Relaxed);
    pm.slots[0].state.store(SLOT_FREE, Ordering::Relaxed);
    pm.slots[0].last_used.store(0, Ordering::Relaxed);
    pm.slots[0].generation.store(10, Ordering::Relaxed);

    let _to_destroy = pm.reap_idle(10000);

    // Generation must have been incremented (invalidates TLS caches)
    assert!(pm.slots[0].generation.load(Ordering::Relaxed) > 10);
}

#[test]
fn reaper_critical_fix_tls_generation_mismatch() {
    // This test verifies the fix for CRITICAL #1 + #2:
    // After reaping, a TLS-cached (slot_index, generation) pair must
    // fail validation because the generation was bumped.
    let pm = PoolManager::new(5, 64);
    pm.idle_timeout_secs.store(60, Ordering::Relaxed);
    let fake = 0xBEEF as *mut c_void;

    pm.slots[2].conn.store(fake, Ordering::Relaxed);
    pm.slots[2].state.store(SLOT_FREE, Ordering::Relaxed);
    pm.slots[2].last_used.store(0, Ordering::Relaxed);
    pm.slots[2].generation.store(7, Ordering::Relaxed);

    // Simulate: thread cached this slot at generation 7
    let cached_gen = pm.slots[2].generation.load(Ordering::Acquire);
    assert_eq!(cached_gen, 7);

    // Reaper runs
    let _to_destroy = pm.reap_idle(10000);

    // Now the cached generation doesn't match → stale!
    let current_gen = pm.slots[2].generation.load(Ordering::Acquire);
    assert_ne!(cached_gen, current_gen, "Generation must change after reap");

    // The connection pointer is gone
    assert!(pm.slots[2].conn.load(Ordering::Relaxed).is_null());
}

// ═════════════════════════════════════════════════════════════════════════
// Regression: reclaiming a slot must never ask whether its owner thread is
// alive. `pthread_kill(owner, 0)` on an exited thread's pthread_t is a
// segfault on musl, which is what Plex runs on; two core dumps put that frame
// under the pool in sqlite3_prepare_v2 and sqlite3_step.
// ═════════════════════════════════════════════════════════════════════════

#[test]
fn an_unreferenced_slot_idle_past_the_timeout_is_reclaimable() {
    assert!(PoolManager::zombie_reclaimable(false, 301, 300));
}

#[test]
fn a_referenced_slot_is_never_reclaimable_however_idle() {
    assert!(!PoolManager::zombie_reclaimable(true, 100_000, 300));
}

#[test]
fn a_recently_used_slot_is_not_reclaimable() {
    assert!(!PoolManager::zombie_reclaimable(false, 300, 300));
    assert!(!PoolManager::zombie_reclaimable(false, 5, 300));
}

#[test]
fn reclaim_decides_without_probing_the_owner_thread() {
    // The decision takes no thread identity at all, so there is nothing to
    // probe. If someone reintroduces a liveness check, this will not compile
    // against it -- which is the point.
    let decide: fn(bool, i64, i64) -> bool = PoolManager::zombie_reclaimable;
    assert!(decide(false, 301, 300));
}
