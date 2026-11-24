//! Threading primitives whose layout differs from glibc: semaphores and
//! `setjmp`/`longjmp`. pthread mutexes, condition variables, rwlocks and
//! attributes have the same size as glibc's on x86-64 and are zero-initialised
//! in both, so those are forwarded untouched; on AArch64 the mutex and thread
//! attribute initialisers need care (see `aarch64_pthread`).

use std::arch::naked_asm;
use std::collections::HashMap;
use std::ffi::c_int;
use std::sync::atomic::{AtomicU32, Ordering};

/// bionic's `sem_t` is 16 bytes with the counter in the first word; glibc's
/// is 32 bytes, so the host implementation cannot be used on guest storage.
unsafe fn sem(s: *mut u32) -> &'static AtomicU32 {
    unsafe { &*(s as *const AtomicU32) }
}

unsafe extern "C" fn sem_init(s: *mut u32, _pshared: c_int, value: u32) -> c_int {
    unsafe { sem(s) }.store(value, Ordering::SeqCst);
    0
}

unsafe extern "C" fn sem_destroy(_s: *mut u32) -> c_int {
    0
}

unsafe extern "C" fn sem_post(s: *mut u32) -> c_int {
    unsafe { sem(s) }.fetch_add(1, Ordering::SeqCst);
    unsafe { libc::syscall(libc::SYS_futex, s, libc::FUTEX_WAKE, 1, 0usize, 0usize, 0usize) };
    0
}

unsafe extern "C" fn sem_trywait(s: *mut u32) -> c_int {
    let a = unsafe { sem(s) };
    let mut cur = a.load(Ordering::SeqCst);
    while cur > 0 {
        match a.compare_exchange_weak(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return 0,
            Err(v) => cur = v,
        }
    }
    unsafe { *libc::__errno_location() = libc::EAGAIN };
    -1
}

unsafe extern "C" fn sem_wait(s: *mut u32) -> c_int {
    loop {
        if unsafe { sem_trywait(s) } == 0 {
            return 0;
        }
        unsafe {
            libc::syscall(libc::SYS_futex, s, libc::FUTEX_WAIT, 0u32, 0usize, 0usize, 0usize)
        };
    }
}

unsafe extern "C" fn sem_getvalue(s: *mut u32, out: *mut c_int) -> c_int {
    unsafe { *out = sem(s).load(Ordering::SeqCst) as c_int };
    0
}

/// bionic's `jmp_buf` is 11 words, glibc's is 25: use our own register save.
/// Layout: rbx, rbp, r12, r13, r14, r15, rsp, rip.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
unsafe extern "C" fn setjmp(_env: *mut u64) -> c_int {
    naked_asm!(
        "mov [rdi], rbx",
        "mov [rdi + 8], rbp",
        "mov [rdi + 16], r12",
        "mov [rdi + 24], r13",
        "mov [rdi + 32], r14",
        "mov [rdi + 40], r15",
        "lea rdx, [rsp + 8]",
        "mov [rdi + 48], rdx",
        "mov rdx, [rsp]",
        "mov [rdi + 56], rdx",
        "xor eax, eax",
        "ret",
    )
}

#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
unsafe extern "C" fn sigsetjmp(_env: *mut u64, _save: c_int) -> c_int {
    naked_asm!(
        "mov [rdi], rbx",
        "mov [rdi + 8], rbp",
        "mov [rdi + 16], r12",
        "mov [rdi + 24], r13",
        "mov [rdi + 32], r14",
        "mov [rdi + 40], r15",
        "lea rdx, [rsp + 8]",
        "mov [rdi + 48], rdx",
        "mov rdx, [rsp]",
        "mov [rdi + 56], rdx",
        "xor eax, eax",
        "ret",
    )
}

#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
unsafe extern "C" fn longjmp(_env: *mut u64, _val: c_int) -> ! {
    naked_asm!(
        "mov eax, esi",
        "test eax, eax",
        "jnz 2f",
        "inc eax",
        "2:",
        "mov rbx, [rdi]",
        "mov rbp, [rdi + 8]",
        "mov r12, [rdi + 16]",
        "mov r13, [rdi + 24]",
        "mov r14, [rdi + 32]",
        "mov r15, [rdi + 40]",
        "mov rsp, [rdi + 48]",
        "jmp [rdi + 56]",
    )
}

