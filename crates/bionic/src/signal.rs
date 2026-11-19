//! Signals: bionic's x86-64 `sigset_t` is one word (the kernel's), glibc's is
//! 1024 bits, and `struct sigaction` is laid out differently.

use std::collections::HashMap;
use std::ffi::c_int;

#[repr(C)]
struct BionicSigaction {
    sa_flags: c_int,
    sa_handler: usize,
    sa_mask: u64,
    sa_restorer: usize,
}

unsafe fn to_host_set(mask: u64) -> libc::sigset_t {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe { (&raw mut set as *mut u64).write(mask) };
    set
}

unsafe fn from_host_set(set: &libc::sigset_t) -> u64 {
    unsafe { (set as *const libc::sigset_t as *const u64).read() }
}

unsafe extern "C" fn sigaction(
    sig: c_int,
    act: *const BionicSigaction,
    old: *mut BionicSigaction,
) -> c_int {
    let mut host_old: libc::sigaction = unsafe { std::mem::zeroed() };
    let r = if act.is_null() {
        unsafe { libc::sigaction(sig, std::ptr::null(), &mut host_old) }
    } else {
        let a = unsafe { &*act };
        log::debug!(
            "guest sigaction(sig={sig}, handler={:#x}, flags={:#x})",
            a.sa_handler,
            a.sa_flags
        );
        if std::env::var_os("LUCID_NO_GUEST_SIGNALS").is_some() {
            return 0;
        }
        let mut host: libc::sigaction = unsafe { std::mem::zeroed() };
        host.sa_sigaction = a.sa_handler;
        host.sa_mask = unsafe { to_host_set(a.sa_mask) };
        // Let glibc install its own restorer.
        host.sa_flags = a.sa_flags & !0x0400_0000;
        unsafe { libc::sigaction(sig, &host, &mut host_old) }
    };
    if r == 0 && !old.is_null() {
        unsafe {
            old.write(BionicSigaction {
                sa_flags: host_old.sa_flags,
                sa_handler: host_old.sa_sigaction,
                sa_mask: from_host_set(&host_old.sa_mask),
                sa_restorer: 0,
            })
        };
    }
    r
}

unsafe extern "C" fn sigmask_impl(how: c_int, set: *const u64, old: *mut u64) -> c_int {
    let host_set;
    let set_ptr = if set.is_null() {
        std::ptr::null()
    } else {
        host_set = unsafe { to_host_set(*set) };
        &host_set as *const libc::sigset_t
    };
    let mut host_old: libc::sigset_t = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::pthread_sigmask(how, set_ptr, &mut host_old) };
    if r == 0 && !old.is_null() {
        unsafe { *old = from_host_set(&host_old) };
    }
    r
}

unsafe extern "C" fn sigprocmask(how: c_int, set: *const u64, old: *mut u64) -> c_int {
    let r = unsafe { sigmask_impl(how, set, old) };
    if r != 0 {
        unsafe { *libc::__errno_location() = r };
        return -1;
    }
    0
}

unsafe extern "C" fn sigemptyset(set: *mut u64) -> c_int {
    unsafe { *set = 0 };
    0
}

unsafe extern "C" fn sigfillset(set: *mut u64) -> c_int {
    unsafe { *set = !0 };
    0
}

unsafe extern "C" fn sigaddset(set: *mut u64, sig: c_int) -> c_int {
    unsafe { *set |= 1u64 << (sig - 1) };
    0
}

unsafe extern "C" fn sigdelset(set: *mut u64, sig: c_int) -> c_int {
    unsafe { *set &= !(1u64 << (sig - 1)) };
    0
}

unsafe extern "C" fn sigismember(set: *const u64, sig: c_int) -> c_int {
    unsafe { ((*set >> (sig - 1)) & 1) as c_int }
}

pub fn symbols(m: &mut HashMap<&'static str, usize>) {
    m.insert("sigaction", sigaction as usize);
    m.insert("pthread_sigmask", sigmask_impl as usize);
    m.insert("sigprocmask", sigprocmask as usize);
    m.insert("sigemptyset", sigemptyset as usize);
    m.insert("sigfillset", sigfillset as usize);
    m.insert("sigaddset", sigaddset as usize);
    m.insert("sigdelset", sigdelset as usize);
    m.insert("sigismember", sigismember as usize);
}
