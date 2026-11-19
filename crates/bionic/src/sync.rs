//! Threading primitives whose layout differs from glibc: semaphores and
//! `setjmp`/`longjmp`. pthread mutexes, condition variables, rwlocks and
//! attributes have the same size as glibc's on x86-64 and are zero-initialised
//! in both, so those are forwarded untouched.

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
}
