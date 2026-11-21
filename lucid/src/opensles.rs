//! `libOpenSLES.so`: the part of OpenSL ES that FMOD (Roblox's sound engine)
//! uses on Android, on top of PulseAudio. FMOD plays all game sound through
//! it and records the microphone for voice chat.
//!
//! FMOD creates an engine, an output mix, an audio player and, for voice,
//! an audio recorder, each fed through an Android "simple buffer queue": it
//! enqueues buffers of 16-bit PCM and gets a callback when one was played
//! (refill it) or recorded (read it). Here each player and recorder has a
//! worker thread that moves those buffers to or from a PulseAudio stream.
//! The microphone is only open while a recorder is recording, and opening
//! and closing it is announced in the terminal.
//!
//! OpenSL ES objects and interfaces are pointers to pointers to tables of
//! functions. Each object here is one heap block whose fields hold those
//! table pointers; an interface handle is the address of its field, and a
//! method finds its object again from that address.

use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, c_char, c_void};
use std::mem::offset_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;

use crate::pulse;

type SLresult = u32;
const SUCCESS: SLresult = 0;
const PARAMETER_INVALID: SLresult = 2;
const BUFFER_INSUFFICIENT: SLresult = 7;
const CONTENT_UNSUPPORTED: SLresult = 9;
const FEATURE_UNSUPPORTED: SLresult = 12;

const OBJECT_STATE_UNREALIZED: u32 = 1;
const OBJECT_STATE_REALIZED: u32 = 2;
/// Play and record states share their values.
const STATE_STOPPED: u32 = 1;
const STATE_PAUSED: u32 = 2;
const STATE_ACTIVE: u32 = 3;

const DATALOCATOR_IODEVICE: u32 = 3;
const DATALOCATOR_OUTPUTMIX: u32 = 4;
const DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE: u32 = 0x8000_07BD;
const DATAFORMAT_PCM: u32 = 2;

// --- interface IDs -------------------------------------------------------------
//
// `SL_IID_*` are exported pointer variables that the guest only hands back
// to this implementation, so the 16 bytes only need to differ.

#[repr(C)]
struct Iid([u8; 16]);
/// The exported variable: the guest reads the pointer through its symbol.
#[allow(dead_code)]
struct IidVar(*const Iid);
unsafe impl Sync for IidVar {}

static IID_ENGINE: Iid = Iid(*b"veneer-sl-engine");
static IID_PLAY: Iid = Iid(*b"veneer-sl-play\0\0");
static IID_RECORD: Iid = Iid(*b"veneer-sl-record");
static IID_ANDROIDSIMPLEBUFFERQUEUE: Iid = Iid(*b"veneer-sl-queue\0");
static IID_ANDROIDCONFIGURATION: Iid = Iid(*b"veneer-sl-config");

static SL_IID_ENGINE: IidVar = IidVar(&IID_ENGINE);
static SL_IID_PLAY: IidVar = IidVar(&IID_PLAY);
static SL_IID_RECORD: IidVar = IidVar(&IID_RECORD);
static SL_IID_ANDROIDSIMPLEBUFFERQUEUE: IidVar = IidVar(&IID_ANDROIDSIMPLEBUFFERQUEUE);
static SL_IID_ANDROIDCONFIGURATION: IidVar = IidVar(&IID_ANDROIDCONFIGURATION);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Interface {
    Engine,
    Play,
    Record,
    Queue,
    Config,
}

unsafe fn interface_of(id: *const Iid) -> Option<Interface> {
    if id.is_null() {
        return None;
    }
    let bytes = unsafe { &(*id).0 };
    [
        (&IID_ENGINE, Interface::Engine),
        (&IID_PLAY, Interface::Play),
        (&IID_RECORD, Interface::Record),
        (&IID_ANDROIDSIMPLEBUFFERQUEUE, Interface::Queue),
        (&IID_ANDROIDCONFIGURATION, Interface::Config),
    ]
    .into_iter()
    .find(|(iid, _)| iid.0 == *bytes)
    .map(|(_, i)| i)
}

// --- audio devices ---------------------------------------------------------------

