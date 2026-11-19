//! Everything else that needs translating: errno, sysconf, system
//! properties, fortify (`*_chk`) helpers, locales and process control.

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_long, c_void};

use libc::{size_t, ssize_t};

fn set_errno(e: c_int) {
    unsafe { *libc::__errno_location() = e };
}

unsafe extern "C" fn errno_location() -> *mut c_int {
    unsafe { libc::__errno_location() }
}

static mut STACK_CHK_GUARD: usize = 0x5bd1_e995_9d2c_5680;

// --- sysconf ---------------------------------------------------------------

unsafe extern "C" fn sysconf(name: c_int) -> c_long {
    let host = match name {
        0x00 => libc::_SC_ARG_MAX,
        0x05 => libc::_SC_CHILD_MAX,
        0x06 => libc::_SC_CLK_TCK,
        0x09 => libc::_SC_LINE_MAX,
        0x0a => libc::_SC_NGROUPS_MAX,
        0x0b => libc::_SC_OPEN_MAX,
        0x27 | 0x28 => libc::_SC_PAGESIZE,
        0x60 => libc::_SC_NPROCESSORS_CONF,
        0x61 => libc::_SC_NPROCESSORS_ONLN,
        0x62 => libc::_SC_PHYS_PAGES,
        0x63 => libc::_SC_AVPHYS_PAGES,
        0x64 => libc::_SC_MONOTONIC_CLOCK,
        other => {
            log::warn!("sysconf: unmapped bionic name {other:#x}");
            set_errno(libc::EINVAL);
            return -1;
        }
    };
    unsafe { libc::sysconf(host) }
}

// --- system properties -----------------------------------------------------

fn property(name: &str) -> &'static str {
    match name {
        "ro.build.version.sdk" => "33",
        "ro.build.version.release" => "13",
        "ro.build.version.codename" => "REL",
        "ro.product.cpu.abi" => "x86_64",
        "ro.product.cpu.abilist" => "x86_64",
        "ro.product.model" => "Linux",
        "ro.product.manufacturer" => "Lucid",
        "ro.product.brand" => "Lucid",
        "ro.product.device" => "lucid",
        "ro.product.name" => "lucid",
        "ro.hardware" => "lucid",
        "ro.build.type" => "user",
        "ro.debuggable" => "0",
        _ => "",
    }
}

unsafe extern "C" fn system_property_get(name: *const c_char, value: *mut c_char) -> c_int {
    let n = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    let v = property(&n);
    log::debug!("__system_property_get({n}) -> {v:?}");
    unsafe {
        std::ptr::copy_nonoverlapping(v.as_ptr(), value as *mut u8, v.len());
        *value.add(v.len()) = 0;
    }
    v.len() as c_int
}

// --- fortify helpers that glibc lacks or spells differently -----------------

unsafe extern "C" fn strlen_chk(s: *const c_char, _n: size_t) -> size_t {
    unsafe { libc::strlen(s) }
}
unsafe extern "C" fn strchr_chk(s: *const c_char, c: c_int, _n: size_t) -> *mut c_char {
    unsafe { libc::strchr(s, c) }
}
unsafe extern "C" fn strrchr_chk(s: *const c_char, c: c_int, _n: size_t) -> *mut c_char {
    unsafe { libc::strrchr(s, c) }
}
unsafe extern "C" fn strncpy_chk2(
    dst: *mut c_char,
    src: *const c_char,
    n: size_t,
    _dst_len: size_t,
    _src_len: size_t,
) -> *mut c_char {
    unsafe { libc::strncpy(dst, src, n) }
}
unsafe extern "C" fn write_chk(fd: c_int, buf: *const c_void, n: size_t, _b: size_t) -> ssize_t {
    unsafe { libc::write(fd, buf, n) }
}
unsafe extern "C" fn sendto_chk(
    fd: c_int,
    buf: *const c_void,
    n: size_t,
    _buf_size: size_t,
    flags: c_int,
    addr: *const libc::sockaddr,
    len: libc::socklen_t,
) -> ssize_t {
    unsafe { libc::sendto(fd, buf, n, flags, addr, len) }
}
unsafe extern "C" fn fd_set_chk(fd: c_int, set: *mut libc::fd_set, _size: size_t) {
    unsafe { libc::FD_SET(fd, set) }
}
unsafe extern "C" fn fd_clr_chk(fd: c_int, set: *mut libc::fd_set, _size: size_t) {
    unsafe { libc::FD_CLR(fd, set) }
}
unsafe extern "C" fn fd_isset_chk(fd: c_int, set: *const libc::fd_set, _size: size_t) -> c_int {
    unsafe { libc::FD_ISSET(fd, set) as c_int }
}

// --- strerror_r: bionic exports the POSIX flavour under the plain name ------

unsafe extern "C" {
    fn __xpg_strerror_r(e: c_int, buf: *mut c_char, len: size_t) -> c_int;
}

unsafe extern "C" fn posix_strerror_r(e: c_int, buf: *mut c_char, len: size_t) -> c_int {
    unsafe { __xpg_strerror_r(e, buf, len) }
}

unsafe extern "C" fn gnu_strerror_r(e: c_int, buf: *mut c_char, len: size_t) -> *mut c_char {
    unsafe { __xpg_strerror_r(e, buf, len) };
    buf
}

