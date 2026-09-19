#[cfg(target_os = "linux")]
use super::fake_values::fake_value_reset_next;
use super::*;
use crate::log_debug_lazy;
use crate::log_info_lazy;

const WORKER_STACK_SIZE: usize = 8 * 1024 * 1024;

extern "C" fn worker_thread_func(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        log_info_lazy!(
            "WORKER: Thread started with {} MB stack",
            WORKER_STACK_SIZE / (1024 * 1024)
        );

        loop {
            let mut worker_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(worker_mutex));

            while worker_request.work_ready == 0 && worker_running != 0 {
                libc::pthread_cond_wait(
                    ptr::addr_of_mut!(worker_cond_request),
                    worker_guard.mutex_ptr(),
                );
            }

            if worker_running == 0 {
                worker_guard.unlock();
                break;
            }

            worker_request.work_ready = 0;

            if worker_request.type_ == WORK_SHUTDOWN {
                worker_request.work_done = 1;
                libc::pthread_cond_signal(ptr::addr_of_mut!(worker_cond_response));
                worker_guard.unlock();
                break;
            }

            if worker_request.type_ == WORK_PREPARE_V2 {
                let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
                let mut tail: *const c_char = ptr::null();
                let rc = crate::db_interpose_prepare::rust_my_sqlite3_prepare_v2_internal(
                    worker_request.db,
                    worker_request.z_sql,
                    worker_request.n_byte,
                    &mut stmt,
                    &mut tail,
                    1,
                );

                worker_request.stmt = stmt;
                worker_request.tail = tail;
                worker_request.result = rc;
            }

            worker_request.work_done = 1;
            libc::pthread_cond_signal(ptr::addr_of_mut!(worker_cond_response));
            worker_guard.unlock();
        }

        log_info("WORKER: Thread exiting");
        ptr::null_mut()
    }
}

/// Start the worker thread if it is not already running.
///
/// Idempotent and safe to call from several threads at once: callers race here
/// whenever a delegation finds no worker, and a second `pthread_create` would
/// leave two workers sharing the single `worker_request` slot while orphaning
/// the first `worker_thread` handle, which cleanup could then never join.
pub fn rust_worker_init() -> c_int {
    unsafe {
        let mut init_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(WORKER_INIT_MUTEX));
        if worker_running != 0 {
            init_guard.unlock();
            return 0;
        }

        let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
        if libc::pthread_attr_init(attr.as_mut_ptr()) != 0 {
            log_error("WORKER: Failed to init thread attributes");
            init_guard.unlock();
            return -1;
        }
        let mut attr = attr.assume_init();

        if libc::pthread_attr_setstacksize(&mut attr as *mut _, WORKER_STACK_SIZE) != 0 {
            log_error("WORKER: Failed to set stack size");
            libc::pthread_attr_destroy(&mut attr as *mut _);
            init_guard.unlock();
            return -1;
        }

        worker_running = 1;
        worker_request = EMPTY_WORKER_REQUEST;

        // Create the worker with every signal blocked. A new thread inherits
        // the creating thread's mask, and a library thread must never be a
        // candidate for the process's asynchronous signals.
        //
        // Plex handles signals on a dedicated thread that sits in sigwait()
        // with them blocked everywhere else. A thread of ours without that
        // mask can be handed SIGCHLD instead -- Plex spawns plug-ins, so it
        // arrives within seconds -- and Plex treats a signal its own thread
        // never saw as fatal:
        //
        //	Received unexpected async signal 17
        //	****** PLEX MEDIA SERVER CRASHED
        //
        // It happens just after the server starts answering requests, which is
        // a cruel place to lose it.
        let mut all_signals: libc::sigset_t = std::mem::zeroed();
        let mut saved: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut all_signals);
        let masked = libc::pthread_sigmask(libc::SIG_SETMASK, &all_signals, &mut saved) == 0;

        let created = libc::pthread_create(
            ptr::addr_of_mut!(worker_thread),
            &attr as *const _,
            worker_thread_func,
            ptr::null_mut(),
        );

        if masked {
            libc::pthread_sigmask(libc::SIG_SETMASK, &saved, ptr::null_mut());
        }

        if created != 0 {
            log_error("WORKER: Failed to create thread");
            worker_running = 0;
            libc::pthread_attr_destroy(&mut attr as *mut _);
            init_guard.unlock();
            return -1;
        }

        libc::pthread_attr_destroy(&mut attr as *mut _);
        init_guard.unlock();
        log_info_lazy!(
            "WORKER: Initialized with {} MB stack",
            WORKER_STACK_SIZE / (1024 * 1024)
        );
    }

    0
}

