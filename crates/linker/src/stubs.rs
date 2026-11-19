//! Tiny executable thunks used for unresolved function imports, so a missing
//! symbol only fails (loudly, with its name) if the guest actually calls it.

use std::ffi::{CStr, CString, c_char};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

const THUNK_SIZE: usize = 24;
const ARENA_SIZE: usize = 256 * 1024;

struct Arena {
    base: usize,
    used: usize,
}

static ARENA: Mutex<Arena> = Mutex::new(Arena { base: 0, used: 0 });

/// When set, calls to unresolved symbols return 0 instead of aborting.
pub static LENIENT: AtomicBool = AtomicBool::new(false);

extern "C" fn unresolved_called(name: *const c_char) -> usize {
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    if LENIENT.load(Ordering::Relaxed) {
        log::error!("guest called unresolved symbol `{name}` (returning 0)");
        return 0;
    }
    eprintln!("lucid: guest called unresolved symbol `{name}`; aborting");
    std::process::abort();
}

/// Create a thunk that reports `name` when called.
pub fn make(name: &str) -> usize {
    let mut arena = ARENA.lock().unwrap();
    if arena.base == 0 || arena.used + THUNK_SIZE > ARENA_SIZE {
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                ARENA_SIZE,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert!(p != libc::MAP_FAILED, "cannot allocate stub arena");
        arena.base = p as usize;
        arena.used = 0;
    }
    let addr = arena.base + arena.used;
    arena.used += THUNK_SIZE;

    let cname = CString::new(name).unwrap().into_raw() as u64;
    let handler = unresolved_called as usize as u64;
    let mut code = Vec::with_capacity(THUNK_SIZE);
    code.extend_from_slice(&[0x48, 0xbf]); // mov rdi, imm64
    code.extend_from_slice(&cname.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xb8]); // mov rax, imm64
    code.extend_from_slice(&handler.to_le_bytes());
    code.extend_from_slice(&[0xff, 0xe0]); // jmp rax
    unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), addr as *mut u8, code.len()) };
    addr
}