/// Where a player's samples go or a recorder's come from.
pub trait Device: Send {
    /// Play `buf`, or fill it with recorded samples; false on failure.
    fn transfer(&mut self, buf: &mut [u8]) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Format {
    pub rate: u32,
    pub channels: u8,
}

/// Opens a device for a player (`false`) or recorder (`true`) and the
/// size of one buffer in bytes.
pub type Opener = fn(recorder: bool, format: Format, buffer_bytes: usize) -> Result<Box<dyn Device>, String>;

struct Pulse {
    stream: pulse::Stream,
    recorder: bool,
}

impl Device for Pulse {
    fn transfer(&mut self, buf: &mut [u8]) -> bool {
        if self.recorder { self.stream.read(buf) } else { self.stream.write(buf) }
    }
}

impl Drop for Pulse {
    fn drop(&mut self) {
        if self.recorder {
            log::info!("microphone closed");
        }
    }
}

fn open_pulse(recorder: bool, f: Format, buffer_bytes: usize) -> Result<Box<dyn Device>, String> {
    let stream = if recorder {
        pulse::Stream::record(c"Voice chat microphone", f.rate, f.channels, buffer_bytes as u32)?
    } else {
        // A few buffers of output keeps playback smooth at low latency.
        pulse::Stream::playback(c"Game audio", f.rate, f.channels, buffer_bytes as u32 * 4)?
    };
    if recorder {
        log::info!("microphone opened (voice chat), {} Hz", f.rate);
    } else {
        log::info!("audio output: {} channels at {} Hz", f.channels, f.rate);
    }
    Ok(Box::new(Pulse { stream, recorder }))
}

// --- players and recorders ---------------------------------------------------------

type QueueCallback = unsafe extern "C" fn(*const c_void, *mut c_void);

struct QueueState {
    /// Enqueued buffers not yet played / filled: (address, length).
    buffers: VecDeque<(usize, usize)>,
    /// How many buffers have been completed (the queue's `index`).
    done: u32,
    /// Bytes played or recorded since the object was created.
    bytes: u64,
    callback: Option<(QueueCallback, usize)>,
    /// Play / record state.
    state: u32,
    stop: bool,
    worker: Option<JoinHandle<()>>,
    worker_id: Option<std::thread::ThreadId>,
}

/// The shared part of a player or recorder (the worker thread holds it too).
struct Stream {
    recorder: bool,
    format: Format,
    capacity: usize,
    open: Opener,
    /// Address of the object's queue interface (the callback's first argument).
    queue_itf: AtomicUsize,
    inner: Mutex<QueueState>,
    wake: Condvar,
}

impl Stream {
    fn set_state(self: &Arc<Self>, state: u32) -> SLresult {
        if !matches!(state, STATE_STOPPED | STATE_PAUSED | STATE_ACTIVE) {
            return PARAMETER_INVALID;
        }
        let old = {
            let mut q = self.inner.lock().unwrap();
            std::mem::replace(&mut q.state, state)
        };
        match (old == STATE_ACTIVE, state == STATE_ACTIVE) {
            (false, true) => self.start(),
            (true, false) => self.halt(),
            _ => {}
        }
        SUCCESS
    }

    fn start(self: &Arc<Self>) {
        let me = self.clone();
        let name = if self.recorder { "OpenSL recorder" } else { "OpenSL player" };
        let mut q = self.inner.lock().unwrap();
        q.stop = false;
        let handle = std::thread::Builder::new().name(name.into()).spawn(move || me.run());
        match handle {
            Ok(h) => {
                q.worker_id = Some(h.thread().id());
                q.worker = Some(h);
            }
            Err(e) => log::warn!("OpenSL ES: cannot start a worker thread: {e}"),
        }
    }

    /// Stop the worker and wait for it (unless called from it, e.g. from a
    /// buffer callback).
    fn halt(&self) {
        let worker = {
            let mut q = self.inner.lock().unwrap();
            q.stop = true;
            if q.worker_id == Some(std::thread::current().id()) {
                None
            } else {
                q.worker_id = None;
                q.worker.take()
            }
        };
        self.wake.notify_all();
        if let Some(w) = worker {
            let _ = w.join();
        }
    }

    fn run(&self) {
        let one = self.inner.lock().unwrap().buffers.front().map_or(0, |b| b.1);
        // Buffers are normally all the same size; open with the first one's.
        let mut device = match (self.open)(self.recorder, self.format, one.max(480)) {
            Ok(d) => Some(d),
            Err(e) => {
                let what = if self.recorder { "microphone" } else { "audio output" };
                log::warn!("cannot open the {what}: {e}");
                None
            }
        };
        loop {
            let (addr, len) = {
                let mut q = self.inner.lock().unwrap();
                loop {
                    if q.stop {
                        return;
                    }
                    if let Some(&b) = q.buffers.front() {
                        break b;
                    }
                    q = self.wake.wait(q).unwrap();
                }
            };
            // The guest keeps the buffer alive until its callback.
            let buf = unsafe { std::slice::from_raw_parts_mut(addr as *mut u8, len) };
            let ok = device.as_mut().is_some_and(|d| d.transfer(buf));
            if !ok {
                if self.recorder {
                    buf.fill(0);
                }
                // Without a device, keep the pace of real time anyway.
                let frames = len / (2 * self.format.channels.max(1) as usize);
                std::thread::sleep(std::time::Duration::from_micros(
                    frames as u64 * 1_000_000 / self.format.rate.max(1) as u64,
                ));
            }
            let callback = {
                let mut q = self.inner.lock().unwrap();
                if q.stop {
                    return;
                }
                // `Clear` may have emptied the queue meanwhile.
                if q.buffers.front() == Some(&(addr, len)) {
                    q.buffers.pop_front();
                    q.done = q.done.wrapping_add(1);
                    q.bytes += len as u64;
                }
                q.callback
            };
            if let Some((f, ctx)) = callback {
                unsafe { f(self.queue_itf.load(Ordering::Relaxed) as *const c_void, ctx as *mut c_void) };
            }
        }
    }
}

// --- objects ---------------------------------------------------------------------

enum Kind {
    Engine { open: Opener },
    OutputMix,
    Player(Arc<Stream>),
    Recorder(Arc<Stream>),
}

impl Kind {
    fn supports(&self, i: Interface) -> bool {
        matches!(
            (self, i),
            (Kind::Engine { .. }, Interface::Engine)
                | (Kind::Player(_), Interface::Play | Interface::Queue | Interface::Config)
                | (Kind::Recorder(_), Interface::Record | Interface::Queue | Interface::Config)
        )
    }
}

/// One OpenSL ES object. The `*_itf` fields are what interface handles
/// point at.
#[repr(C)]
struct Object {
    object_itf: *const usize,
    engine_itf: *const usize,
    play_itf: *const usize,
    record_itf: *const usize,
    queue_itf: *const usize,
    config_itf: *const usize,
    realized: bool,
    kind: Kind,
    config: Mutex<HashMap<String, Vec<u8>>>,
}

impl Object {
    fn new(kind: Kind) -> *mut Object {
        let t = tables();
        let b = Box::new(Object {
            object_itf: t.object.as_ptr(),
            engine_itf: t.engine.as_ptr(),
            play_itf: t.play.as_ptr(),
            record_itf: t.record.as_ptr(),
            queue_itf: t.queue.as_ptr(),
            config_itf: t.config.as_ptr(),
            realized: false,
            kind,
            config: Mutex::new(HashMap::new()),
        });
        Box::into_raw(b)
    }

