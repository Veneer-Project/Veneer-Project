//! PulseAudio's simple API (PipeWire provides it too), loaded at run time so
//! there is no build dependency. Blocking playback and recording streams of
//! interleaved signed 16-bit samples.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::OnceLock;

/// The application name sound settings show for Veneer's streams.
const APP_NAME: &CStr = c"Veneer";

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

const PA_SAMPLE_S16LE: c_int = 3;
const PA_STREAM_PLAYBACK: c_int = 1;
const PA_STREAM_RECORD: c_int = 2;
/// "Use the server's default" in a buffer attribute.
const DEFAULT: u32 = u32::MAX;

type New = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    c_int,
    *const c_char,
    *const c_char,
    *const SampleSpec,
    *const c_void,
    *const BufferAttr,
    *mut c_int,
) -> *mut c_void;
type Transfer = unsafe extern "C" fn(*mut c_void, *const c_void, usize, *mut c_int) -> c_int;
type Read = unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_int) -> c_int;

struct Api {
    new: New,
    write: Transfer,
    read: Read,
    free: unsafe extern "C" fn(*mut c_void),
}

fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let lib = libc::dlopen(c"libpulse-simple.so.0".as_ptr(), libc::RTLD_NOW);
        if lib.is_null() {
            log::warn!("libpulse-simple.so.0 not found: no audio");
            return None;
        }
        let sym = |name: &CStr| {
            let p = libc::dlsym(lib, name.as_ptr());
            (!p.is_null()).then_some(p)
        };
        Some(Api {
            new: std::mem::transmute::<*mut c_void, New>(sym(c"pa_simple_new")?),
            write: std::mem::transmute::<*mut c_void, Transfer>(sym(c"pa_simple_write")?),
            read: std::mem::transmute::<*mut c_void, Read>(sym(c"pa_simple_read")?),
            free: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void)>(sym(c"pa_simple_free")?),
        })
    })
    .as_ref()
    .ok_or_else(|| "PulseAudio (libpulse-simple.so.0) is not available".to_string())
}

/// An open playback or recording stream.
pub struct Stream {
    raw: *mut c_void,
    api: &'static Api,
}

// A stream is used by one thread at a time (callers keep it behind a lock
// or on one thread); libpulse-simple has no thread affinity.
unsafe impl Send for Stream {}
unsafe impl Sync for Stream {}

impl Stream {
    fn open(kind: c_int, name: &CStr, rate: u32, channels: u8, attr: BufferAttr) -> Result<Stream, String> {
        let api = api()?;
        let spec = SampleSpec { format: PA_SAMPLE_S16LE, rate, channels };
        let mut err = 0;
        let raw = unsafe {
            (api.new)(
                std::ptr::null(),
                APP_NAME.as_ptr(),
                kind,
                std::ptr::null(),
                name.as_ptr(),
                &spec,
                std::ptr::null(),
                &attr,
                &mut err,
            )
        };
        if raw.is_null() {
            return Err(format!("PulseAudio error {err}"));
        }
        Ok(Stream { raw, api })
    }

    /// A playback stream that keeps about `target_bytes` queued.
    pub fn playback(name: &CStr, rate: u32, channels: u8, target_bytes: u32) -> Result<Stream, String> {
        let attr = BufferAttr {
            maxlength: DEFAULT,
            tlength: target_bytes,
            prebuf: DEFAULT,
            minreq: DEFAULT,
            fragsize: DEFAULT,
        };
        Stream::open(PA_STREAM_PLAYBACK, name, rate, channels, attr)
    }

    /// A recording stream from the default input, delivered in pieces of
    /// about `fragment_bytes`.
    pub fn record(name: &CStr, rate: u32, channels: u8, fragment_bytes: u32) -> Result<Stream, String> {
        let attr = BufferAttr {
            maxlength: DEFAULT,
            tlength: DEFAULT,
            prebuf: DEFAULT,
            minreq: DEFAULT,
            fragsize: fragment_bytes,
        };
        Stream::open(PA_STREAM_RECORD, name, rate, channels, attr)
    }

    /// Play `data` (blocks while the server's buffer is full).
    pub fn write(&self, data: &[u8]) -> bool {
        let mut err = 0;
        unsafe { (self.api.write)(self.raw, data.as_ptr().cast(), data.len(), &mut err) == 0 }
    }

    /// Fill `data` with recorded samples (blocks until there are enough).
    pub fn read(&self, data: &mut [u8]) -> bool {
        let mut err = 0;
        unsafe { (self.api.read)(self.raw, data.as_mut_ptr().cast(), data.len(), &mut err) == 0 }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe { (self.api.free)(self.raw) };
    }
}
