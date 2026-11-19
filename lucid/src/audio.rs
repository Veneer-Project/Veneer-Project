//! Audio output: FMOD's Java `AudioDevice` (an `AudioTrack` on Android)
//! implemented on PulseAudio's simple API, which PipeWire also provides.
//! The library is loaded at run time so there is no build dependency.

use std::ffi::{CString, c_char, c_int, c_void};
use std::sync::OnceLock;

use lucid_jni::{JValue, Ref, vm};

const DEVICE: &str = "org/fmod/AudioDevice";

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

struct Pulse {
    new: unsafe extern "C" fn(
        *const c_char,
        *const c_char,
        c_int,
        *const c_char,
        *const c_char,
        *const SampleSpec,
        *const c_void,
        *const BufferAttr,
        *mut c_int,
    ) -> *mut c_void,
    write: unsafe extern "C" fn(*mut c_void, *const c_void, usize, *mut c_int) -> c_int,
    free: unsafe extern "C" fn(*mut c_void),
}

fn pulse() -> Option<&'static Pulse> {
    static PULSE: OnceLock<Option<Pulse>> = OnceLock::new();
    PULSE
        .get_or_init(|| unsafe {
            let lib = libc::dlopen(c"libpulse-simple.so.0".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                log::warn!("libpulse-simple.so.0 not found: no audio output");
                return None;
            }
            let sym = |name: &str| {
                let c = CString::new(name).unwrap();
                libc::dlsym(lib, c.as_ptr())
            };
            let (new, write, free) = (sym("pa_simple_new"), sym("pa_simple_write"), sym("pa_simple_free"));
            if new.is_null() || write.is_null() || free.is_null() {
                return None;
            }
            Some(Pulse {
                new: std::mem::transmute::<*mut c_void, _>(new),
                write: std::mem::transmute::<*mut c_void, _>(write),
                free: std::mem::transmute::<*mut c_void, _>(free),
            })
        })
        .as_ref()
}

/// The open stream attached to an `AudioDevice` object.
struct Stream(usize);

fn stream_of(this: Option<&Ref>) -> Option<usize> {
    let inst = this?.as_instance()?;
    let data = inst.data.lock().unwrap();
    data.as_ref()?.downcast_ref::<Stream>().map(|s| s.0)
}

pub fn install() {
    let vm = vm();
    vm.on(DEVICE, "<init>", "()V", |_, _| ());
    // init(channels, sampleRate, blockSize, blockCount)
    vm.on(DEVICE, "init", "(IIII)Z", |this, a| {
        let (Some(p), Some(inst)) = (pulse(), this.and_then(|t| t.as_instance())) else {
            return false;
        };
        let channels = a[0].as_i64().clamp(1, 8) as u8;
        let rate = a[1].as_i64() as u32;
        let block_bytes = (a[2].as_i64().max(1) * channels as i64 * 2) as u32;
        let spec = SampleSpec {
            format: PA_SAMPLE_S16LE,
            rate,
            channels,
        };
        // Keep latency near FMOD's own buffering (a few mixer blocks).
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: block_bytes * a[3].as_i64().clamp(2, 8) as u32,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: u32::MAX,
        };
        let mut err = 0;
        let s = unsafe {
            (p.new)(
                std::ptr::null(),
                c"Roblox".as_ptr(),
                PA_STREAM_PLAYBACK,
                std::ptr::null(),
                c"Game audio".as_ptr(),
                &spec,
                std::ptr::null(),
                &attr,
                &mut err,
            )
        };
        if s.is_null() {
            log::warn!("cannot open audio output (pulse error {err})");
            return false;
        }
        log::info!("audio output: {channels} channels at {rate} Hz");
        *inst.data.lock().unwrap() = Some(Box::new(Stream(s as usize)));
        true
    });
    // write(byte[] pcm, int length): blocking, interleaved 16-bit samples.
    vm.on(DEVICE, "write", "([BI)V", |this, a| {
        let (Some(p), Some(s)) = (pulse(), stream_of(this)) else {
            return;
        };
        let Some(arr) = a.first().and_then(JValue::as_object).and_then(|o| o.as_prim_array()) else {
            return;
        };
        let len = (a[1].as_i64().max(0) as usize).min(arr.byte_len());
        let mut err = 0;
        unsafe { (p.write)(s as *mut c_void, arr.as_ptr() as *const c_void, len, &mut err) };
    });
    vm.on(DEVICE, "close", "()V", |this, _| {
        let Some(inst) = this.and_then(|t| t.as_instance()) else {
            return;
        };
        let taken = inst.data.lock().unwrap().take();
        if let (Some(p), Some(s)) = (pulse(), taken.and_then(|d| d.downcast::<Stream>().ok())) {
            unsafe { (p.free)(s.0 as *mut c_void) };
        }
    });
}