    fn supports(&self, i: Interface) -> bool {
        self.kind.supports(i)
    }

    fn interface(&self, i: Interface) -> *const c_void {
        let field: *const *const usize = match i {
            Interface::Engine => &self.engine_itf,
            Interface::Play => &self.play_itf,
            Interface::Record => &self.record_itf,
            Interface::Queue => &self.queue_itf,
            Interface::Config => &self.config_itf,
        };
        field.cast()
    }

    fn stream(&self) -> Option<&Arc<Stream>> {
        match &self.kind {
            Kind::Player(s) | Kind::Recorder(s) => Some(s),
            _ => None,
        }
    }
}

/// The object an interface handle belongs to.
macro_rules! object_of {
    ($itf:expr, $field:ident) => {
        unsafe { &*(($itf as usize - offset_of!(Object, $field)) as *const Object) }
    };
}

// --- method tables ---------------------------------------------------------------

struct Tables {
    object: Vec<usize>,
    engine: Vec<usize>,
    play: Vec<usize>,
    record: Vec<usize>,
    queue: Vec<usize>,
    config: Vec<usize>,
}

/// Any method this implementation does not provide (extra arguments are
/// harmless in the C calling convention). Says which, once per method.
unsafe extern "C" fn unsupported<const TABLE: u8, const SLOT: usize>() -> SLresult {
    static SAID: Mutex<Vec<(u8, usize)>> = Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap();
    if !said.contains(&(TABLE, SLOT)) {
        said.push((TABLE, SLOT));
        let table = ["object", "engine", "play", "record", "buffer queue", "configuration"][TABLE as usize];
        log::warn!("OpenSL ES: unsupported {table} method (slot {SLOT}) called");
    }
    FEATURE_UNSUPPORTED
}

/// A method that only has to succeed.
unsafe extern "C" fn no_op() -> SLresult {
    SUCCESS
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let ok = no_op as usize;
        macro_rules! u {
            ($t:literal, $n:literal) => {
                unsupported::<$t, $n> as usize
            };
        }
        Tables {
            // Realize, Resume, GetState, GetInterface, RegisterCallback,
            // AbortAsyncOperation, Destroy, SetPriority, GetPriority,
            // SetLossOfControlInterfaces
            object: vec![
                object_realize as usize, ok, object_get_state as usize, object_get_interface as usize,
                ok, ok, object_destroy as usize, ok, u!(0, 8), ok,
            ],
            // CreateLEDDevice, CreateVibraDevice, CreateAudioPlayer,
            // CreateAudioRecorder, CreateMidiPlayer, CreateListener,
            // Create3DGroup, CreateOutputMix, CreateMetadataExtractor,
            // CreateExtensionObject, QueryNumSupportedInterfaces,
            // QuerySupportedInterfaces, QueryNumSupportedExtensions,
            // QuerySupportedExtension, IsExtensionSupported
            engine: vec![
                u!(1, 0), u!(1, 1), engine_create_player as usize, engine_create_recorder as usize, u!(1, 4),
                u!(1, 5), u!(1, 6), engine_create_output_mix as usize, u!(1, 8), u!(1, 9), u!(1, 10),
                u!(1, 11), u!(1, 12), u!(1, 13), u!(1, 14),
            ],
            // SetPlayState, GetPlayState, GetDuration, GetPosition,
            // RegisterCallback, SetCallbackEventsMask, GetCallbackEventsMask,
            // SetMarkerPosition, ClearMarkerPosition, GetMarkerPosition,
            // SetPositionUpdatePeriod, GetPositionUpdatePeriod
            play: vec![
                play_set_state as usize, play_get_state as usize, u!(2, 2), play_get_position as usize, ok, ok,
                u!(2, 6), u!(2, 7), ok, u!(2, 9), ok, u!(2, 11),
            ],
            // SetRecordState, GetRecordState, SetDurationLimit, GetPosition,
            // then the same callback / marker methods.
            record: vec![
                record_set_state as usize, record_get_state as usize, ok, record_get_position as usize, ok, ok,
                u!(3, 6), u!(3, 7), ok, u!(3, 9), ok, u!(3, 11),
            ],
            // Enqueue, Clear, GetState, RegisterCallback
            queue: vec![queue_enqueue as usize, queue_clear as usize, queue_get_state as usize, queue_register as usize],
            // SetConfiguration, GetConfiguration, AcquireJavaProxy, ReleaseJavaProxy
            config: vec![config_set as usize, config_get as usize, u!(5, 2), u!(5, 3)],
        }
    })
}

// --- SLObjectItf -------------------------------------------------------------------

unsafe extern "C" fn object_realize(itf: *const c_void, _async: u32) -> SLresult {
    let obj = (itf as usize - offset_of!(Object, object_itf)) as *mut Object;
    unsafe { (*obj).realized = true };
    SUCCESS
}

unsafe extern "C" fn object_get_state(itf: *const c_void, state: *mut u32) -> SLresult {
    let obj = object_of!(itf, object_itf);
    if state.is_null() {
        return PARAMETER_INVALID;
    }
    unsafe { *state = if obj.realized { OBJECT_STATE_REALIZED } else { OBJECT_STATE_UNREALIZED } };
    SUCCESS
}

