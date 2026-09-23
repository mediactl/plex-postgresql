use std::sync::atomic::Ordering;

use crate::ffi_types::PgConnection;

use super::super::connection_helpers::conn_is_streaming_active_ptr;
use super::super::connection_lifecycle::destroy_pool_connection;
use super::super::SLOT_READY;
use super::shared::AcquireCtx;
use crate::log_info_lazy;

pub(super) fn reclaim_zombies_and_reap(ctx: &AcquireCtx<'_>) {
    let idle_timeout = ctx.pm.idle_timeout_secs.load(Ordering::Relaxed) as i64;

    for i in 0..ctx.pool_size {
        let slot = &ctx.pm.slots[i];
        let state = slot.state.load(Ordering::Acquire);
        if state != SLOT_READY {
            continue;
        }
        // Referenced or recently used settles it; nothing asks whether the
        // owner thread is still alive. Plex opens twenty database handles when
        // it starts and keeps them for the life of the process while the
        // worker threads that used them come and go, so a dead owner means
        // nothing -- and asking musl about one segfaults, because its
        // pthread_t is a pointer into a stack that is no longer mapped. See
        // PoolManager::zombie_reclaimable.
        let last_used = slot.last_used.load(Ordering::Acquire);
        if !super::super::PoolManager::zombie_reclaimable(
            ctx.pm.slot_is_referenced(i),
            ctx.now - last_used,
            idle_timeout,
        ) {
            continue;
        }

        let conn = slot.conn.load(Ordering::Acquire);
        if !conn.is_null() && conn_is_streaming_active_ptr(conn as *mut PgConnection) {
            log_info_lazy!(
                "Pool PHASE 0: slot {} unreferenced but streaming_active, skipping reclaim",
                i
            );
            continue;
        }

        if slot.try_reclaim_zombie() {
            log_info_lazy!(
                "Pool PHASE 0: Freed zombie slot {} (unreferenced, idle {} sec)",
                i,
                ctx.now - last_used
            );
        }
    }

    let last_reap = ctx.pm.last_reap_time.load(Ordering::Relaxed);
    if ctx.now - last_reap < 60 {
        return;
    }
    if ctx
        .pm
        .last_reap_time
        .compare_exchange(last_reap, ctx.now, Ordering::SeqCst, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    log_info_lazy!(
        "Pool reaper: running (last run {} seconds ago)",
        ctx.now - last_reap
    );
    let to_destroy = ctx.pm.reap_idle(ctx.now);
    for (_slot_idx, conn_ptr) in to_destroy {
        destroy_pool_connection(conn_ptr);
    }
}
