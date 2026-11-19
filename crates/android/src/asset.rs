//! `AAssetManager` / `AAsset`: APK assets served from an extracted directory.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::OnceLock;

static ASSET_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Set the directory that holds the APK's `assets/` tree.
pub fn set_root(path: PathBuf) {
    let _ = ASSET_ROOT.set(path);
}

pub fn root() -> Option<&'static PathBuf> {
    ASSET_ROOT.get()
}

/// The one asset manager; its address is the guest's `AAssetManager*`.
static MANAGER: u8 = 0;

pub fn manager_ptr() -> *mut c_void {
    &MANAGER as *const u8 as *mut c_void
}

struct Asset {
    file: File,
    map: *mut c_void,
    len: usize,
    pos: usize,
}

impl Drop for Asset {
    fn drop(&mut self) {
        if self.len > 0 {
            unsafe { libc::munmap(self.map, self.len) };
        }
    }
}

fn resolve(name: &str) -> Option<PathBuf> {
    let root = ASSET_ROOT.get()?;
    let rel = name.trim_start_matches('/');
    if rel.split('/').any(|c| c == "..") {
        return None;
    }
    Some(root.join(rel))
}

unsafe extern "C" fn manager_from_java(_env: *mut c_void, _obj: *mut c_void) -> *mut c_void {
    manager_ptr()
}

unsafe extern "C" fn manager_open(_m: *mut c_void, name: *const c_char, _mode: c_int) -> *mut Asset {
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    let Some(path) = resolve(&name) else {
        return std::ptr::null_mut();
    };
    let Ok(file) = File::open(&path) else {
        log::debug!("AAssetManager_open({name}) -> not found");
        return std::ptr::null_mut();
    };
    let Ok(meta) = file.metadata() else {
        return std::ptr::null_mut();
    };
    if !meta.is_file() {
        return std::ptr::null_mut();
    }
    let len = meta.len() as usize;
    let map = if len == 0 {
        std::ptr::NonNull::<u8>::dangling().as_ptr() as *mut c_void
    } else {
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return std::ptr::null_mut();
        }
        p
    };
    log::trace!("AAssetManager_open({name}) -> {len} bytes");
    Box::into_raw(Box::new(Asset {
        file,
        map,
        len,
        pos: 0,
    }))
}

unsafe extern "C" fn asset_close(a: *mut Asset) {
    if !a.is_null() {
        drop(unsafe { Box::from_raw(a) });
    }
}

unsafe extern "C" fn asset_get_buffer(a: *mut Asset) -> *const c_void {
    unsafe { (*a).map }
}

unsafe extern "C" fn asset_get_length(a: *mut Asset) -> i64 {
    unsafe { (*a).len as i64 }
}

unsafe extern "C" fn asset_get_remaining(a: *mut Asset) -> i64 {
    unsafe { ((*a).len - (*a).pos) as i64 }
}

unsafe extern "C" fn asset_read(a: *mut Asset, buf: *mut c_void, count: usize) -> c_int {
    let a = unsafe { &mut *a };
    let n = count.min(a.len - a.pos);
    unsafe { std::ptr::copy_nonoverlapping((a.map as *const u8).add(a.pos), buf as *mut u8, n) };
    a.pos += n;
    n as c_int
}

unsafe extern "C" fn asset_seek(a: *mut Asset, offset: i64, whence: c_int) -> i64 {
    let a = unsafe { &mut *a };
    let base = match whence {
        libc::SEEK_SET => 0,
        libc::SEEK_CUR => a.pos as i64,
        libc::SEEK_END => a.len as i64,
        _ => return -1,
    };
    let target = base + offset;
    if target < 0 || target > a.len as i64 {
        return -1;
    }
    a.pos = target as usize;
    target
}

unsafe extern "C" fn asset_open_fd(a: *mut Asset, start: *mut i64, length: *mut i64) -> c_int {
    let a = unsafe { &*a };
    let fd = unsafe { libc::dup(a.file.as_raw_fd()) };
    if fd >= 0 {
        unsafe {
            *start = 0;
            *length = a.len as i64;
        }
    }
    fd
}

unsafe extern "C" fn asset_is_allocated(_a: *mut Asset) -> c_int {
    0
}

struct AssetDir {
    names: Vec<CString>,
    next: usize,
}

unsafe extern "C" fn manager_open_dir(_m: *mut c_void, name: *const c_char) -> *mut AssetDir {
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    let mut names = Vec::new();
    if let Some(Ok(rd)) = resolve(&name).map(std::fs::read_dir) {
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| t.is_file()) {
                if let Ok(c) = CString::new(e.file_name().to_string_lossy().as_bytes()) {
                    names.push(c);
                }
            }
        }
    }
    names.sort();
    Box::into_raw(Box::new(AssetDir { names, next: 0 }))
}

unsafe extern "C" fn dir_next(d: *mut AssetDir) -> *const c_char {
    let d = unsafe { &mut *d };
    match d.names.get(d.next) {
        Some(n) => {
            d.next += 1;
            n.as_ptr()
        }
        None => std::ptr::null(),
    }
}

unsafe extern "C" fn dir_rewind(d: *mut AssetDir) {
    unsafe { (*d).next = 0 };
}

unsafe extern "C" fn dir_close(d: *mut AssetDir) {
    if !d.is_null() {
        drop(unsafe { Box::from_raw(d) });
    }
}

pub fn symbols(m: &mut HashMap<&'static str, usize>) {
    m.insert("AAssetManager_fromJava", manager_from_java as usize);
    m.insert("AAssetManager_open", manager_open as usize);
    m.insert("AAssetManager_openDir", manager_open_dir as usize);
    m.insert("AAssetDir_getNextFileName", dir_next as usize);
    m.insert("AAssetDir_rewind", dir_rewind as usize);
    m.insert("AAssetDir_close", dir_close as usize);
    m.insert("AAsset_close", asset_close as usize);
    m.insert("AAsset_getBuffer", asset_get_buffer as usize);
    m.insert("AAsset_getLength", asset_get_length as usize);
    m.insert("AAsset_getLength64", asset_get_length as usize);
    m.insert("AAsset_getRemainingLength", asset_get_remaining as usize);
    m.insert("AAsset_getRemainingLength64", asset_get_remaining as usize);
    m.insert("AAsset_read", asset_read as usize);
    m.insert("AAsset_seek", asset_seek as usize);
    m.insert("AAsset_seek64", asset_seek as usize);
    m.insert("AAsset_openFileDescriptor", asset_open_fd as usize);
    m.insert("AAsset_openFileDescriptor64", asset_open_fd as usize);
    m.insert("AAsset_isAllocated", asset_is_allocated as usize);
}
