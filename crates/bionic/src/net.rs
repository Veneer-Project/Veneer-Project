//! Name resolution: `struct addrinfo` field order, `AI_*`/`NI_*` flags and
//! `EAI_*` error codes all differ between bionic and glibc.

use std::collections::HashMap;
use std::ffi::{c_char, c_int};

use libc::{sockaddr, socklen_t};

#[repr(C)]
struct BionicAddrinfo {
    ai_flags: c_int,
    ai_family: c_int,
    ai_socktype: c_int,
    ai_protocol: c_int,
    ai_addrlen: socklen_t,
    ai_canonname: *mut c_char,
    ai_addr: *mut sockaddr,
    ai_next: *mut BionicAddrinfo,
}

fn ai_flags_to_host(f: c_int) -> c_int {
    let mut h = 0;
    for (b, g) in [
        (0x1, libc::AI_PASSIVE),
        (0x2, libc::AI_CANONNAME),
        (0x4, libc::AI_NUMERICHOST),
        (0x8, libc::AI_NUMERICSERV),
        (0x100, libc::AI_ALL),
        (0x400, libc::AI_ADDRCONFIG),
        (0x800, libc::AI_V4MAPPED),
    ] {
        if f & b != 0 {
            h |= g;
        }
    }
    h
}

fn eai_to_bionic(e: c_int) -> c_int {
    match e {
        0 => 0,
        libc::EAI_AGAIN => 2,
        libc::EAI_BADFLAGS => 3,
        libc::EAI_FAIL => 4,
        libc::EAI_FAMILY => 5,
        libc::EAI_MEMORY => 6,
        libc::EAI_NODATA => 7,
        libc::EAI_NONAME => 8,
        libc::EAI_SERVICE => 9,
        libc::EAI_SOCKTYPE => 10,
        libc::EAI_SYSTEM => 11,
        libc::EAI_OVERFLOW => 14,
        _ => 4,
    }
}

/// glibc and bionic `addrinfo` have the same size; only `ai_addr` and
/// `ai_canonname` swap places, so the list is converted in place.
unsafe fn swap_list(mut p: *mut BionicAddrinfo) {
    while !p.is_null() {
        unsafe {
            let a = (*p).ai_canonname as usize;
            (*p).ai_canonname = (*p).ai_addr as *mut c_char;
            (*p).ai_addr = a as *mut sockaddr;
            p = (*p).ai_next;
        }
    }
}

unsafe extern "C" fn getaddrinfo(
    node: *const c_char,
    service: *const c_char,
    hints: *const BionicAddrinfo,
    res: *mut *mut BionicAddrinfo,
) -> c_int {
    let mut host_hints: libc::addrinfo = unsafe { std::mem::zeroed() };
    let hints_ptr = if hints.is_null() {
        std::ptr::null()
    } else {
        let h = unsafe { &*hints };
        host_hints.ai_flags = ai_flags_to_host(h.ai_flags);
        host_hints.ai_family = h.ai_family;
        host_hints.ai_socktype = h.ai_socktype;
        host_hints.ai_protocol = h.ai_protocol;
        &host_hints as *const libc::addrinfo
    };
    let mut out: *mut libc::addrinfo = std::ptr::null_mut();
    let r = unsafe { libc::getaddrinfo(node, service, hints_ptr, &mut out) };
    if r != 0 {
        return eai_to_bionic(r);
    }
    unsafe {
        swap_list(out as *mut BionicAddrinfo);
        *res = out as *mut BionicAddrinfo;
    }
    0
}

unsafe extern "C" fn freeaddrinfo(res: *mut BionicAddrinfo) {
    if res.is_null() {
        return;
    }
    unsafe {
        swap_list(res);
        libc::freeaddrinfo(res as *mut libc::addrinfo);
    }
}

unsafe extern "C" fn gai_strerror(e: c_int) -> *const c_char {
    let s: &'static std::ffi::CStr = match e {
        0 => c"Success",
        2 => c"Temporary failure in name resolution",
        3 => c"Invalid value for ai_flags",
        4 => c"Non-recoverable failure in name resolution",
        5 => c"ai_family not supported",
        6 => c"Memory allocation failure",
        7 => c"No address associated with hostname",
        8 => c"hostname nor servname provided, or not known",
        9 => c"servname not supported for ai_socktype",
        10 => c"ai_socktype not supported",
        11 => c"System error returned in errno",
        _ => c"Unknown error",
    };
    s.as_ptr()
}

unsafe extern "C" fn getnameinfo(
    sa: *const sockaddr,
    salen: socklen_t,
    host: *mut c_char,
    hostlen: socklen_t,
    serv: *mut c_char,
    servlen: socklen_t,
    flags: c_int,
) -> c_int {
    let mut h = 0;
    for (b, g) in [
        (0x1, libc::NI_NOFQDN),
        (0x2, libc::NI_NUMERICHOST),
        (0x4, libc::NI_NAMEREQD),
        (0x8, libc::NI_NUMERICSERV),
        (0x10, libc::NI_DGRAM),
    ] {
        if flags & b != 0 {
            h |= g;
        }
    }
    eai_to_bionic(unsafe { libc::getnameinfo(sa, salen, host, hostlen, serv, servlen, h) })
}

pub fn symbols(m: &mut HashMap<&'static str, usize>) {
    m.insert("getaddrinfo", getaddrinfo as usize);
    m.insert("freeaddrinfo", freeaddrinfo as usize);
    m.insert("gai_strerror", gai_strerror as usize);
    m.insert("getnameinfo", getnameinfo as usize);
}