unsafe extern "C" fn object_get_interface(itf: *const c_void, iid: *const Iid, out: *mut *const c_void) -> SLresult {
    let obj = object_of!(itf, object_itf);
    if out.is_null() {
        return PARAMETER_INVALID;
    }
    // Android also hands out interfaces before Realize (the configuration
    // interface is used then, to set the stream type or preset).
    match unsafe { interface_of(iid) } {
        Some(i) if obj.supports(i) => {
            unsafe { *out = obj.interface(i) };
            SUCCESS
        }
        other => {
            let what = match &obj.kind {
                Kind::Engine { .. } => "engine",
                Kind::OutputMix => "output mix",
                Kind::Player(_) => "player",
                Kind::Recorder(_) => "recorder",
            };
            log::warn!("OpenSL ES: the {what} has no {other:?} interface (asked for {:?})", unsafe { iid.as_ref() }.map(|i| String::from_utf8_lossy(&i.0).into_owned()));
            FEATURE_UNSUPPORTED
        }
    }
}

unsafe extern "C" fn object_destroy(itf: *const c_void) {
    let obj = (itf as usize - offset_of!(Object, object_itf)) as *mut Object;
    let obj = unsafe { Box::from_raw(obj) };
    if let Some(s) = obj.stream() {
        s.halt();
    }
}

// --- SLEngineItf -------------------------------------------------------------------

#[repr(C)]
struct DataSource {
    locator: *const u32,
    format: *const u32,
}

#[repr(C)]
struct BufferQueueLocator {
    locator_type: u32,
    num_buffers: u32,
}

#[repr(C)]
struct PcmFormat {
    format_type: u32,
    num_channels: u32,
    /// In milliHertz.
    samples_per_sec: u32,
    bits_per_sample: u32,
    container_size: u32,
    channel_mask: u32,
    endianness: u32,
}

/// The buffer queue locator and PCM format of one side of a player or
/// recorder: (buffer count, format).
unsafe fn queue_side(side: *const DataSource) -> Result<(usize, Format), SLresult> {
    let side = unsafe { side.as_ref() }.ok_or(PARAMETER_INVALID)?;
    let locator = unsafe { (side.locator as *const BufferQueueLocator).as_ref() }.ok_or(PARAMETER_INVALID)?;
    let format = unsafe { (side.format as *const PcmFormat).as_ref() }.ok_or(PARAMETER_INVALID)?;
    if locator.locator_type != DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE
        || format.format_type != DATAFORMAT_PCM
        || format.bits_per_sample != 16
        || !(1..=2).contains(&format.num_channels)
    {
        log::warn!(
            "OpenSL ES: unsupported stream (locator {:#x}, format type {}, {} bits, {} channels, {} mHz)",
            locator.locator_type,
            format.format_type,
            format.bits_per_sample,
            format.num_channels,
            format.samples_per_sec
        );
        return Err(CONTENT_UNSUPPORTED);
    }
    let format = Format { rate: format.samples_per_sec / 1000, channels: format.num_channels as u8 };
    Ok((locator.num_buffers.max(1) as usize, format))
}

unsafe fn locator_type(side: *const DataSource) -> Option<u32> {
    let side = unsafe { side.as_ref() }?;
    unsafe { side.locator.as_ref() }.copied()
}

/// Check the interfaces the guest requires against what `kind` offers.
unsafe fn check_required(kind: &Kind, count: u32, ids: *const *const Iid, required: *const u32) -> SLresult {
    for i in 0..count as usize {
        let id = if ids.is_null() { std::ptr::null() } else { unsafe { *ids.add(i) } };
        let needed = !required.is_null() && unsafe { *required.add(i) } != 0;
        let supported = unsafe { interface_of(id) }.is_some_and(|i| kind.supports(i));
        if needed && !supported {
            let bytes = if id.is_null() { [0; 16] } else { unsafe { (*id).0 } };
            log::warn!("OpenSL ES: a required interface is not supported ({})", String::from_utf8_lossy(&bytes));
            return FEATURE_UNSUPPORTED;
        }
    }
    SUCCESS
}

fn new_stream(recorder: bool, format: Format, capacity: usize, open: Opener) -> Arc<Stream> {
    Arc::new(Stream {
        recorder,
        format,
        capacity,
        open,
        queue_itf: AtomicUsize::new(0),
        inner: Mutex::new(QueueState {
            buffers: VecDeque::new(),
            done: 0,
            bytes: 0,
            callback: None,
            state: STATE_STOPPED,
            stop: false,
            worker: None,
            worker_id: None,
        }),
        wake: Condvar::new(),
    })
}

/// Create a player or recorder object around `stream` and return its handle.
unsafe fn finish(out: *mut *const c_void, stream: Arc<Stream>, recorder: bool) -> SLresult {
    let kind = if recorder { Kind::Recorder(stream) } else { Kind::Player(stream) };
    let obj = Object::new(kind);
    // The stream needs its object's queue handle for callbacks.
    let queue_itf = unsafe { (*obj).interface(Interface::Queue) } as usize;
    unsafe {
        (*obj).stream().unwrap().queue_itf.store(queue_itf, Ordering::Relaxed);
        *out = (*obj).interface_object();
    }
    SUCCESS
}

impl Object {
    fn interface_object(&self) -> *const c_void {
        (&self.object_itf as *const *const usize).cast()
    }
}

