//! A last-resort fatal signal handler that prints where the guest crashed.

use std::ffi::c_void;

/// Program counter, stack pointer and frame pointer at the fault.
#[cfg(target_arch = "x86_64")]
unsafe fn registers(uc: *const libc::ucontext_t) -> (usize, usize, usize) {
    let g = unsafe { &(*uc).uc_mcontext.gregs };
    (
        g[libc::REG_RIP as usize] as usize,
        g[libc::REG_RSP as usize] as usize,
        g[libc::REG_RBP as usize] as usize,
    )
}

#[cfg(target_arch = "aarch64")]
unsafe fn registers(uc: *const libc::ucontext_t) -> (usize, usize, usize) {
    let m = unsafe { &(*uc).uc_mcontext };
    (m.pc as usize, m.sp as usize, m.regs[29] as usize)
}

unsafe extern "C" fn on_fatal(sig: i32, info: *mut libc::siginfo_t, ctx: *mut c_void) {
    let (pc, sp, fp) = unsafe { registers(ctx as *const libc::ucontext_t) };
    let addr = unsafe { (*info).si_addr() } as usize;
    eprintln!("\nveneer: fatal signal {sig} (fault address {addr:#x})");
    eprintln!("  pc {}", lucid_linker::describe_addr(pc));
    eprintln!("  sp {sp:#x} fp {fp:#x}");
    if let Ok(maps) = std::fs::read_to_string("/proc/self/maps") {
        for l in maps.lines() {
            let mut it = l.split(['-', ' ']);
            let lo = usize::from_str_radix(it.next().unwrap_or("0"), 16).unwrap_or(0);
            let hi = usize::from_str_radix(it.next().unwrap_or("0"), 16).unwrap_or(0);
            if l.ends_with("[stack]") || (addr >= lo.saturating_sub(1 << 20) && addr < hi) {
                eprintln!("  map {l}");
            }
        }
    }
    // Without unwind info at hand, list stack words that point into guest
    // code: these are (mostly) return addresses, innermost first.
    eprintln!("  guest code addresses on the stack:");
    let mut shown = 0;
    let mut last = 0usize;
    let mut repeats = 0usize;
    let page = match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        n if n > 0 => n as usize,
        _ => 4096,
    };
    for i in 0..4096usize {
        let p = (sp + i * 8) as *const usize;
        // Stay within the page range we know is mapped: stop at a page
        // boundary we cannot verify.
        let mut vec = 0u8;
        if (p as usize) % page == 0
            && unsafe { libc::mincore((p as usize) as *mut c_void, page, &mut vec) } != 0
        {
            break;
        }
        let v = unsafe { p.read_volatile() };
        if lucid_linker::image_for_addr(v).is_some() {
            if v == last {
                repeats += 1;
                continue;
            }
            last = v;
            eprintln!("    [{:#06x}] {}", i * 8, lucid_linker::describe_addr(v));
            shown += 1;
            if shown >= 40 {
                break;
            }
        }
    }
    if repeats > 0 {
        eprintln!("    ({repeats} repeated entries omitted)");
    }
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Install the handler for the usual crash signals, on an alternate stack
/// so stack overflows on the main thread are reported too.
pub fn install() {
    unsafe {
        let size = 256 * 1024;
        let stack = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        if stack != libc::MAP_FAILED {
            let ss = libc::stack_t {
                ss_sp: stack,
                ss_flags: 0,
                ss_size: size,
            };
            libc::sigaltstack(&ss, std::ptr::null_mut());
        }
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_fatal as usize;
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
        for sig in [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGFPE] {
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}
