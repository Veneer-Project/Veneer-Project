//! `ANativeWindow` and `AConfiguration`, backed by the host window.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

/// Which host window system the window lives on, with raw handles.
#[derive(Clone, Copy, Debug)]
pub enum Backend {
    Wayland { display: usize, surface: usize },
    Xlib { display: usize, window: u64 },
}

/// The single application window; its address is the guest's
/// `ANativeWindow*`.
pub struct NativeWindow {
    pub backend: Backend,
    width: AtomicI32,
    height: AtomicI32,
    /// Density in dpi (Android's "160 = 1x").
    density: AtomicI32,
    /// Lazily created `wl_egl_window*` (Wayland + EGL only).
    pub(crate) egl_window: AtomicUsize,
}

static WINDOW: OnceLock<NativeWindow> = OnceLock::new();

/// Publish the host window. Must be called before the guest starts rendering.
pub fn set_window(backend: Backend, width: i32, height: i32, scale: f64) -> &'static NativeWindow {
    WINDOW.get_or_init(|| NativeWindow {
        backend,
        width: AtomicI32::new(width),
        height: AtomicI32::new(height),
        density: AtomicI32::new((160.0 * scale) as i32),
        egl_window: AtomicUsize::new(0),
    })
}

pub fn window() -> Option<&'static NativeWindow> {
    WINDOW.get()
}

impl NativeWindow {
    pub fn as_ptr(&'static self) -> *mut c_void {
        self as *const NativeWindow as *mut c_void
    }

    pub fn size(&self) -> (i32, i32) {
        (
            self.width.load(Ordering::Relaxed),
            self.height.load(Ordering::Relaxed),
        )
    }

    pub fn density(&self) -> i32 {
        self.density.load(Ordering::Relaxed)
    }

    /// Record a new size in physical pixels.
    pub fn resize(&self, width: i32, height: i32) {
        self.width.store(width, Ordering::Relaxed);
        self.height.store(height, Ordering::Relaxed);
        crate::egl::resize_egl_window(self, width, height);
    }

    pub fn set_scale(&self, scale: f64) {
        self.density.store((160.0 * scale) as i32, Ordering::Relaxed);
    }
}

fn is_our_window(p: *mut c_void) -> Option<&'static NativeWindow> {
    WINDOW.get().filter(|w| w.as_ptr() == p)
}

unsafe extern "C" fn from_surface(_env: *mut c_void, _surface: *mut c_void) -> *mut c_void {
    match WINDOW.get() {
        Some(w) => w.as_ptr(),
        None => {
            log::error!("ANativeWindow_fromSurface called before a window exists");
            std::ptr::null_mut()
        }
    }
}

unsafe extern "C" fn acquire(_w: *mut c_void) {}
unsafe extern "C" fn release(_w: *mut c_void) {}

unsafe extern "C" fn get_width(w: *mut c_void) -> i32 {
    is_our_window(w).map(|w| w.size().0).unwrap_or(-1)
}
unsafe extern "C" fn get_height(w: *mut c_void) -> i32 {
    is_our_window(w).map(|w| w.size().1).unwrap_or(-1)
}
unsafe extern "C" fn get_format(_w: *mut c_void) -> i32 {
    1 // WINDOW_FORMAT_RGBA_8888
}
unsafe extern "C" fn set_buffers_geometry(_w: *mut c_void, _width: i32, _height: i32, _f: i32) -> i32 {
    0
}

// --- AConfiguration -------------------------------------------------------------

unsafe extern "C" fn config_new() -> *mut c_void {
    Box::into_raw(Box::new(0u64)) as *mut c_void
}
unsafe extern "C" fn config_delete(c: *mut c_void) {
    if !c.is_null() {
        drop(unsafe { Box::from_raw(c as *mut u64) });
    }
}
unsafe extern "C" fn config_from_asset_manager(_c: *mut c_void, _am: *mut c_void) {}