#[cfg(target_os = "linux")]
pub(crate) unsafe fn fast_mark_fork_child_passthrough() {
    SHIM_PASSTHROUGH_ONLY.store(1, Ordering::Release);
    shim_init_pid = libc::getpid();
    CRASH_LAST_COLUMN_LEN.store(0, Ordering::SeqCst);
    GLOBAL_VALUE_TYPE_CALLS.store(0, Ordering::Relaxed);
    GLOBAL_COLUMN_TYPE_CALLS.store(0, Ordering::Relaxed);
    fake_value_reset_next();
    worker_thread = 0 as libc::pthread_t;
    worker_running = 0;
    worker_request = EMPTY_WORKER_REQUEST;
    rust_reset_exception_tracking();
    rust_reset_symbol_verification();
}

pub fn rust_worker_cleanup() {
    unsafe {
        // Same lock as `rust_worker_init`, so a shutdown and a lazy start
        // cannot overlap: otherwise one could hand `worker_running` back to 1
        // while the other is joining the thread that flag refers to.
        let mut init_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(WORKER_INIT_MUTEX));
        if worker_running == 0 {
            init_guard.unlock();
            return;
        }

        let mut worker_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(worker_mutex));
        worker_request.type_ = WORK_SHUTDOWN;
        worker_request.work_ready = 1;
        worker_running = 0;
        libc::pthread_cond_signal(ptr::addr_of_mut!(worker_cond_request));
        worker_guard.unlock();

        libc::pthread_join(worker_thread, ptr::null_mut());
        init_guard.unlock();
    }

    log_info("WORKER: Cleaned up");
}

pub fn rust_delegate_prepare_to_worker(
    db: *mut sqlite3,
    z_sql: *const c_char,
    n_byte: c_int,
    pp_stmt: *mut *mut sqlite3_stmt,
    pz_tail: *mut *const c_char,
) -> c_int {
    unsafe {
        // Unconditional: `rust_worker_init` returns immediately when a worker
        // is already running. Testing `worker_running` here instead would be a
        // check-then-act on an unsynchronised global, which is how two workers
        // used to end up sharing one request slot.
        if rust_worker_init() != 0 {
            log_error("WORKER: Not running, cannot delegate");
            return SQLITE_ERROR;
        }

        // Held until the answer has been read out of the slot. There is one
        // `worker_request` and one `worker_cond_response` for every caller, so
        // without this a second delegation walks into the middle of the first.
        let mut call_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(WORKER_CALL_MUTEX));

        let preview = crate::db_interpose_conn_utils::cstr_prefix(z_sql, 100, "NULL");
        log_debug_lazy!("WORKER: Delegating query ({})", preview);

        let mut worker_guard = PthreadMutexGuard::lock(ptr::addr_of_mut!(worker_mutex));

        worker_request.type_ = WORK_PREPARE_V2;
        worker_request.db = db;
        worker_request.z_sql = z_sql;
        worker_request.n_byte = n_byte;
        worker_request.stmt = ptr::null_mut();
        worker_request.tail = ptr::null();
        worker_request.result = SQLITE_ERROR;
        worker_request.work_done = 0;
        worker_request.work_ready = 1;

        libc::pthread_cond_signal(ptr::addr_of_mut!(worker_cond_request));

        while worker_request.work_done == 0 {
            libc::pthread_cond_wait(
                ptr::addr_of_mut!(worker_cond_response),
                worker_guard.mutex_ptr(),
            );
        }

        if !pp_stmt.is_null() {
            *pp_stmt = worker_request.stmt;
        }
        if !pz_tail.is_null() {
            *pz_tail = worker_request.tail;
        }
        let result = worker_request.result;

        worker_guard.unlock();
        call_guard.unlock();

        log_debug_lazy!("WORKER: Delegation complete, rc={}", result);
        result
    }
}
