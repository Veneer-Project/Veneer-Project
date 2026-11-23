//! `libvulkan.so`: the host Vulkan loader, with Android's window-system
//! integration (`VK_KHR_android_surface`) mapped onto Wayland or Xlib.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::OnceLock;

use crate::window::{Backend, window};

const VK_SUCCESS: i32 = 0;
const VK_INCOMPLETE: i32 = 5;
const VK_ERROR_INITIALIZATION_FAILED: i32 = -3;
const VK_ERROR_EXTENSION_NOT_PRESENT: i32 = -7;

const ANDROID_SURFACE: &CStr = c"VK_KHR_android_surface";
const WAYLAND_SURFACE: &CStr = c"VK_KHR_wayland_surface";
const XLIB_SURFACE: &CStr = c"VK_KHR_xlib_surface";

fn host() -> usize {
    static HOST: OnceLock<usize> = OnceLock::new();
    *HOST.get_or_init(|| unsafe {
        let h = libc::dlopen(c"libvulkan.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
        if h.is_null() {
            log::warn!("libvulkan.so.1 is not available on the host");
        }
        h as usize
    })
}

fn host_sym(name: &str) -> Option<usize> {
    let h = host();
    if h == 0 {
        return None;
    }
    let c = CString::new(name).ok()?;
    let p = unsafe { libc::dlsym(h as *mut c_void, c.as_ptr()) };
    (!p.is_null()).then_some(p as usize)
}

/// The host surface extension that stands in for `VK_KHR_android_surface`.
fn host_surface_ext() -> &'static CStr {
    match window().map(|w| w.backend) {
        Some(Backend::Xlib { .. }) => XLIB_SURFACE,
        _ => WAYLAND_SURFACE,
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ExtensionProperties {
    name: [c_char; 256],
    spec_version: u32,
}

unsafe extern "C" fn enumerate_instance_extensions(
    layer: *const c_char,
    count: *mut u32,
    props: *mut ExtensionProperties,
) -> i32 {
    let Some(f) = host_sym("vkEnumerateInstanceExtensionProperties") else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let f: unsafe extern "C" fn(*const c_char, *mut u32, *mut ExtensionProperties) -> i32 =
        unsafe { std::mem::transmute(f) };
    // Fetch the complete host list, then present the host's surface
    // extension under the Android name.
    let mut n = 0u32;
    let r = unsafe { f(layer, &mut n, std::ptr::null_mut()) };
    if r != VK_SUCCESS {
        return r;
    }
    let mut list = vec![
        ExtensionProperties {
            name: [0; 256],
            spec_version: 0
        };
        n as usize
    ];
    let r = unsafe { f(layer, &mut n, list.as_mut_ptr()) };
    if r != VK_SUCCESS && r != VK_INCOMPLETE {
        return r;
    }
    list.truncate(n as usize);
    let wanted = host_surface_ext();
    for e in list.iter_mut() {
        if unsafe { CStr::from_ptr(e.name.as_ptr()) } == wanted {
            e.name = [0; 256];
            for (i, b) in ANDROID_SURFACE.to_bytes().iter().enumerate() {
                e.name[i] = *b as c_char;
            }
            e.spec_version = 6;
        }
    }
    unsafe {
        if props.is_null() {
            *count = list.len() as u32;
            return VK_SUCCESS;
        }
        let room = *count as usize;
        let copied = room.min(list.len());
        std::ptr::copy_nonoverlapping(list.as_ptr(), props, copied);
        *count = copied as u32;
        if copied < list.len() { VK_INCOMPLETE } else { VK_SUCCESS }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InstanceCreateInfo {
    s_type: u32,
    p_next: *const c_void,
    flags: u32,
    application_info: *const c_void,
    layer_count: u32,
    layer_names: *const *const c_char,
    extension_count: u32,
    extension_names: *const *const c_char,
}

unsafe extern "C" fn create_instance(
    info: *const InstanceCreateInfo,
    alloc: *const c_void,
    out: *mut usize,
) -> i32 {
    let Some(f) = host_sym("vkCreateInstance") else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let f: unsafe extern "C" fn(*const InstanceCreateInfo, *const c_void, *mut usize) -> i32 =
        unsafe { std::mem::transmute(f) };
    let mut patched = unsafe { *info };
    let names =
        unsafe { std::slice::from_raw_parts(patched.extension_names, patched.extension_count as usize) };
    let translated: Vec<*const c_char> = names
        .iter()
        .map(|&n| {
            if unsafe { CStr::from_ptr(n) } == ANDROID_SURFACE {
                host_surface_ext().as_ptr()
            } else {
                n
            }
        })
        .collect();
    for n in &translated {
        log::debug!("vkCreateInstance extension {:?}", unsafe { CStr::from_ptr(*n) });
    }
    patched.extension_names = translated.as_ptr();
    let r = unsafe { f(&patched, alloc, out) };
    log::debug!("vkCreateInstance -> {r}");
    r
}

#[repr(C)]
struct AndroidSurfaceCreateInfo {
    s_type: u32,
    p_next: *const c_void,
    flags: u32,
    window: *mut c_void,
}

#[repr(C)]
struct WaylandSurfaceCreateInfo {
    s_type: u32,
    p_next: *const c_void,
    flags: u32,
    display: usize,
    surface: usize,
}

#[repr(C)]
struct XlibSurfaceCreateInfo {
    s_type: u32,
    p_next: *const c_void,
    flags: u32,
    dpy: usize,
    window: u64,
}

unsafe extern "C" fn create_android_surface(
    instance: usize,
    info: *const AndroidSurfaceCreateInfo,
    alloc: *const c_void,
    out: *mut u64,
) -> i32 {
    let Some(w) = window().filter(|w| w.as_ptr() == unsafe { (*info).window }) else {
        log::error!("vkCreateAndroidSurfaceKHR with an unknown native window");
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let r = match w.backend {
        Backend::Wayland { display, surface } => {
            let Some(f) = host_sym("vkCreateWaylandSurfaceKHR") else {
                return VK_ERROR_EXTENSION_NOT_PRESENT;
            };
            let f: unsafe extern "C" fn(
                usize,
                *const WaylandSurfaceCreateInfo,
                *const c_void,
                *mut u64,
            ) -> i32 = unsafe { std::mem::transmute(f) };
            let host_info = WaylandSurfaceCreateInfo {
                s_type: 1000006000,
                p_next: std::ptr::null(),
                flags: 0,
                display,
                surface,
            };
            unsafe { f(instance, &host_info, alloc, out) }
        }
        Backend::Xlib { display, window } => {
            let Some(f) = host_sym("vkCreateXlibSurfaceKHR") else {
                return VK_ERROR_EXTENSION_NOT_PRESENT;
            };
            let f: unsafe extern "C" fn(
                usize,
                *const XlibSurfaceCreateInfo,
                *const c_void,
                *mut u64,
            ) -> i32 = unsafe { std::mem::transmute(f) };
            let host_info = XlibSurfaceCreateInfo {
                s_type: 1000004000,
                p_next: std::ptr::null(),
                flags: 0,
                dpy: display,
                window,
            };
            unsafe { f(instance, &host_info, alloc, out) }
        }
    };
    log::debug!("vkCreateAndroidSurfaceKHR -> {r}");
    r
}

#[repr(C)]
struct SurfaceCapabilities {
    min_image_count: u32,
    max_image_count: u32,
    current_extent: [u32; 2],
    min_image_extent: [u32; 2],
    max_image_extent: [u32; 2],
    max_image_array_layers: u32,
    supported_transforms: u32,
    current_transform: u32,
    supported_composite_alpha: u32,
    supported_usage_flags: u32,
}

/// Wayland surfaces have no intrinsic size (`currentExtent` is the special
/// value 0xFFFFFFFF); Android always reports the real size, and code written
/// for Android relies on that.
unsafe extern "C" fn get_surface_capabilities(
    device: usize,
    surface: u64,
    caps: *mut SurfaceCapabilities,
) -> i32 {
    let Some(f) = host_sym("vkGetPhysicalDeviceSurfaceCapabilitiesKHR") else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let f: unsafe extern "C" fn(usize, u64, *mut SurfaceCapabilities) -> i32 =
        unsafe { std::mem::transmute(f) };
    let r = unsafe { f(device, surface, caps) };
    if r == VK_SUCCESS {
        let caps = unsafe { &mut *caps };
        if caps.current_extent[0] == u32::MAX {
            if let Some(w) = window() {
                let (width, height) = w.size();
                caps.current_extent = [width.max(1) as u32, height.max(1) as u32];
            }
        }
    }
    r
}

const VK_SUBOPTIMAL_KHR: i32 = 1000001003;

/// `VK_SUBOPTIMAL_KHR` → `VK_SUCCESS`, logged the first time.
///
/// The image is still valid and presentable; "suboptimal" only says a new
/// swapchain would suit the surface better. Android practically never
/// returns it (only for rotation, which a desktop window does not have),
/// and the engine treats it as fatal: it crashes a moment later. Mesa on
/// Wayland returns it whenever the compositor's dma-buf feedback changes,
/// which KWin does when the window goes fullscreen (direct scanout). The
/// engine recreates its swapchain on the next resize anyway.
fn hide_suboptimal(r: i32, call: &str) -> i32 {
    if r != VK_SUBOPTIMAL_KHR {
        return r;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| log::debug!("{call} returned VK_SUBOPTIMAL_KHR; reported as VK_SUCCESS"));
    VK_SUCCESS
}

unsafe extern "C" fn acquire_next_image(
    device: usize,
    swapchain: u64,
    timeout: u64,
    semaphore: u64,
    fence: u64,
    index: *mut u32,
) -> i32 {
    let Some(f) = host_sym("vkAcquireNextImageKHR") else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let f: unsafe extern "C" fn(usize, u64, u64, u64, u64, *mut u32) -> i32 =
        unsafe { std::mem::transmute(f) };
    hide_suboptimal(
        unsafe { f(device, swapchain, timeout, semaphore, fence, index) },
        "vkAcquireNextImageKHR",
    )
}

unsafe extern "C" fn acquire_next_image2(device: usize, info: *const c_void, index: *mut u32) -> i32 {
    let Some(f) = host_sym("vkAcquireNextImage2KHR") else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let f: unsafe extern "C" fn(usize, *const c_void, *mut u32) -> i32 = unsafe { std::mem::transmute(f) };
    hide_suboptimal(unsafe { f(device, info, index) }, "vkAcquireNextImage2KHR")
}

#[repr(C)]
struct PresentInfo {
    s_type: u32,
    p_next: *const c_void,
    wait_semaphore_count: u32,
    wait_semaphores: *const u64,
    swapchain_count: u32,
    swapchains: *const u64,
    image_indices: *const u32,
    results: *mut i32,
}

unsafe extern "C" fn queue_present(queue: usize, info: *const PresentInfo) -> i32 {
    let Some(f) = host_sym("vkQueuePresentKHR") else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let f: unsafe extern "C" fn(usize, *const PresentInfo) -> i32 = unsafe { std::mem::transmute(f) };
    let r = unsafe { f(queue, info) };
    let info = unsafe { &*info };
    if !info.results.is_null() {
        let results = unsafe { std::slice::from_raw_parts_mut(info.results, info.swapchain_count as usize) };
        for r in results {
            *r = hide_suboptimal(*r, "vkQueuePresentKHR");
        }
    }
    hide_suboptimal(r, "vkQueuePresentKHR")
}

unsafe extern "C" fn get_device_proc_addr(device: usize, name: *const c_char) -> usize {
    let n = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    if let Some(a) = device_overrides(&n) {
        return a;
    }
    let Some(f) = host_sym("vkGetDeviceProcAddr") else {
        return 0;
    };
    let f: unsafe extern "C" fn(usize, *const c_char) -> usize = unsafe { std::mem::transmute(f) };
    unsafe { f(device, name) }
}

/// Device-level functions replaced here; looked up by `vkGetDeviceProcAddr`
/// too, so the engine gets them however it asks.
fn device_overrides(name: &str) -> Option<usize> {
    Some(match name {
        "vkAcquireNextImageKHR" => acquire_next_image as usize,
        "vkAcquireNextImage2KHR" => acquire_next_image2 as usize,
        "vkQueuePresentKHR" => queue_present as usize,
        "vkGetDeviceProcAddr" => get_device_proc_addr as usize,
        _ => return None,
    })
}

fn overrides() -> &'static HashMap<&'static str, usize> {
    static OVERRIDES: OnceLock<HashMap<&'static str, usize>> = OnceLock::new();
    OVERRIDES.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert(
            "vkEnumerateInstanceExtensionProperties",
            enumerate_instance_extensions as usize,
        );
        m.insert("vkCreateInstance", create_instance as usize);
        m.insert("vkCreateAndroidSurfaceKHR", create_android_surface as usize);
        m.insert(
            "vkGetPhysicalDeviceSurfaceCapabilitiesKHR",
            get_surface_capabilities as usize,
        );
        m.insert("vkGetInstanceProcAddr", get_instance_proc_addr as usize);
        for name in [
            "vkAcquireNextImageKHR",
            "vkAcquireNextImage2KHR",
            "vkQueuePresentKHR",
            "vkGetDeviceProcAddr",
        ] {
            m.insert(name, device_overrides(name).unwrap());
        }
        m
    })
}

unsafe extern "C" fn get_instance_proc_addr(instance: usize, name: *const c_char) -> usize {
    let n = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    if let Some(a) = overrides().get(&*n) {
        return *a;
    }
    let Some(f) = host_sym("vkGetInstanceProcAddr") else {
        return 0;
    };
    let f: unsafe extern "C" fn(usize, *const c_char) -> usize = unsafe { std::mem::transmute(f) };
    unsafe { f(instance, name) }
}

pub fn resolve(name: &str) -> Option<usize> {
    overrides().get(name).copied().or_else(|| host_sym(name))
}

pub fn available() -> bool {
    host() != 0
}