fn locale_parts() -> (String, String) {
    let lang = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LC_MESSAGES"))
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default();
    let lang = lang.split('.').next().unwrap_or("");
    let mut it = lang.split('_');
    let l = it.next().filter(|s| s.len() == 2).unwrap_or("en").to_string();
    let c = it.next().filter(|s| s.len() == 2).unwrap_or("US").to_string();
    (l, c)
}

/// The user's language and country as two-letter codes (e.g. `("en", "US")`).
pub fn locale() -> (String, String) {
    locale_parts()
}

unsafe extern "C" fn config_get_language(_c: *mut c_void, out: *mut c_char) {
    let (l, _) = locale_parts();
    unsafe { std::ptr::copy_nonoverlapping(l.as_ptr(), out as *mut u8, 2) };
}
unsafe extern "C" fn config_get_country(_c: *mut c_void, out: *mut c_char) {
    let (_, c) = locale_parts();
    unsafe { std::ptr::copy_nonoverlapping(c.as_ptr(), out as *mut u8, 2) };
}

fn dp(px: i32, density: i32) -> i32 {
    px * 160 / density.max(1)
}

unsafe extern "C" fn config_width_dp(_c: *mut c_void) -> i32 {
    WINDOW.get().map(|w| dp(w.size().0, w.density())).unwrap_or(1280)
}
unsafe extern "C" fn config_height_dp(_c: *mut c_void) -> i32 {
    WINDOW.get().map(|w| dp(w.size().1, w.density())).unwrap_or(720)
}
unsafe extern "C" fn config_screen_size(_c: *mut c_void) -> i32 {
    4 // ACONFIGURATION_SCREENSIZE_XLARGE
}
unsafe extern "C" fn config_nav_hidden(_c: *mut c_void) -> i32 {
    1 // ACONFIGURATION_NAVHIDDEN_NO
}
unsafe extern "C" fn config_density(_c: *mut c_void) -> i32 {
    WINDOW.get().map(|w| w.density()).unwrap_or(160)
}
unsafe extern "C" fn config_orientation(_c: *mut c_void) -> i32 {
    2 // ACONFIGURATION_ORIENTATION_LAND
}
unsafe extern "C" fn config_keyboard(_c: *mut c_void) -> i32 {
    2 // ACONFIGURATION_KEYBOARD_QWERTY
}
unsafe extern "C" fn config_sdk_version(_c: *mut c_void) -> i32 {
    33
}
unsafe extern "C" fn config_zero(_c: *mut c_void) -> c_int {
    0
}

pub fn symbols(m: &mut HashMap<&'static str, usize>) {
    m.insert("ANativeWindow_fromSurface", from_surface as usize);
    m.insert("ANativeWindow_acquire", acquire as usize);
    m.insert("ANativeWindow_release", release as usize);
    m.insert("ANativeWindow_getWidth", get_width as usize);
    m.insert("ANativeWindow_getHeight", get_height as usize);
    m.insert("ANativeWindow_getFormat", get_format as usize);
    m.insert("ANativeWindow_setBuffersGeometry", set_buffers_geometry as usize);

    m.insert("AConfiguration_new", config_new as usize);
    m.insert("AConfiguration_delete", config_delete as usize);
    m.insert("AConfiguration_fromAssetManager", config_from_asset_manager as usize);
    m.insert("AConfiguration_getLanguage", config_get_language as usize);
    m.insert("AConfiguration_getCountry", config_get_country as usize);
    m.insert("AConfiguration_getScreenWidthDp", config_width_dp as usize);
    m.insert("AConfiguration_getScreenHeightDp", config_height_dp as usize);
    m.insert("AConfiguration_getScreenSize", config_screen_size as usize);
    m.insert("AConfiguration_getNavHidden", config_nav_hidden as usize);
    m.insert("AConfiguration_getDensity", config_density as usize);
    m.insert("AConfiguration_getOrientation", config_orientation as usize);
    m.insert("AConfiguration_getKeyboard", config_keyboard as usize);
    m.insert("AConfiguration_getSdkVersion", config_sdk_version as usize);
    m.insert("AConfiguration_getMcc", config_zero as usize);
    m.insert("AConfiguration_getMnc", config_zero as usize);
}