/// bionic's AArch64 `jmp_buf` is 32 words. Saved here (21 words): the
/// callee-saved x19-x28, the frame pointer x29, the return address x30, sp,
/// and the callee-saved floating-point registers d8-d15.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn setjmp(_env: *mut u64) -> c_int {
    naked_asm!(
        "stp x19, x20, [x0, #0]",
        "stp x21, x22, [x0, #16]",
        "stp x23, x24, [x0, #32]",
        "stp x25, x26, [x0, #48]",
        "stp x27, x28, [x0, #64]",
        "stp x29, x30, [x0, #80]",
        "mov x2, sp",
        "str x2, [x0, #96]",
        "stp d8, d9, [x0, #104]",
        "stp d10, d11, [x0, #120]",
        "stp d12, d13, [x0, #136]",
        "stp d14, d15, [x0, #152]",
        "mov w0, #0",
        "ret",
    )
}

/// Like x86-64, the signal mask is not saved (the engine does not rely on it).
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn sigsetjmp(_env: *mut u64, _save: c_int) -> c_int {
    naked_asm!("b {}", sym setjmp)
}

#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn longjmp(_env: *mut u64, _val: c_int) -> ! {
    naked_asm!(
        "ldp x19, x20, [x0, #0]",
        "ldp x21, x22, [x0, #16]",
        "ldp x23, x24, [x0, #32]",
        "ldp x25, x26, [x0, #48]",
        "ldp x27, x28, [x0, #64]",
        "ldp x29, x30, [x0, #80]",
        "ldr x2, [x0, #96]",
        "mov sp, x2",
        "ldp d8, d9, [x0, #104]",
        "ldp d10, d11, [x0, #120]",
        "ldp d12, d13, [x0, #136]",
        "ldp d14, d15, [x0, #152]",
        // longjmp(env, 0) returns 1.
        "cmp w1, #0",
        "csinc w0, w1, wzr, ne",
        "ret",
    )
}

// --- AArch64: pthread objects that are smaller in bionic -----------------------
//
// On AArch64 glibc's `pthread_mutex_t` is 48 bytes and `pthread_attr_t` 64,
// bionic's 40 and 56. glibc's state fits in bionic's size (the mutex uses
// 40 bytes, the attributes 56); only the initialisers clear the full glibc
// size, which would overwrite the 8 bytes after the guest's object. They
// run on a scratch copy here and only bionic's size is copied back.
// (On x86-64 both libcs use the same sizes, so nothing is needed there.)

#[cfg(target_arch = "aarch64")]
mod aarch64_pthread {
    use std::ffi::c_int;

    const BIONIC_MUTEX: usize = 40;
    const BIONIC_ATTR: usize = 56;

    #[repr(C, align(16))]
    struct Scratch([u8; 64]);

    const _: () = assert!(size_of::<libc::pthread_mutex_t>() <= size_of::<Scratch>());
    const _: () = assert!(size_of::<libc::pthread_attr_t>() <= size_of::<Scratch>());

    pub unsafe extern "C" fn mutex_init(
        m: *mut libc::pthread_mutex_t,
        attr: *const libc::pthread_mutexattr_t,
    ) -> c_int {
        let mut s = Scratch([0; 64]);
        let r = unsafe { libc::pthread_mutex_init(s.0.as_mut_ptr().cast(), attr) };
        unsafe { std::ptr::copy_nonoverlapping(s.0.as_ptr(), m.cast(), BIONIC_MUTEX) };
        r
    }

    pub unsafe extern "C" fn attr_init(a: *mut libc::pthread_attr_t) -> c_int {
        let mut s = Scratch([0; 64]);
        let r = unsafe { libc::pthread_attr_init(s.0.as_mut_ptr().cast()) };
        unsafe { std::ptr::copy_nonoverlapping(s.0.as_ptr(), a.cast(), BIONIC_ATTR) };
        r
    }

    pub unsafe extern "C" fn getattr_np(t: libc::pthread_t, a: *mut libc::pthread_attr_t) -> c_int {
        let mut s = Scratch([0; 64]);
        let r = unsafe { libc::pthread_getattr_np(t, s.0.as_mut_ptr().cast()) };
        unsafe { std::ptr::copy_nonoverlapping(s.0.as_ptr(), a.cast(), BIONIC_ATTR) };
        r
    }
}

/// `pthread_getattr_np` as the guest should get it; logged, as the engine's
/// stack probing has needed debugging before.
pub(crate) unsafe extern "C" fn getattr_np(t: libc::pthread_t, a: *mut libc::pthread_attr_t) -> c_int {
    #[cfg(target_arch = "aarch64")]
    let r = unsafe { aarch64_pthread::getattr_np(t, a) };
    #[cfg(not(target_arch = "aarch64"))]
    let r = unsafe { libc::pthread_getattr_np(t, a) };
    log::debug!("pthread_getattr_np({t:#x}) -> {r}");
    r
}