// --- locales: bionic only knows "C"/"C.UTF-8" --------------------------------

unsafe extern "C" fn newlocale(
    _mask: c_int,
    _name: *const c_char,
    _base: libc::locale_t,
) -> libc::locale_t {
    unsafe { libc::newlocale(libc::LC_ALL_MASK, c"C.UTF-8".as_ptr(), std::ptr::null_mut()) }
}

// --- allocator statistics ----------------------------------------------------

#[repr(C)]
struct Mallinfo {
    fields: [size_t; 10],
}

unsafe extern "C" fn mallinfo() -> Mallinfo {
    Mallinfo { fields: [0; 10] }
}

// --- diagnostics ------------------------------------------------------------

unsafe extern "C" fn assert_fail(file: *const c_char, line: c_int, expr: *const c_char) -> ! {
    let file = unsafe { CStr::from_ptr(file) }.to_string_lossy();
    let expr = unsafe { CStr::from_ptr(expr) }.to_string_lossy();
    eprintln!("lucid: guest assertion failed: {file}:{line}: {expr}");
    std::process::abort();
}

unsafe extern "C" fn assert2_fail(
    file: *const c_char,
    line: c_int,
    func: *const c_char,
    expr: *const c_char,
) -> ! {
    let file = unsafe { CStr::from_ptr(file) }.to_string_lossy();
    let func = unsafe { CStr::from_ptr(func) }.to_string_lossy();
    let expr = unsafe { CStr::from_ptr(expr) }.to_string_lossy();
    eprintln!("lucid: guest assertion failed: {file}:{line}: {func}: {expr}");
    std::process::abort();
}

unsafe extern "C" fn set_abort_message(msg: *const c_char) {
    if !msg.is_null() {
        let msg = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
        log::error!("guest abort message: {msg}");
    }
}

unsafe extern "C" fn guest_abort() -> ! {
    eprintln!("lucid: guest called abort()");
    unsafe {
        // Make sure the default action runs even if the guest hooked SIGABRT.
        libc::signal(libc::SIGABRT, libc::SIG_DFL);
        libc::abort()
    }
}

// --- process control we do not let the guest do ------------------------------

unsafe extern "C" fn fork() -> c_int {
    log::warn!("guest fork() denied");
    set_errno(libc::ENOSYS);
    -1
}

unsafe extern "C" fn exec_denied(path: *const c_char) -> c_int {
    let p = unsafe { CStr::from_ptr(path) }.to_string_lossy();
    log::warn!("guest exec({p}) denied");
    set_errno(libc::EACCES);
    -1
}

unsafe extern "C" fn system(cmd: *const c_char) -> c_int {
    if cmd.is_null() {
        return 0;
    }
    let c = unsafe { CStr::from_ptr(cmd) }.to_string_lossy();
    log::warn!("guest system({c}) denied");
    -1
}

unsafe extern "C" fn waitpid(_pid: c_int, _status: *mut c_int, _opts: c_int) -> c_int {
    set_errno(libc::ECHILD);
    -1
}

unsafe extern "C" fn ptrace(req: c_int) -> c_long {
    log::debug!("guest ptrace({req}) ignored");
    0
}

unsafe extern "C" fn no_tty() -> c_int {
    set_errno(libc::ENOTTY);
    -1
}

pub fn symbols(m: &mut HashMap<&'static str, usize>) {
    m.insert("__errno", errno_location as usize);
    m.insert("__stack_chk_guard", &raw const STACK_CHK_GUARD as usize);
    m.insert("sysconf", sysconf as usize);
    m.insert("__system_property_get", system_property_get as usize);
    m.insert("__strlen_chk", strlen_chk as usize);
    m.insert("__strchr_chk", strchr_chk as usize);
    m.insert("__strrchr_chk", strrchr_chk as usize);
    m.insert("__strncpy_chk2", strncpy_chk2 as usize);
    m.insert("__write_chk", write_chk as usize);
    m.insert("__sendto_chk", sendto_chk as usize);
    m.insert("__FD_SET_chk", fd_set_chk as usize);
    m.insert("__FD_CLR_chk", fd_clr_chk as usize);
    m.insert("__FD_ISSET_chk", fd_isset_chk as usize);
    m.insert("strerror_r", posix_strerror_r as usize);
    m.insert("__gnu_strerror_r", gnu_strerror_r as usize);
    m.insert("newlocale", newlocale as usize);
    m.insert("mallinfo", mallinfo as usize);
    m.insert("__assert", assert_fail as usize);
    m.insert("__assert2", assert2_fail as usize);
    m.insert("android_set_abort_message", set_abort_message as usize);
    m.insert("abort", guest_abort as usize);
    m.insert("fork", fork as usize);
    m.insert("execv", exec_denied as usize);
    m.insert("execve", exec_denied as usize);
    m.insert("execvp", exec_denied as usize);
    m.insert("system", system as usize);
    m.insert("waitpid", waitpid as usize);
    m.insert("ptrace", ptrace as usize);
    m.insert("tcgetattr", no_tty as usize);
    m.insert("tcsetattr", no_tty as usize);
}