unsafe extern "C" fn engine_create_player(
    itf: *const c_void,
    out: *mut *const c_void,
    source: *const DataSource,
    sink: *const DataSource,
    count: u32,
    ids: *const *const Iid,
    required: *const u32,
) -> SLresult {
    let Kind::Engine { open } = object_of!(itf, engine_itf).kind else {
        return PARAMETER_INVALID;
    };
    if out.is_null() || unsafe { locator_type(sink) } != Some(DATALOCATOR_OUTPUTMIX) {
        return PARAMETER_INVALID;
    }
    let (capacity, format) = match unsafe { queue_side(source) } {
        Ok(v) => v,
        Err(e) => return e,
    };
    let stream = new_stream(false, format, capacity, open);
    let r = unsafe { check_required(&Kind::Player(stream.clone()), count, ids, required) };
    if r != SUCCESS {
        return r;
    }
    log::debug!("OpenSL ES: player, {} Hz, {} channel(s), {capacity} buffers", format.rate, format.channels);
    unsafe { finish(out, stream, false) }
}

unsafe extern "C" fn engine_create_recorder(
    itf: *const c_void,
    out: *mut *const c_void,
    source: *const DataSource,
    sink: *const DataSource,
    count: u32,
    ids: *const *const Iid,
    required: *const u32,
) -> SLresult {
    let Kind::Engine { open } = object_of!(itf, engine_itf).kind else {
        return PARAMETER_INVALID;
    };
    if out.is_null() || unsafe { locator_type(source) } != Some(DATALOCATOR_IODEVICE) {
        return PARAMETER_INVALID;
    }
    let (capacity, format) = match unsafe { queue_side(sink) } {
        Ok(v) => v,
        Err(e) => return e,
    };
    let stream = new_stream(true, format, capacity, open);
    let r = unsafe { check_required(&Kind::Recorder(stream.clone()), count, ids, required) };
    if r != SUCCESS {
        return r;
    }
    log::debug!("OpenSL ES: recorder, {} Hz, {} channel(s), {capacity} buffers", format.rate, format.channels);
    unsafe { finish(out, stream, true) }
}

unsafe extern "C" fn engine_create_output_mix(
    _itf: *const c_void,
    out: *mut *const c_void,
    count: u32,
    ids: *const *const Iid,
    required: *const u32,
) -> SLresult {
    if out.is_null() {
        return PARAMETER_INVALID;
    }
    let r = unsafe { check_required(&Kind::OutputMix, count, ids, required) };
    if r != SUCCESS {
        return r;
    }
    let obj = Object::new(Kind::OutputMix);
    unsafe { *out = (*obj).interface_object() };
    SUCCESS
}

// --- SLPlayItf / SLRecordItf -------------------------------------------------------

unsafe extern "C" fn play_set_state(itf: *const c_void, state: u32) -> SLresult {
    match object_of!(itf, play_itf).stream() {
        Some(s) => s.set_state(state),
        None => PARAMETER_INVALID,
    }
}

unsafe extern "C" fn play_get_state(itf: *const c_void, state: *mut u32) -> SLresult {
    match (object_of!(itf, play_itf).stream(), state.is_null()) {
        (Some(s), false) => {
            unsafe { *state = s.inner.lock().unwrap().state };
            SUCCESS
        }
        _ => PARAMETER_INVALID,
    }
}

/// Milliseconds of audio played / recorded so far.
fn position_ms(s: &Stream) -> u32 {
    let bytes = s.inner.lock().unwrap().bytes;
    let per_second = s.format.rate as u64 * s.format.channels.max(1) as u64 * 2;
    (bytes * 1000 / per_second.max(1)) as u32
}

unsafe extern "C" fn play_get_position(itf: *const c_void, ms: *mut u32) -> SLresult {
    match (object_of!(itf, play_itf).stream(), ms.is_null()) {
        (Some(s), false) => {
            unsafe { *ms = position_ms(s) };
            SUCCESS
        }
        _ => PARAMETER_INVALID,
    }
}

unsafe extern "C" fn record_get_position(itf: *const c_void, ms: *mut u32) -> SLresult {
    match (object_of!(itf, record_itf).stream(), ms.is_null()) {
        (Some(s), false) => {
            unsafe { *ms = position_ms(s) };
            SUCCESS
        }
        _ => PARAMETER_INVALID,
    }
}

unsafe extern "C" fn record_set_state(itf: *const c_void, state: u32) -> SLresult {
    match object_of!(itf, record_itf).stream() {
        Some(s) => s.set_state(state),
        None => PARAMETER_INVALID,
    }
}

unsafe extern "C" fn record_get_state(itf: *const c_void, state: *mut u32) -> SLresult {
    match (object_of!(itf, record_itf).stream(), state.is_null()) {
        (Some(s), false) => {
            unsafe { *state = s.inner.lock().unwrap().state };
            SUCCESS
        }
        _ => PARAMETER_INVALID,
    }
}

// --- SLAndroidSimpleBufferQueueItf ---------------------------------------------------

unsafe extern "C" fn queue_enqueue(itf: *const c_void, buf: *const c_void, size: u32) -> SLresult {
    let Some(s) = object_of!(itf, queue_itf).stream() else {
        return PARAMETER_INVALID;
    };
    if buf.is_null() || size == 0 {
        return PARAMETER_INVALID;
    }
    let mut q = s.inner.lock().unwrap();
    if q.buffers.len() >= s.capacity {
        return BUFFER_INSUFFICIENT;
    }
    q.buffers.push_back((buf as usize, size as usize));
    drop(q);
    s.wake.notify_all();
    SUCCESS
}

