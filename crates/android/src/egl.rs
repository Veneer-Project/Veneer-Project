//! `libEGL.so`: forwarded to the host EGL, except for the entry points that
//! take Android native handles (the display and the `ANativeWindow`).

use std::collections::HashMap;
use std::ffi::{CString, c_void};
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use crate::window::{Backend, NativeWindow, window};

const EGL_PLATFORM_X11_KHR: u32 = 0x31d5;
const EGL_PLATFORM_WAYLAND_KHR: u32 = 0x31d8;
const EGL_NONE: i32 = 0x3038;

struct Host {
    egl: usize,
    wayland_egl: usize,
}

fn host() -> &'static Host {
    static HOST: OnceLock<Host> = OnceLock::new();
    HOST.get_or_init(|| unsafe {
        let flags = libc::RTLD_NOW | libc::RTLD_GLOBAL;
        Host {
            egl: libc::dlopen(c"libEGL.so.1".as_ptr(), flags) as usize,
            wayland_egl: libc::dlopen(c"libwayland-egl.so.1".as_ptr(), flags) as usize,
        }
    })
}

fn sym(handle: usize, name: &str) -> Option<usize> {
    if handle == 0 {
        return None;
    }
    let c = CString::new(name).ok()?;
    let p = unsafe { libc::dlsym(handle as *mut c_void, c.as_ptr()) };
    (!p.is_null()).then_some(p as usize)
}

/// Get (creating on first use) the `wl_egl_window` for a Wayland window.
fn wl_egl_window(w: &NativeWindow, surface: usize) -> usize {
    let cur = w.egl_window.load(Ordering::Acquire);
    if cur != 0 {
        return cur;
    }
    let Some(create) = sym(host().wayland_egl, "wl_egl_window_create") else {
        log::error!("libwayland-egl.so.1 is not available");
        return 0;
    };
    let create: unsafe extern "C" fn(usize, i32, i32) -> usize =
        unsafe { std::mem::transmute(create) };
    let (width, height) = w.size();
    let win = unsafe { create(surface, width.max(1), height.max(1)) };
    w.egl_window.store(win, Ordering::Release);
    win
}

pub(crate) fn resize_egl_window(w: &NativeWindow, width: i32, height: i32) {
    let win = w.egl_window.load(Ordering::Acquire);
    if win == 0 {
        return;
    }
    if let Some(resize) = sym(host().wayland_egl, "wl_egl_window_resize") {
        let resize: unsafe extern "C" fn(usize, i32, i32, i32, i32) =
            unsafe { std::mem::transmute(resize) };
        unsafe { resize(win, width.max(1), height.max(1), 0, 0) };
    }
}

unsafe extern "C" fn get_display(native: *mut c_void) -> *mut c_void {
    let Some(w) = window() else {
        log::error!("eglGetDisplay called before a window exists");
        return std::ptr::null_mut();
    };
    let Some(f) = sym(host().egl, "eglGetPlatformDisplay") else {
        return std::ptr::null_mut();
    };
    let f: unsafe extern "C" fn(u32, usize, *const isize) -> *mut c_void =
        unsafe { std::mem::transmute(f) };
    let _ = native;
    let dpy = match w.backend {
        Backend::Wayland { display, .. } => unsafe {
            f(EGL_PLATFORM_WAYLAND_KHR, display, std::ptr::null())
        },
        Backend::Xlib { display, .. } => unsafe {
            f(EGL_PLATFORM_X11_KHR, display, std::ptr::null())
        },
    };
    log::debug!("eglGetDisplay -> {dpy:p}");
    dpy
}

unsafe extern "C" fn create_window_surface(
    dpy: *mut c_void,
    config: *mut c_void,
    win: *mut c_void,
    attribs: *const i32,
) -> *mut c_void {
    let Some(f) = sym(host().egl, "eglCreateWindowSurface") else {
        return std::ptr::null_mut();
    };
    let f: unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *const i32) -> *mut c_void =
        unsafe { std::mem::transmute(f) };
    let native = match window().filter(|w| w.as_ptr() == win) {
        Some(w) => match w.backend {
            Backend::Wayland { surface, .. } => wl_egl_window(w, surface),
            Backend::Xlib { window, .. } => window as usize,
        },
        None => {
            log::error!("eglCreateWindowSurface with an unknown native window {win:p}");
            return std::ptr::null_mut();
        }
    };
    let surface = unsafe { f(dpy, config, native, attribs) };
    log::debug!("eglCreateWindowSurface -> {surface:p}");
    surface
}

/// Android-only config attributes that desktop EGL rejects.
fn is_android_attrib(a: i32) -> bool {
    matches!(a, 0x3142 | 0x3147) // EGL_RECORDABLE_ANDROID, EGL_FRAMEBUFFER_TARGET_ANDROID
}

unsafe extern "C" fn choose_config(
    dpy: *mut c_void,
    attribs: *const i32,
    configs: *mut *mut c_void,
    size: i32,
    num: *mut i32,
) -> u32 {
    let Some(f) = sym(host().egl, "eglChooseConfig") else {
        return 0;
    };
    let f: unsafe extern "C" fn(*mut c_void, *const i32, *mut *mut c_void, i32, *mut i32) -> u32 =
        unsafe { std::mem::transmute(f) };
    let mut filtered = Vec::new();
    if !attribs.is_null() {
        let mut p = attribs;
        unsafe {
            while *p != EGL_NONE {
                if !is_android_attrib(*p) {
                    filtered.push(*p);
                    filtered.push(*p.add(1));
                }
                p = p.add(2);
            }
        }
    }
    filtered.push(EGL_NONE);
    unsafe { f(dpy, filtered.as_ptr(), configs, size, num) }
}

pub fn resolve(name: &str) -> Option<usize> {
    static OVERRIDES: OnceLock<HashMap<&'static str, usize>> = OnceLock::new();
    let o = OVERRIDES.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert("eglGetDisplay", get_display as usize);
        m.insert("eglCreateWindowSurface", create_window_surface as usize);
        m.insert("eglChooseConfig", choose_config as usize);
        m
    });
    o.get(name).copied().or_else(|| sym(host().egl, name))
}
