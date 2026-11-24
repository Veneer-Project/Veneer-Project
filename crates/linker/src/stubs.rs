//! Tiny executable thunks used for unresolved function imports, so a missing
//! symbol only fails (loudly, with its name) if the guest actually calls it.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_arch = "x86_64")]
const THUNK_SIZE: usize = 24;
#[cfg(target_arch = "aarch64")]
const THUNK_SIZE: usize = 32;
/// A multiple of every page size in use (4, 16 and 64 KiB).
const ARENA_SIZE: usize = 256 * 1024;

/// Machine code that calls `handler(name)`: loads the two constants and
/// jumps (tail call), so the handler's return value is the thunk's.
#[cfg(target_arch = "x86_64")]
fn thunk_code(name: u64, handler: u64) -> Vec<u8> {
    let mut code = Vec::with_capacity(THUNK_SIZE);
    code.extend_from_slice(&[0x48, 0xbf]); // mov rdi, imm64
    code.extend_from_slice(&name.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xb8]); // mov rax, imm64
    code.extend_from_slice(&handler.to_le_bytes());
    code.extend_from_slice(&[0xff, 0xe0]); // jmp rax
    code
}

#[cfg(target_arch = "aarch64")]
fn thunk_code(name: u64, handler: u64) -> Vec<u8> {
    let mut code = Vec::with_capacity(THUNK_SIZE);
    for insn in [
        0x5800_0080u32, // ldr x0, #16   (name)
        0x5800_00b0,    // ldr x16, #20  (handler, at offset 24)
        0xd61f_0200,    // br x16
        0xd503_201f,    // nop (aligns the literals)
    ] {
        code.extend_from_slice(&insn.to_le_bytes());
    }
    code.extend_from_slice(&name.to_le_bytes());
    code.extend_from_slice(&handler.to_le_bytes());
    code
}

/// x86-64 keeps instruction and data caches coherent by itself.
#[cfg(target_arch = "x86_64")]
unsafe fn sync_icache(_write: usize, _exec: usize, _len: usize) {}

/// AArch64 does not: clean the data cache lines (through the view that was
/// written) to the point of unification, then invalidate the instruction
/// cache lines of the view that runs. Linux lets user space do both.
#[cfg(target_arch = "aarch64")]
unsafe fn sync_icache(write: usize, exec: usize, len: usize) {
    use std::arch::asm;
    let ctr: u64;
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    let dline = 4usize << ((ctr >> 16) & 0xf);
    let iline = 4usize << (ctr & 0xf);
    unsafe {
        let mut a = write & !(dline - 1);
        while a < write + len {
            asm!("dc cvau, {}", in(reg) a, options(nostack));
            a += dline;
        }
        asm!("dsb ish", options(nostack));
        let mut a = exec & !(iline - 1);
        while a < exec + len {
            asm!("ic ivau, {}", in(reg) a, options(nostack));
            a += iline;
        }
        asm!("dsb ish", "isb", options(nostack));
    }
}

/// Thunk memory, mapped twice so that no mapping is ever both writable
/// and executable (W^X): thunks are written through `write` and run from
/// `exec`, two views of the same pages. This also means a page never has
/// to lose its execute permission while another thread may be running it.
///
/// The `write` view is kept inaccessible (`---`) and only opened, under the
/// lock, for the few instructions that copy a new thunk in. So outside
/// [`make`] there is no writable address for the thunk code at all.
struct Arena {
    write: usize,
    exec: usize,
    used: usize,
}

static ARENA: Mutex<Arena> = Mutex::new(Arena { write: 0, exec: 0, used: 0 });

/// When set, calls to unresolved symbols return 0 instead of aborting.
pub static LENIENT: AtomicBool = AtomicBool::new(false);

extern "C" fn unresolved_called(name: *const c_char) -> usize {
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    if LENIENT.load(Ordering::Relaxed) {
        log::error!("guest called unresolved symbol `{name}` (returning 0)");
        return 0;
    }
    eprintln!("veneer: guest called unresolved symbol `{name}`; aborting");
    std::process::abort();
}

