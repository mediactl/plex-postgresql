use std::mem::size_of;
use std::os::raw::{c_char, c_int, c_long, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

#[repr(C)]
struct TlsState {
    in_interpose_call: c_int,
    prepare_v2_depth: c_int,
    in_resolve_tables: c_int,
    value_type_calls: c_long,
    column_type_calls: c_long,
    last_query: *const c_char,
}

static TLS_INIT: Once = Once::new();
/// Whether `TLS_KEY` holds a key we actually created. `pthread_key_t` is an
/// opaque index and 0 is a perfectly ordinary key -- on glibc it is the one the
/// *first* caller in the process gets -- so the key value cannot double as a
/// success flag. Only the `pthread_key_create` return code says that.
static TLS_KEY_VALID: AtomicBool = AtomicBool::new(false);
static mut TLS_KEY: libc::pthread_key_t = 0;
static mut TLS_FALLBACK: TlsState = TlsState {
    in_interpose_call: 0,
    prepare_v2_depth: 0,
    in_resolve_tables: 0,
    value_type_calls: 0,
    column_type_calls: 0,
    last_query: ptr::null(),
};

#[cfg(target_os = "macos")]
unsafe extern "C" {
    static mut __stderrp: *mut libc::FILE;
}

#[cfg(not(target_os = "macos"))]
unsafe extern "C" {
    static mut stderr: *mut libc::FILE;
}

#[inline]
pub(crate) unsafe fn stderr_ptr() -> *mut libc::FILE {
    #[cfg(target_os = "macos")]
    {
        __stderrp
    }
    #[cfg(not(target_os = "macos"))]
    {
        stderr
    }
}

unsafe extern "C" fn tls_destructor(ptr: *mut c_void) {
    if !ptr.is_null() {
        libc::free(ptr);
    }
}

/// The process-wide TLS key, or `None` if one could not be created.
fn tls_key() -> Option<libc::pthread_key_t> {
    TLS_INIT.call_once(|| unsafe {
        let mut key: libc::pthread_key_t = 0;
        if libc::pthread_key_create(&mut key as *mut _, Some(tls_destructor)) == 0 {
            TLS_KEY = key;
            TLS_KEY_VALID.store(true, Ordering::Release);
        }
    });
    if TLS_KEY_VALID.load(Ordering::Acquire) {
        // SAFETY: `TLS_KEY` is written only inside `call_once`, which has
        // already returned, and only on the path that sets `TLS_KEY_VALID`.
        Some(unsafe { TLS_KEY })
    } else {
        None
    }
}

unsafe fn tls_state() -> *mut TlsState {
    // `TLS_FALLBACK` is shared by every thread, so reaching it turns the
    // per-thread reentrancy guards into process-wide ones. It is a last resort
    // for a failed key or a failed allocation, never the normal path.
    let key = match tls_key() {
        Some(key) => key,
        None => return ptr::addr_of_mut!(TLS_FALLBACK),
    };
    let ptr_val = libc::pthread_getspecific(key) as *mut TlsState;
    if !ptr_val.is_null() {
        return ptr_val;
    }
    let new = libc::calloc(1, size_of::<TlsState>()) as *mut TlsState;
    if new.is_null() {
        return ptr::addr_of_mut!(TLS_FALLBACK);
    }
    libc::pthread_setspecific(key, new as *mut c_void);
    new
}

pub(crate) fn tls_in_interpose_call_ptr() -> *mut c_int {
    unsafe { ptr::addr_of_mut!((*tls_state()).in_interpose_call) }
}

pub(crate) fn tls_prepare_v2_depth_ptr() -> *mut c_int {
    unsafe { ptr::addr_of_mut!((*tls_state()).prepare_v2_depth) }
}

pub(crate) fn tls_in_resolve_tables_ptr() -> *mut c_int {
    unsafe { ptr::addr_of_mut!((*tls_state()).in_resolve_tables) }
}

pub(crate) fn tls_value_type_calls_ptr() -> *mut c_long {
    unsafe { ptr::addr_of_mut!((*tls_state()).value_type_calls) }
}

pub(crate) fn tls_column_type_calls_ptr() -> *mut c_long {
    unsafe { ptr::addr_of_mut!((*tls_state()).column_type_calls) }
}

pub(crate) fn tls_last_query_ptr() -> *mut *const c_char {
    unsafe { ptr::addr_of_mut!((*tls_state()).last_query) }
}