unsafe extern "C" fn queue_clear(itf: *const c_void) -> SLresult {
    let Some(s) = object_of!(itf, queue_itf).stream() else {
        return PARAMETER_INVALID;
    };
    s.inner.lock().unwrap().buffers.clear();
    SUCCESS
}

#[repr(C)]
struct QueueStatus {
    count: u32,
    index: u32,
}

unsafe extern "C" fn queue_get_state(itf: *const c_void, out: *mut QueueStatus) -> SLresult {
    let (Some(s), false) = (object_of!(itf, queue_itf).stream(), out.is_null()) else {
        return PARAMETER_INVALID;
    };
    let q = s.inner.lock().unwrap();
    unsafe { *out = QueueStatus { count: q.buffers.len() as u32, index: q.done } };
    SUCCESS
}

unsafe extern "C" fn queue_register(itf: *const c_void, callback: Option<QueueCallback>, ctx: *mut c_void) -> SLresult {
    let Some(s) = object_of!(itf, queue_itf).stream() else {
        return PARAMETER_INVALID;
    };
    s.inner.lock().unwrap().callback = callback.map(|f| (f, ctx as usize));
    SUCCESS
}

// --- SLAndroidConfigurationItf ---------------------------------------------------------

unsafe extern "C" fn config_set(itf: *const c_void, key: *const c_char, value: *const c_void, size: u32) -> SLresult {
    let obj = object_of!(itf, config_itf);
    if key.is_null() || (value.is_null() && size > 0) {
        return PARAMETER_INVALID;
    }
    let key = unsafe { CStr::from_ptr(key) }.to_string_lossy().into_owned();
    let bytes = if size == 0 { Vec::new() } else { unsafe { std::slice::from_raw_parts(value as *const u8, size as usize) }.to_vec() };
    log::debug!("OpenSL ES: configuration {key} = {bytes:?}");
    obj.config.lock().unwrap().insert(key, bytes);
    SUCCESS
}

unsafe extern "C" fn config_get(itf: *const c_void, key: *const c_char, size: *mut u32, value: *mut c_void) -> SLresult {
    let obj = object_of!(itf, config_itf);
    if key.is_null() || size.is_null() {
        return PARAMETER_INVALID;
    }
    let key = unsafe { CStr::from_ptr(key) }.to_string_lossy();
    let config = obj.config.lock().unwrap();
    let Some(bytes) = config.get(key.as_ref()) else {
        return PARAMETER_INVALID;
    };
    let room = unsafe { *size } as usize;
    unsafe { *size = bytes.len() as u32 };
    if !value.is_null() {
        if room < bytes.len() {
            return BUFFER_INSUFFICIENT;
        }
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), value as *mut u8, bytes.len()) };
    }
    SUCCESS
}

// --- slCreateEngine ------------------------------------------------------------------

fn create_engine(out: *mut *const c_void, open: Opener) -> SLresult {
    if out.is_null() {
        return PARAMETER_INVALID;
    }
    let obj = Object::new(Kind::Engine { open });
    unsafe { *out = (*obj).interface_object() };
    SUCCESS
}

unsafe extern "C" fn sl_create_engine(
    out: *mut *const c_void,
    _options: u32,
    _option_list: *const c_void,
    _count: u32,
    _ids: *const *const Iid,
    _required: *const u32,
) -> SLresult {
    log::debug!("OpenSL ES: engine created");
    create_engine(out, open_pulse)
}

/// Make `libOpenSLES.so` available to the guest.
pub fn install() {
    let mut m: HashMap<&'static str, usize> = HashMap::new();
    m.insert("slCreateEngine", sl_create_engine as usize);
    m.insert("SL_IID_ENGINE", &SL_IID_ENGINE as *const IidVar as usize);
    m.insert("SL_IID_PLAY", &SL_IID_PLAY as *const IidVar as usize);
    m.insert("SL_IID_RECORD", &SL_IID_RECORD as *const IidVar as usize);
    m.insert("SL_IID_ANDROIDSIMPLEBUFFERQUEUE", &SL_IID_ANDROIDSIMPLEBUFFERQUEUE as *const IidVar as usize);
    m.insert("SL_IID_ANDROIDCONFIGURATION", &SL_IID_ANDROIDCONFIGURATION as *const IidVar as usize);
    lucid_linker::register_table("libOpenSLES.so", m);
}

#[cfg(test)]
mod tests {
    //! Drive the implementation the way FMOD does, through the method
    //! tables, with a fake device instead of the speakers and microphone.
    use super::*;

    /// Plays into nowhere, records a counting pattern; 1 ms per buffer.
    struct Fake {
        recorder: bool,
        next: u8,
    }

    impl Device for Fake {
        fn transfer(&mut self, buf: &mut [u8]) -> bool {
            if self.recorder {
                buf.fill(self.next);
                self.next = self.next.wrapping_add(1);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
            true
        }
    }

    fn fake(recorder: bool, _: Format, _: usize) -> Result<Box<dyn Device>, String> {
        Ok(Box::new(Fake { recorder, next: 1 }))
    }

    /// Call slot `n` of the table behind interface handle `itf`.
    unsafe fn slot(itf: *const c_void, n: usize) -> usize {
        unsafe { *(*(itf as *const *const usize)).add(n) }
    }

    unsafe fn get_interface(obj: *const c_void, iid: &IidVar) -> *const c_void {
        let f: unsafe extern "C" fn(*const c_void, *const Iid, *mut *const c_void) -> SLresult =
            unsafe { std::mem::transmute(slot(obj, 3)) };
        let mut out = std::ptr::null();
        assert_eq!(unsafe { f(obj, iid.0, &mut out) }, SUCCESS);
        out
    }

