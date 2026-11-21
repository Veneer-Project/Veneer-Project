//! FMOD's Java `AudioDevice` (an `AudioTrack` on Android) on PulseAudio.
//! Only used with `LUCID_FMOD_AUDIOTRACK=1`: by default FMOD plays and
//! records through OpenSL ES (`opensles.rs`), which voice chat needs.

use lucid_jni::{JValue, Ref, vm};

use crate::pulse;

const DEVICE: &str = "org/fmod/AudioDevice";

/// Run `f` with the stream open on an `AudioDevice` object, if any.
fn with_stream(this: Option<&Ref>, f: impl FnOnce(&pulse::Stream)) {
    let Some(inst) = this.and_then(|t| t.as_instance()) else {
        return;
    };
    let data = inst.data.lock().unwrap();
    if let Some(s) = data.as_ref().and_then(|d| d.downcast_ref::<pulse::Stream>()) {
        f(s);
    }
}

pub fn install() {
    let vm = vm();
    vm.on(DEVICE, "<init>", "()V", |_, _| ());
    // init(channels, sampleRate, blockSize, blockCount)
    vm.on(DEVICE, "init", "(IIII)Z", |this, a| {
        let Some(inst) = this.and_then(|t| t.as_instance()) else {
            return false;
        };
        let channels = a[0].as_i64().clamp(1, 8) as u8;
        let rate = a[1].as_i64() as u32;
        let block_bytes = (a[2].as_i64().max(1) * channels as i64 * 2) as u32;
        // Keep latency near FMOD's own buffering (a few mixer blocks).
        let target = block_bytes * a[3].as_i64().clamp(2, 8) as u32;
        match pulse::Stream::playback(c"Game audio", rate, channels, target) {
            Ok(stream) => {
                log::info!("audio output: {channels} channels at {rate} Hz");
                *inst.data.lock().unwrap() = Some(Box::new(stream));
                true
            }
            Err(e) => {
                log::warn!("cannot open audio output ({e})");
                false
            }
        }
    });
    // write(byte[] pcm, int length): blocking, interleaved 16-bit samples.
    vm.on(DEVICE, "write", "([BI)V", |this, a| {
        let Some(arr) = a.first().and_then(JValue::as_object).and_then(|o| o.as_prim_array()) else {
            return;
        };
        let len = (a[1].as_i64().max(0) as usize).min(arr.byte_len());
        let bytes = unsafe { std::slice::from_raw_parts(arr.as_ptr() as *const u8, len) };
        with_stream(this, |s| {
            s.write(bytes);
        });
    });
    vm.on(DEVICE, "close", "()V", |this, _| {
        if let Some(inst) = this.and_then(|t| t.as_instance()) {
            // Dropping the stream closes it.
            inst.data.lock().unwrap().take();
        }
    });
}
