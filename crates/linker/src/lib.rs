//! A minimal dynamic linker for Android (bionic) x86-64 shared objects,
//! running inside a glibc host process.
//!
//! The linker keeps one flat global namespace made of *libraries*:
//!  * guest images (real ELF files mapped from the APK), and
//!  * virtual libraries (symbol tables provided by the host, e.g. our
//!    `libc.so` shim or `libandroid.so` reimplementation).

pub mod elf;
mod image;
pub mod stubs;

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::Path;
use std::sync::RwLock;

pub use image::{Image, LoadError};

/// A host-provided symbol resolver for a virtual library.
pub type Resolver = Box<dyn Fn(&str) -> Option<usize> + Send + Sync>;

pub enum Library {
    Guest(&'static Image),
    Virtual { name: String, resolve: Resolver },
}

impl Library {
    pub fn name(&self) -> &str {
        match self {
            Library::Guest(i) => &i.name,
            Library::Virtual { name, .. } => name,
        }
    }

    pub fn symbol(&self, name: &str) -> Option<usize> {
        match self {
            Library::Guest(i) => i.symbol(name),
            Library::Virtual { resolve, .. } => resolve(name),
        }
    }
}

static LIBRARIES: RwLock<Vec<&'static Library>> = RwLock::new(Vec::new());

/// Register a host-implemented library under an Android soname.
pub fn register_virtual(name: &str, resolve: Resolver) {
    let lib = Box::leak(Box::new(Library::Virtual {
        name: name.to_string(),
        resolve,
    }));
    LIBRARIES.write().unwrap().push(lib);
}

/// Register a virtual library backed by a fixed symbol table.
pub fn register_table(name: &str, table: HashMap<&'static str, usize>) {
    register_virtual(name, Box::new(move |s| table.get(s).copied()));
}

pub fn libraries() -> Vec<&'static Library> {
    LIBRARIES.read().unwrap().clone()
}

pub fn images() -> Vec<&'static Image> {
    LIBRARIES
        .read()
        .unwrap()
        .iter()
        .filter_map(|l| match l {
            Library::Guest(i) => Some(*i),
            _ => None,
        })
        .collect()
}

fn base_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

pub fn find_library(name: &str) -> Option<&'static Library> {
    let want = base_name(name);
    LIBRARIES
        .read()
        .unwrap()
        .iter()
        .copied()
        .find(|l| base_name(l.name()) == want)
}

/// Look a symbol up in every library, in registration order.
pub fn resolve_global(name: &str) -> Option<usize> {
    for lib in LIBRARIES.read().unwrap().iter() {
        if let Some(a) = lib.symbol(name) {
            return Some(a);
        }
    }
    None
}

/// Map and relocate a guest shared object. Constructors are *not* run;
/// call [`Image::run_init`] once everything it depends on is in place.
pub fn load(path: &Path) -> Result<&'static Image, LoadError> {
    let img = image::load(path)?;
    let img: &'static Image = Box::leak(Box::new(img));
    LIBRARIES
        .write()
        .unwrap()
        .push(Box::leak(Box::new(Library::Guest(img))));
    Ok(img)
}

pub fn image_for_addr(addr: usize) -> Option<&'static Image> {
    images().into_iter().find(|i| i.contains(addr))
}

/// Describe an address as `lib+0xoff (symbol+0xoff)` for diagnostics.
pub fn describe_addr(addr: usize) -> String {
    match image_for_addr(addr) {
        Some(img) => match img.nearest_symbol(addr) {
            Some((name, sym)) => {
                format!("{}+{:#x} ({}+{:#x})", img.name, addr - img.bias, name, addr - sym)
            }
            None => format!("{}+{:#x}", img.name, addr - img.bias),
        },
        None => format!("{addr:#x}"),
    }
}