    unsafe fn realize(obj: *const c_void) {
        let f: unsafe extern "C" fn(*const c_void, u32) -> SLresult = unsafe { std::mem::transmute(slot(obj, 0)) };
        assert_eq!(unsafe { f(obj, 0) }, SUCCESS);
    }

    unsafe fn destroy(obj: *const c_void) {
        let f: unsafe extern "C" fn(*const c_void) = unsafe { std::mem::transmute(slot(obj, 6)) };
        unsafe { f(obj) };
    }

    /// What a test's buffer callback sees.
    struct Seen {
        calls: AtomicUsize,
        queue: AtomicUsize,
        first_bytes: Mutex<Vec<u8>>,
        buffers: Mutex<Vec<Box<[u8; 960]>>>,
    }

    /// Like FMOD's callback: note the buffer just completed, enqueue the
    /// next one.
    unsafe extern "C" fn on_buffer(itf: *const c_void, ctx: *mut c_void) {
        let seen = unsafe { &*(ctx as *const Seen) };
        let n = seen.calls.fetch_add(1, Ordering::SeqCst);
        seen.queue.store(itf as usize, Ordering::SeqCst);
        let buffers = seen.buffers.lock().unwrap();
        let done = &buffers[n % buffers.len()];
        seen.first_bytes.lock().unwrap().push(done[0]);
        let enqueue: unsafe extern "C" fn(*const c_void, *const c_void, u32) -> SLresult =
            unsafe { std::mem::transmute(slot(itf, 0)) };
        assert_eq!(unsafe { enqueue(itf, done.as_ptr().cast(), 960) }, SUCCESS);
    }

    fn pcm() -> PcmFormat {
        PcmFormat {
            format_type: DATAFORMAT_PCM,
            num_channels: 1,
            samples_per_sec: 48_000_000,
            bits_per_sample: 16,
            container_size: 16,
            channel_mask: 4,
            endianness: 2,
        }
    }