/// Create a thunk that reports `name` when called.
pub fn make(name: &str) -> usize {
    let mut arena = ARENA.lock().unwrap();
    if arena.exec == 0 || arena.used + THUNK_SIZE > ARENA_SIZE {
        // The old arena stays mapped: its thunks may still be called.
        let (write, exec) = map_arena().expect("cannot allocate stub arena");
        *arena = Arena { write, exec, used: 0 };
    }
    let offset = arena.used;
    arena.used += THUNK_SIZE;

    let cname = CString::new(name).unwrap().into_raw() as u64;
    let handler = unresolved_called as usize as u64;
    let code = thunk_code(cname, handler);
    let write = arena.write as *mut c_void;
    unsafe {
        if libc::mprotect(write, ARENA_SIZE, libc::PROT_READ | libc::PROT_WRITE) != 0 {
            panic!("cannot open stub arena: {}", std::io::Error::last_os_error());
        }
        std::ptr::copy_nonoverlapping(code.as_ptr(), (arena.write + offset) as *mut u8, code.len());
        sync_icache(arena.write + offset, arena.exec + offset, code.len());
        libc::mprotect(write, ARENA_SIZE, libc::PROT_NONE);
    }
    arena.exec + offset
}

/// Map `ARENA_SIZE` bytes of shared memory twice: a write view (left
/// inaccessible until [`make`] opens it) and a read+exec view.
fn map_arena() -> Option<(usize, usize)> {
    unsafe {
        // Ask for an executable memfd explicitly: with `vm.memfd_noexec = 1`
        // a plain memfd is sealed non-executable and the exec view would fail.
        // Kernels before 6.3 do not know MFD_EXEC and answer EINVAL.
        let mut fd = libc::memfd_create(c"lucid-stubs".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_EXEC);
        if fd < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
            fd = libc::memfd_create(c"lucid-stubs".as_ptr(), libc::MFD_CLOEXEC);
        }
        if fd < 0 {
            log::error!("stub arena: memfd_create: {}", std::io::Error::last_os_error());
            return None;
        }
        if libc::ftruncate(fd, ARENA_SIZE as libc::off_t) != 0 {
            libc::close(fd);
            return None;
        }
        let map =
            |prot| libc::mmap(std::ptr::null_mut(), ARENA_SIZE, prot, libc::MAP_SHARED, fd, 0);
        let write = map(libc::PROT_NONE);
        let exec = map(libc::PROT_READ | libc::PROT_EXEC);
        let err = std::io::Error::last_os_error();
        libc::close(fd);
        if write == libc::MAP_FAILED || exec == libc::MAP_FAILED {
            log::error!("stub arena: mmap: {err}");
            for view in [write, exec] {
                if view != libc::MAP_FAILED {
                    libc::munmap(view, ARENA_SIZE);
                }
            }
            return None;
        }
        Some((write as usize, exec as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Protection flags (`r-xs` etc.) of the mapping containing `addr`.
    fn perms(addr: usize) -> String {
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        for line in maps.lines() {
            let mut parts = line.split_whitespace();
            let (lo, hi) = parts.next().unwrap().split_once('-').unwrap();
            let lo = usize::from_str_radix(lo, 16).unwrap();
            let hi = usize::from_str_radix(hi, 16).unwrap();
            if addr >= lo && addr < hi {
                return parts.next().unwrap().to_string();
            }
        }
        String::new()
    }

    #[test]
    fn thunks_run_and_memory_is_never_writable_and_executable() {
        LENIENT.store(true, Ordering::Relaxed);
        let first = make("first_missing_symbol");
        let second = make("second_missing_symbol");
        assert_ne!(first, second);
        // The thunk is callable and reports through the handler (which
        // returns 0 in lenient mode instead of aborting).
        let f: extern "C" fn() -> usize = unsafe { std::mem::transmute(second) };
        assert_eq!(f(), 0);

        // Executable view: not writable. Write view: closed between thunks.
        let arena = ARENA.lock().unwrap();
        let (x, w) = (perms(arena.exec), perms(arena.write));
        assert!(x.starts_with("r-x"), "exec view is {x}");
        assert!(w.starts_with("---"), "write view is {w}");
        // And nothing in the process is mapped rwx by us.
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        assert!(!maps.lines().any(|l| l.contains("lucid-stubs") && l.contains("rwx")));
    }
}
