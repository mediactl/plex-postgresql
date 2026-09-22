use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use crate::db_interpose_common::CXA_DEMANGLE_FN;
use crate::db_interpose_conn_utils::cstr_to_lossy_or;

const MAX_FRAMES: usize = 64;
const MAX_DISPLAY: usize = 25;
const MAX_FUNC_LEN: usize = 70;

#[repr(C)]
#[derive(Copy, Clone)]
struct ResolvedSymbol {
    func: [c_char; 256],
    lib: [c_char; 256],
    offset: usize,
    has_offset: bool,
}

impl Default for ResolvedSymbol {
    fn default() -> Self {
        ResolvedSymbol {
            func: [0; 256],
            lib: [0; 256],
            offset: 0,
            has_offset: false,
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    fn backtrace(frames: *mut *mut c_void, size: c_int) -> c_int;
    fn backtrace_symbols(frames: *const *mut c_void, size: c_int) -> *mut *mut c_char;
}

#[cfg(target_os = "linux")]
#[cfg(target_arch = "x86_64")]
unsafe fn current_frame_ptr() -> *mut *mut c_void {
    let fp: *mut *mut c_void;
    core::arch::asm!("mov {}, rbp", out(reg) fp);
    fp
}

#[cfg(target_os = "linux")]
#[cfg(target_arch = "aarch64")]
unsafe fn current_frame_ptr() -> *mut *mut c_void {
    let fp: *mut *mut c_void;
    core::arch::asm!("mov {}, x29", out(reg) fp);
    fp
}

#[cfg(target_os = "linux")]
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
unsafe fn current_frame_ptr() -> *mut *mut c_void {
    ptr::null_mut()
}

#[cfg(target_os = "macos")]
unsafe fn collect_frames(frames: &mut [*mut c_void]) -> usize {
    backtrace(frames.as_mut_ptr(), frames.len() as c_int) as usize
}

#[cfg(target_os = "linux")]
mod unwind_ffi {
    use std::os::raw::{c_int, c_void};

    pub const URC_NO_REASON: c_int = 0;
    pub const URC_END_OF_STACK: c_int = 5;

    extern "C" {
        pub fn _Unwind_Backtrace(
            trace: extern "C" fn(*mut c_void, *mut c_void) -> c_int,
            trace_argument: *mut c_void,
        ) -> c_int;
        pub fn _Unwind_GetIP(ctx: *mut c_void) -> usize;
    }
}

#[cfg(target_os = "linux")]
struct UnwindCollector {
    frames: *mut *mut c_void,
    capacity: usize,
    depth: usize,
}

#[cfg(target_os = "linux")]
extern "C" fn unwind_collect(ctx: *mut c_void, arg: *mut c_void) -> c_int {
    let collector = unsafe { &mut *(arg as *mut UnwindCollector) };
    if collector.depth >= collector.capacity {
        return unwind_ffi::URC_END_OF_STACK;
    }
    let ip = unsafe { unwind_ffi::_Unwind_GetIP(ctx) };
    if ip == 0 {
        return unwind_ffi::URC_END_OF_STACK;
    }
    unsafe { *collector.frames.add(collector.depth) = ip as *mut c_void };
    collector.depth += 1;
    unwind_ffi::URC_NO_REASON
}

/// Walk the stack with the DWARF unwinder, falling back to the frame-pointer
/// chain.
///
/// The frame-pointer walk reports nothing inside Plex, which is built without
/// them. `_Unwind_Backtrace` reads the same CFI the exception machinery does,
/// so it works whatever the register allocator did -- but only because this
/// library now links libgcc statically and keeps its symbols local. While
/// libgcc_s was a shared dependency, `_Unwind_Backtrace` came from libgcc and
/// `_Unwind_GetIP` from the copy inside Plex's libc++, and handing one
/// unwinder's context to the other segfaults. See the link step in
/// scripts/docker-build-shim.sh.
#[cfg(target_os = "linux")]
unsafe fn collect_frames(frames: &mut [*mut c_void]) -> usize {
    let mut collector = UnwindCollector {
        frames: frames.as_mut_ptr(),
        capacity: frames.len(),
        depth: 0,
    };
    unwind_ffi::_Unwind_Backtrace(
        unwind_collect,
        &mut collector as *mut UnwindCollector as *mut c_void,
    );
    if collector.depth > 0 {
        return collector.depth;
    }
    collect_frames_fp(frames)
}

#[cfg(target_os = "linux")]
unsafe fn collect_frames_fp(frames: &mut [*mut c_void]) -> usize {
    let mut depth = 0usize;
    let mut fp = current_frame_ptr();
    let mut iterations = 0usize;

    while !fp.is_null() && depth < frames.len() && iterations < 100 {
        iterations += 1;

        let fp_addr = fp as usize;
        if !(0x1000..=0x0000_ffff_ffff_ffff).contains(&fp_addr) {
            break;
        }

        let ret_addr = *fp.add(1) as *mut c_void;
        if ret_addr.is_null() || (ret_addr as usize) < 0x1000 {
            break;
        }

        frames[depth] = ret_addr;
        depth += 1;

        let next_fp = *fp as *mut *mut c_void;
        if next_fp.is_null() || next_fp <= fp {
            break;
        }
        if (next_fp as usize).saturating_sub(fp_addr) > 0x100000 {
            break;
        }

        fp = next_fp;
    }

    depth
}

#[cfg(target_os = "macos")]
unsafe fn resolve_symbols(frames: &[*mut c_void], out: &mut [ResolvedSymbol]) {
    let symbols = backtrace_symbols(frames.as_ptr(), frames.len() as c_int);

    for (i, sym) in out.iter_mut().enumerate() {
        sym.func[0] = 0;
        sym.lib[0] = 0;
        sym.offset = 0;
        sym.has_offset = false;

        if symbols.is_null() {
            continue;
        }

        let symbol_ptr = *symbols.add(i);
        if symbol_ptr.is_null() {
            continue;
        }

        let bytes = CStr::from_ptr(symbol_ptr).to_bytes();
        let plus_pos = bytes.iter().rposition(|&b| b == b'+');
        if let Some(plus) = plus_pos {
            let before_plus = &bytes[..plus];
            let start = before_plus
                .iter()
                .rposition(|&b| b == b' ')
                .map(|p| p + 1)
                .unwrap_or(0);
            let mut end = before_plus.len();
            while end > start && before_plus[end - 1] == b' ' {
                end -= 1;
            }
            if end > start {
                let mangled = &before_plus[start..end];
                let demangle_opt = CXA_DEMANGLE_FN.get().copied().flatten();
                if let Some(demangle) = demangle_opt {
                    let mut status: c_int = 0;
                    let demangled = demangle(
                        mangled.as_ptr() as *const c_char,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        &mut status,
                    );
                    if !demangled.is_null() && status == 0 {
                        libc::strncpy(sym.func.as_mut_ptr(), demangled, sym.func.len() - 1);
                        libc::free(demangled as *mut c_void);
                    } else {
                        libc::strncpy(sym.func.as_mut_ptr(), symbol_ptr, sym.func.len() - 1);
                    }
                } else {
                    libc::strncpy(sym.func.as_mut_ptr(), symbol_ptr, sym.func.len() - 1);
                }
            }
        }

        if sym.func[0] == 0 {
            libc::strncpy(sym.func.as_mut_ptr(), symbol_ptr, sym.func.len() - 1);
        }
    }

    if !symbols.is_null() {
        libc::free(symbols as *mut c_void);
    }
}

#[cfg(target_os = "linux")]
#[derive(Copy, Clone)]
struct MapEntry {
    start: usize,
    end: usize,
    path: [c_char; 256],
}

#[cfg(target_os = "linux")]
impl Default for MapEntry {
    fn default() -> Self {
        MapEntry {
            start: 0,
            end: 0,
            path: [0; 256],
        }
    }
}

#[cfg(target_os = "linux")]
const MAX_MAPS_ENTRIES: usize = 256;

#[cfg(target_os = "linux")]
fn load_memory_map() -> Vec<MapEntry> {
    let mut entries = Vec::new();
    let Ok(content) = std::fs::read_to_string("/proc/self/maps") else {
        return entries;
    };

    for line in content.lines() {
        if entries.len() >= MAX_MAPS_ENTRIES {
            break;
        }

        let mut parts = line.split_whitespace();
        let Some(range) = parts.next() else {
            continue;
        };
        let Some((start_str, end_str)) = range.split_once('-') else {
            continue;
        };
        let Ok(start) = usize::from_str_radix(start_str, 16) else {
            continue;
        };
        let Ok(end) = usize::from_str_radix(end_str, 16) else {
            continue;
        };

        let path = parts.last().unwrap_or("[anonymous]");
        let mut entry = MapEntry {
            start,
            end,
            ..MapEntry::default()
        };

        let bytes = path.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() && i < entry.path.len() - 1 {
            entry.path[i] = bytes[i] as c_char;
            i += 1;
        }
        entry.path[i] = 0;

        entries.push(entry);
    }

    entries
}

#[cfg(target_os = "linux")]
fn find_lib_for_addr(entries: &[MapEntry], addr: usize) -> *const c_char {
    for entry in entries {
        if addr >= entry.start && addr < entry.end {
            let path_ptr = entry.path.as_ptr();
            let base = unsafe { libc::strrchr(path_ptr, b'/' as c_int) };
            return if base.is_null() {
                path_ptr
            } else {
                unsafe { base.add(1) }
            };
        }
    }
    b"[unknown]\0".as_ptr() as *const c_char
}

#[cfg(target_os = "linux")]
unsafe fn resolve_symbols(frames: &[*mut c_void], out: &mut [ResolvedSymbol]) {
    let memory_map = load_memory_map();

    for (i, sym) in out.iter_mut().enumerate() {
        sym.func[0] = 0;
        sym.lib[0] = 0;

        let mut info = libc::Dl_info {
            dli_fname: ptr::null(),
            dli_fbase: ptr::null_mut(),
            dli_sname: ptr::null(),
            dli_saddr: ptr::null_mut(),
        };

        if libc::dladdr(frames[i], &mut info) != 0 {
            if !info.dli_fname.is_null() {
                let base = libc::strrchr(info.dli_fname, b'/' as c_int);
                let lib_name = if base.is_null() {
                    info.dli_fname
                } else {
                    base.add(1)
                };
                libc::strncpy(sym.lib.as_mut_ptr(), lib_name, sym.lib.len() - 1);
            }

            if !info.dli_sname.is_null() {
                let demangle_opt = CXA_DEMANGLE_FN.get().copied().flatten();
                if let Some(demangle) = demangle_opt {
                    let mut status: c_int = 0;
                    let demangled = demangle(
                        info.dli_sname,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        &mut status,
                    );
                    if !demangled.is_null() && status == 0 {
                        libc::strncpy(sym.func.as_mut_ptr(), demangled, sym.func.len() - 1);
                        libc::free(demangled as *mut c_void);
                    } else {
                        libc::strncpy(sym.func.as_mut_ptr(), info.dli_sname, sym.func.len() - 1);
                    }
                } else {
                    libc::strncpy(sym.func.as_mut_ptr(), info.dli_sname, sym.func.len() - 1);
                }
            }

            if !info.dli_fbase.is_null() {
                let addr = frames[i] as usize;
                let base = info.dli_fbase as usize;
                if addr >= base {
                    sym.offset = addr - base;
                    sym.has_offset = true;
                }
            }
        }

        if sym.lib[0] == 0 {
            let lib = find_lib_for_addr(&memory_map, frames[i] as usize);
            libc::strncpy(sym.lib.as_mut_ptr(), lib, sym.lib.len() - 1);
        }

        if sym.func[0] == 0 {
            let _ = libc::snprintf(
                sym.func.as_mut_ptr(),
                sym.func.len(),
                b"[%p]\0".as_ptr() as *const c_char,
                frames[i],
            );
        }
    }
}

fn write_stderr(msg: &str) {
    unsafe {
        let _ = libc::write(
            libc::STDERR_FILENO,
            msg.as_ptr() as *const c_void,
            msg.len(),
        );
    }
}

fn log_error(msg: &str) {
    if let Ok(cs) = std::ffi::CString::new(msg) {
        crate::pg_logging::rust_logging_write(0, cs.as_ptr());
    }
}

#[no_mangle]
pub extern "C" fn platform_print_backtrace(reason: *const c_char, skip_frames: c_int) {
    let mut frames: [*mut c_void; MAX_FRAMES] = [ptr::null_mut(); MAX_FRAMES];
    let depth = unsafe { collect_frames(&mut frames) };

    if depth == 0 {
        write_stderr("\n  [Stack trace unavailable]\n");
        return;
    }

    let reason_str = cstr_to_lossy_or(reason, "Unknown");

    write_stderr("\n");
    write_stderr(
        "╔══════════════════════════════════════════════════════════════════════════════╗\n",
    );
    write_stderr(&format!("║ BACKTRACE: {:<67} ║\n", reason_str));
    write_stderr(
        "╠══════════════════════════════════════════════════════════════════════════════╣\n",
    );
    log_error(&format!("=== BACKTRACE ({}) ===", reason_str));

    let mut symbols = vec![ResolvedSymbol::default(); depth];
    unsafe {
        resolve_symbols(&frames[..depth], &mut symbols);
    }

    let mut printed = 0usize;
    let start = skip_frames.max(0) as usize;
    for i in start..depth {
        if printed >= MAX_DISPLAY {
            break;
        }

        let func = unsafe { CStr::from_ptr(symbols[i].func.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let lib = unsafe { CStr::from_ptr(symbols[i].lib.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let mut func_display = func;
        if func_display.len() > MAX_FUNC_LEN {
            func_display.truncate(MAX_FUNC_LEN - 3);
            func_display.push_str("...");
        }

        let lib_display = if lib.is_empty() { "?".to_string() } else { lib };
        let lib_with_offset = if symbols[i].has_offset {
            format!("{}+0x{:x}", lib_display, symbols[i].offset)
        } else {
            lib_display
        };
        let mut line = format!("[{:2}] {} ({})", printed, func_display, lib_with_offset);
        if line.len() > 78 {
            let prefix = format!("[{:2}] ", printed);
            let suffix = format!(" ({})", lib_with_offset);
            let max_func = 78usize.saturating_sub(prefix.len() + suffix.len());
            if max_func >= 4 && func_display.len() > max_func {
                func_display.truncate(max_func - 3);
                func_display.push_str("...");
                line = format!("{}{}{}", prefix, func_display, suffix);
            } else if max_func < 4 {
                line = format!("[{:2}] {}", printed, func_display);
            }
        }
        write_stderr(&format!("║ {:<78} ║\n", line));
        log_error(&format!("  {}", line));
        printed += 1;
    }

    if depth > start + MAX_DISPLAY {
        write_stderr(&format!(
            "║ ... and {} more frames                                                         ║\n",
            depth - start - MAX_DISPLAY
        ));
    }
    write_stderr(
        "╚══════════════════════════════════════════════════════════════════════════════╝\n",
    );
}