    /// Create a player or recorder, run it for a while, stop it.
    unsafe fn run(recorder: bool) -> Arc<Seen> {
        let mut engine = std::ptr::null();
        assert_eq!(create_engine(&mut engine, fake), SUCCESS);
        unsafe { realize(engine) };
        let engine_itf = unsafe { get_interface(engine, &SL_IID_ENGINE) };

        let mut queue_locator = BufferQueueLocator { locator_type: DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE, num_buffers: 2 };
        let mut format = pcm();
        let queue_side = DataSource { locator: (&mut queue_locator as *mut BufferQueueLocator).cast(), format: (&mut format as *mut PcmFormat).cast() };
        let mut io = [DATALOCATOR_IODEVICE, 1, 0, 0];
        let mut mix_locator = [DATALOCATOR_OUTPUTMIX, 0, 0, 0];
        let other = DataSource {
            locator: if recorder { io.as_mut_ptr() } else { mix_locator.as_mut_ptr() },
            format: std::ptr::null(),
        };
        let ids = [SL_IID_ANDROIDSIMPLEBUFFERQUEUE.0, SL_IID_ANDROIDCONFIGURATION.0];
        let required = [1u32, 1];
        type Create = unsafe extern "C" fn(*const c_void, *mut *const c_void, *const DataSource, *const DataSource, u32, *const *const Iid, *const u32) -> SLresult;
        let create: Create = unsafe { std::mem::transmute(slot(engine_itf, if recorder { 3 } else { 2 })) };
        let mut obj = std::ptr::null();
        let (src, snk) = if recorder { (&other, &queue_side) } else { (&queue_side, &other) };
        assert_eq!(unsafe { create(engine_itf, &mut obj, src, snk, 2, ids.as_ptr(), required.as_ptr()) }, SUCCESS);

        // Configuration, as FMOD sets the recording preset / stream type.
        unsafe { realize(obj) };
        let config = unsafe { get_interface(obj, &SL_IID_ANDROIDCONFIGURATION) };
        let set: unsafe extern "C" fn(*const c_void, *const c_char, *const c_void, u32) -> SLresult =
            unsafe { std::mem::transmute(slot(config, 0)) };
        let value: u32 = 4;
        assert_eq!(unsafe { set(config, c"androidRecordingPreset".as_ptr(), (&value as *const u32).cast(), 4) }, SUCCESS);
        let get: unsafe extern "C" fn(*const c_void, *const c_char, *mut u32, *mut c_void) -> SLresult =
            unsafe { std::mem::transmute(slot(config, 1)) };
        let (mut size, mut back) = (4u32, 0u32);
        assert_eq!(unsafe { get(config, c"androidRecordingPreset".as_ptr(), &mut size, (&mut back as *mut u32).cast()) }, SUCCESS);
        assert_eq!(back, 4);

        let queue = unsafe { get_interface(obj, &SL_IID_ANDROIDSIMPLEBUFFERQUEUE) };
        let state_itf = unsafe { get_interface(obj, if recorder { &SL_IID_RECORD } else { &SL_IID_PLAY }) };
        let seen = Arc::new(Seen {
            calls: AtomicUsize::new(0),
            queue: AtomicUsize::new(0),
            first_bytes: Mutex::new(Vec::new()),
            buffers: Mutex::new(vec![Box::new([0xAAu8; 960]), Box::new([0xAAu8; 960])]),
        });
        let register: unsafe extern "C" fn(*const c_void, Option<QueueCallback>, *mut c_void) -> SLresult =
            unsafe { std::mem::transmute(slot(queue, 3)) };
        assert_eq!(unsafe { register(queue, Some(on_buffer), Arc::as_ptr(&seen) as *mut c_void) }, SUCCESS);
        let enqueue: unsafe extern "C" fn(*const c_void, *const c_void, u32) -> SLresult =
            unsafe { std::mem::transmute(slot(queue, 0)) };
        for b in seen.buffers.lock().unwrap().iter() {
            assert_eq!(unsafe { enqueue(queue, b.as_ptr().cast(), 960) }, SUCCESS);
        }
        // The queue holds only as many buffers as asked for.
        let extra = [0u8; 960];
        assert_eq!(unsafe { enqueue(queue, extra.as_ptr().cast(), 960) }, BUFFER_INSUFFICIENT);

        let set_state: unsafe extern "C" fn(*const c_void, u32) -> SLresult =
            unsafe { std::mem::transmute(slot(state_itf, 0)) };
        assert_eq!(unsafe { set_state(state_itf, STATE_ACTIVE) }, SUCCESS);
        std::thread::sleep(std::time::Duration::from_millis(60));
        assert_eq!(unsafe { set_state(state_itf, STATE_STOPPED) }, SUCCESS);
        let after_stop = seen.calls.load(Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Stopped means no more callbacks.
        assert_eq!(seen.calls.load(Ordering::SeqCst), after_stop);
        assert_eq!(seen.queue.load(Ordering::SeqCst), queue as usize);

        let clear: unsafe extern "C" fn(*const c_void) -> SLresult = unsafe { std::mem::transmute(slot(queue, 1)) };
        assert_eq!(unsafe { clear(queue) }, SUCCESS);
        let status: unsafe extern "C" fn(*const c_void, *mut QueueStatus) -> SLresult =
            unsafe { std::mem::transmute(slot(queue, 2)) };
        let mut st = QueueStatus { count: 9, index: 0 };
        assert_eq!(unsafe { status(queue, &mut st) }, SUCCESS);
        assert_eq!((st.count, st.index as usize), (0, after_stop));

        unsafe { destroy(obj) };
        unsafe { destroy(engine) };
        seen
    }

    #[test]
    fn a_player_plays_queued_buffers_and_calls_back() {
        let seen = unsafe { run(false) };
        let calls = seen.calls.load(Ordering::SeqCst);
        assert!(calls >= 10, "only {calls} buffers played in 60 ms at 1 ms each");
        // Players do not touch the samples.
        assert!(seen.first_bytes.lock().unwrap().iter().all(|b| *b == 0xAA));
    }

    #[test]
    fn a_recorder_fills_buffers_in_order() {
        let seen = unsafe { run(true) };
        let firsts = seen.first_bytes.lock().unwrap().clone();
        assert!(firsts.len() >= 10, "only {} buffers recorded", firsts.len());
        // The fake microphone counts 1, 2, 3...: buffers come back in order.
        for (i, b) in firsts.iter().enumerate() {
            assert_eq!(*b as usize, (i + 1) % 256);
        }
    }

    /// The real PulseAudio output (silence; the microphone is never opened
    /// by tests). Run with `-- --include-ignored`.
    #[test]
    #[ignore]
    fn pulseaudio_playback_works() {
        let format = Format { rate: 48_000, channels: 1 };
        let mut device = open_pulse(false, format, 960).expect("PulseAudio playback");
        let mut silence = [0u8; 960];
        for _ in 0..10 {
            assert!(device.transfer(&mut silence));
        }
    }

    #[test]
    fn unsupported_formats_and_interfaces_are_refused() {
        let mut engine = std::ptr::null();
        assert_eq!(create_engine(&mut engine, fake), SUCCESS);
        unsafe { realize(engine) };
        let engine_itf = unsafe { get_interface(engine, &SL_IID_ENGINE) };
        // The engine object has no play interface.
        let f: unsafe extern "C" fn(*const c_void, *const Iid, *mut *const c_void) -> SLresult =
            unsafe { std::mem::transmute(slot(engine, 3)) };
        let mut out = std::ptr::null();
        assert_eq!(unsafe { f(engine, SL_IID_PLAY.0, &mut out) }, FEATURE_UNSUPPORTED);

        let mut locator = BufferQueueLocator { locator_type: DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE, num_buffers: 2 };
        let mut format = PcmFormat { bits_per_sample: 8, ..pcm() };
        let source = DataSource { locator: (&mut locator as *mut BufferQueueLocator).cast(), format: (&mut format as *mut PcmFormat).cast() };
        let mut mix = [DATALOCATOR_OUTPUTMIX, 0];
        let sink = DataSource { locator: mix.as_mut_ptr(), format: std::ptr::null() };
        type Create = unsafe extern "C" fn(*const c_void, *mut *const c_void, *const DataSource, *const DataSource, u32, *const *const Iid, *const u32) -> SLresult;
        let create: Create = unsafe { std::mem::transmute(slot(engine_itf, 2)) };
        let mut obj = std::ptr::null();
        assert_eq!(unsafe { create(engine_itf, &mut obj, &source, &sink, 0, std::ptr::null(), std::ptr::null()) }, CONTENT_UNSUPPORTED);
        // A player cannot offer a record interface.
        format.bits_per_sample = 16;
        let ids = [SL_IID_RECORD.0];
        let required = [1u32];
        assert_eq!(unsafe { create(engine_itf, &mut obj, &source, &sink, 1, ids.as_ptr(), required.as_ptr()) }, FEATURE_UNSUPPORTED);
        unsafe { destroy(engine) };
    }
}