// ---------------------------------------------------------------------------
// The guest-facing `libdl` API.
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct DlInfo {
    pub dli_fname: *const c_char,
    pub dli_fbase: *mut c_void,
    pub dli_sname: *const c_char,
    pub dli_saddr: *mut c_void,
}

#[repr(C)]
pub struct DlPhdrInfo {
    pub dlpi_addr: u64,
    pub dlpi_name: *const c_char,
    pub dlpi_phdr: *const elf::Phdr,
    pub dlpi_phnum: u16,
    pub dlpi_adds: u64,
    pub dlpi_subs: u64,
    pub dlpi_tls_modid: usize,
    pub dlpi_tls_data: *mut c_void,
}

const RTLD_NEXT: usize = usize::MAX; // bionic LP64: ((void*) -1L)

pub unsafe extern "C" fn dlopen(name: *const c_char, _flags: c_int) -> *mut c_void {
    if name.is_null() {
        // Handle for the "main program": resolved through the global namespace.
        return std::ptr::null_mut();
    }
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    match find_library(&name) {
        Some(lib) => {
            log::debug!("dlopen({name}) -> ok");
            lib as *const Library as *mut c_void
        }
        None => {
            log::warn!("dlopen({name}) -> not available");
            std::ptr::null_mut()
        }
    }
}

pub unsafe extern "C" fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void {
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    let found = if handle.is_null() || handle as usize == RTLD_NEXT {
        resolve_global(&name)
    } else {
        let lib = unsafe { &*(handle as *const Library) };
        lib.symbol(&name)
    };
    match found {
        Some(a) => a as *mut c_void,
        None => {
            log::debug!("dlsym({name}) -> not found");
            std::ptr::null_mut()
        }
    }
}

pub unsafe extern "C" fn dlclose(_handle: *mut c_void) -> c_int {
    0
}

pub unsafe extern "C" fn dlerror() -> *const c_char {
    std::ptr::null()
}

pub unsafe extern "C" fn dladdr(addr: *const c_void, info: *mut DlInfo) -> c_int {
    let Some(img) = image_for_addr(addr as usize) else {
        return 0;
    };
    let info = unsafe { &mut *info };
    info.dli_fname = img.c_name.as_ptr();
    info.dli_fbase = img.map_start as *mut c_void;
    match img.nearest_symbol_raw(addr as usize) {
        Some((name, saddr)) => {
            info.dli_sname = name;
            info.dli_saddr = saddr as *mut c_void;
        }
        None => {
            info.dli_sname = std::ptr::null();
            info.dli_saddr = std::ptr::null_mut();
        }
    }
    1
}

pub type PhdrCallback =
    unsafe extern "C" fn(info: *mut DlPhdrInfo, size: usize, data: *mut c_void) -> c_int;

pub unsafe extern "C" fn dl_iterate_phdr(cb: PhdrCallback, data: *mut c_void) -> c_int {
    let imgs = images();
    for (n, img) in imgs.iter().enumerate() {
        let mut info = DlPhdrInfo {
            dlpi_addr: img.bias as u64,
            dlpi_name: img.c_name.as_ptr(),
            dlpi_phdr: img.phdr,
            dlpi_phnum: img.phnum as u16,
            dlpi_adds: imgs.len() as u64,
            dlpi_subs: 0,
            dlpi_tls_modid: 0,
            dlpi_tls_data: std::ptr::null_mut(),
        };
        let _ = n;
        let r = unsafe { cb(&mut info, size_of::<DlPhdrInfo>(), data) };
        if r != 0 {
            return r;
        }
    }
    0
}

/// Symbols exported as `libdl.so` (bionic also re-exports them from libc).
pub fn dl_symbols() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    m.insert("dlopen", dlopen as usize);
    m.insert("dlsym", dlsym as usize);
    m.insert("dlclose", dlclose as usize);
    m.insert("dlerror", dlerror as usize);
    m.insert("dladdr", dladdr as usize);
    m.insert("dl_iterate_phdr", dl_iterate_phdr as usize);
    m
}
