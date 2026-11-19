//! A bionic (Android libc) personality on top of the host glibc.
//!
//! Most of bionic's x86-64 ABI matches glibc's because both follow the Linux
//! kernel's structure layouts, so the bulk of libc/libm is forwarded straight
//! to the host with `dlsym`. Only the entry points whose ABI or semantics
//! differ are reimplemented here (see the `overrides` table).

mod misc;
mod net;
mod signal;
mod stdio;
mod sync;

use std::collections::HashMap;
use std::ffi::{CString, c_void};
use std::sync::OnceLock;

/// Imports that must never reach the host implementation.
const DENIED: &[&str] = &["vfork", "__system_property_find", "__system_property_read"];

fn host_symbol(name: &str) -> Option<usize> {
    if DENIED.contains(&name) {
        return None;
    }
    let c = CString::new(name).ok()?;
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
    (!p.is_null()).then_some(p as usize)
}

fn overrides() -> &'static HashMap<&'static str, usize> {
    static TABLE: OnceLock<HashMap<&'static str, usize>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m = HashMap::new();
        m.extend(lucid_linker::dl_symbols());
        stdio::symbols(&mut m);
        sync::symbols(&mut m);
        signal::symbols(&mut m);
        net::symbols(&mut m);
        misc::symbols(&mut m);
        m
    })
}

/// Resolve a libc/libm/libdl symbol the way the guest should see it.
pub fn resolve(name: &str) -> Option<usize> {
    if let Some(a) = overrides().get(name) {
        return Some(*a);
    }
    let found = host_symbol(name);
    if found.is_some() {
        log::trace!("libc passthrough: {name}");
    }
    found
}

/// Register the bionic system libraries with the linker.
pub fn install() {
    unsafe {
        // bionic is always UTF-8 and has a "C"-like numeric locale.
        libc::setlocale(libc::LC_ALL, c"C.UTF-8".as_ptr());
        // Make sure libm is around for the dlsym passthrough.
        let _: *mut c_void = libc::dlopen(c"libm.so.6".as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
    }
    for lib in ["libc.so", "libm.so", "libdl.so"] {
        lucid_linker::register_virtual(lib, Box::new(resolve));
    }
}