pub fn symbols(m: &mut HashMap<&'static str, usize>) {
    m.insert("sem_init", sem_init as usize);
    m.insert("sem_destroy", sem_destroy as usize);
    m.insert("sem_post", sem_post as usize);
    m.insert("sem_wait", sem_wait as usize);
    m.insert("sem_trywait", sem_trywait as usize);
    m.insert("sem_getvalue", sem_getvalue as usize);
    m.insert("setjmp", setjmp as usize);
    m.insert("_setjmp", setjmp as usize);
    m.insert("sigsetjmp", sigsetjmp as usize);
    m.insert("longjmp", longjmp as usize);
    m.insert("_longjmp", longjmp as usize);
    m.insert("siglongjmp", longjmp as usize);
    m.insert("pthread_getattr_np", getattr_np as usize);
    #[cfg(target_arch = "aarch64")]
    {
        m.insert("pthread_mutex_init", aarch64_pthread::mutex_init as usize);
        m.insert("pthread_attr_init", aarch64_pthread::attr_init as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// Jumps back into this test's own frame, like C code using setjmp
    /// does; state that must survive the jump lives in statics.
    #[test]
    fn setjmp_returns_twice_and_longjmp_zero_becomes_one() {
        static PASSES: AtomicU32 = AtomicU32::new(0);
        static RESULTS: [AtomicU32; 3] = [AtomicU32::new(99), AtomicU32::new(99), AtomicU32::new(99)];
        let mut env = [0u64; 32];
        let set: unsafe extern "C" fn(*mut u64) -> c_int = std::hint::black_box(setjmp);
        let jump: unsafe extern "C" fn(*mut u64, c_int) -> ! = std::hint::black_box(longjmp);
        let r = unsafe { set(env.as_mut_ptr()) };
        let pass = PASSES.fetch_add(1, Ordering::SeqCst) as usize;
        RESULTS[pass].store(r as u32, Ordering::SeqCst);
        match pass {
            0 => unsafe { jump(env.as_mut_ptr(), 7) },
            1 => unsafe { jump(env.as_mut_ptr(), 0) },
            _ => {}
        }
        let got: Vec<u32> = RESULTS.iter().map(|a| a.load(Ordering::SeqCst)).collect();
        assert_eq!(got, [0, 7, 1]);
    }

    /// The initialisers must not write past bionic's sizes (40 and 56 bytes),
    /// and the objects must still work afterwards.
    #[test]
    fn pthread_initialisers_stay_within_bionic_sizes() {
        #[repr(C, align(16))]
        struct Buf([u8; 80]);
        let resolve = |name| crate::overrides().get(name).copied().or_else(|| crate::host_symbol(name)).unwrap();

        let mut m = Buf([0xa5; 80]);
        let init: unsafe extern "C" fn(*mut u8, *const u8) -> c_int =
            unsafe { std::mem::transmute(resolve("pthread_mutex_init")) };
        assert_eq!(unsafe { init(m.0.as_mut_ptr(), std::ptr::null()) }, 0);
        assert!(m.0[40..].iter().all(|b| *b == 0xa5), "pthread_mutex_init wrote past 40 bytes");
        let p = m.0.as_mut_ptr().cast::<libc::pthread_mutex_t>();
        assert_eq!(unsafe { libc::pthread_mutex_lock(p) }, 0);
        assert_eq!(unsafe { libc::pthread_mutex_unlock(p) }, 0);

        let mut a = Buf([0xa5; 80]);
        let attr_init: unsafe extern "C" fn(*mut u8) -> c_int =
            unsafe { std::mem::transmute(resolve("pthread_attr_init")) };
        assert_eq!(unsafe { attr_init(a.0.as_mut_ptr()) }, 0);
        assert!(a.0[56..].iter().all(|b| *b == 0xa5), "pthread_attr_init wrote past 56 bytes");
        let mut size = 0usize;
        let ap = a.0.as_mut_ptr().cast::<libc::pthread_attr_t>();
        assert_eq!(unsafe { libc::pthread_attr_setstacksize(ap, 1 << 20) }, 0);
        assert_eq!(unsafe { libc::pthread_attr_getstacksize(ap, &mut size) }, 0);
        assert_eq!(size, 1 << 20);

        let mut g = Buf([0xa5; 80]);
        let getattr: unsafe extern "C" fn(libc::pthread_t, *mut u8) -> c_int =
            unsafe { std::mem::transmute(resolve("pthread_getattr_np")) };
        assert_eq!(unsafe { getattr(libc::pthread_self(), g.0.as_mut_ptr()) }, 0);
        assert!(g.0[56..].iter().all(|b| *b == 0xa5), "pthread_getattr_np wrote past 56 bytes");
        unsafe { libc::pthread_attr_destroy(g.0.as_mut_ptr().cast()) };
    }
}
