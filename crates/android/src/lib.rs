//! Host implementations of the Android NDK system libraries the guest links
//! against (`liblog`, `libandroid`, `libEGL`, `libGLESv2`, ...).

pub mod asset;
mod egl;
mod log_lib;
mod looper;
mod media;
mod vulkan;
pub mod window;

use std::collections::HashMap;
use std::ffi::{CString, c_void};

/// A virtual library that forwards every symbol to a host shared object
/// (used for EGL/GLES, whose C ABI is the same on Android and Linux).
fn host_passthrough(android_name: &str, host_names: &[&str]) {
    let mut handle: *mut c_void = std::ptr::null_mut();
    for n in host_names {
        let c = CString::new(*n).unwrap();
        handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        if !handle.is_null() {
            break;
        }
    }
    if handle.is_null() {
        log::warn!("host library for {android_name} not found ({host_names:?})");
    }
    let handle = handle as usize;
    lucid_linker::register_virtual(
        android_name,
        Box::new(move |name| {
            if handle == 0 {
                return None;
            }
            let c = CString::new(name).ok()?;
            let p = unsafe { libc::dlsym(handle as *mut c_void, c.as_ptr()) };
            (!p.is_null()).then_some(p as usize)
        }),
    );
}

/// Register all Android system libraries with the linker.
pub fn install() {
    let mut log = HashMap::new();
    log_lib::symbols(&mut log);
    lucid_linker::register_table("liblog.so", log);

    let mut m = HashMap::new();
    media::symbols(&mut m);
    lucid_linker::register_table("libmediandk.so", m);

    let mut a = HashMap::new();
    asset::symbols(&mut a);
    looper::symbols(&mut a);
    window::symbols(&mut a);
    lucid_linker::register_table("libandroid.so", a);
    // Some code asks for the window API under its newer library name.
    let mut nw = HashMap::new();
    window::symbols(&mut nw);
    lucid_linker::register_table("libnativewindow.so", nw);

    lucid_linker::register_virtual("libEGL.so", Box::new(egl::resolve));
    if vulkan::available() {
        lucid_linker::register_virtual("libvulkan.so", Box::new(vulkan::resolve));
    }
    host_passthrough("libGLESv2.so", &["libGLESv2.so.2"]);
    host_passthrough("libGLESv3.so", &["libGLESv2.so.2"]);
}
