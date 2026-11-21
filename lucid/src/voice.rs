//! Voice chat's Java side: WebRTC's `WebRtcAudioManager`.
//!
//! Roblox captures and plays voice through FMOD (see `opensles.rs`), not
//! through WebRTC's own audio devices, but WebRTC still creates its audio
//! layer when voice starts and aborts the game if this class does not
//! answer. On Android it reads the device's audio setup and hands it to
//! WebRTC (`nativeCacheAudioParameters`). Hardware echo cancellation and
//! noise suppression are reported missing, so software ones would be used.

use lucid_jni::{vm, with_frame};

use crate::java::local;

const MANAGER: &str = "org/webrtc/voiceengine/WebRtcAudioManager";

/// Voice audio: 48 kHz mono, 10 ms buffers.
const SAMPLE_RATE: i32 = 48_000;
const FRAMES_PER_BUFFER: i32 = SAMPLE_RATE / 100;

pub fn install() {
    let vm = vm();
    // new WebRtcAudioManager(long nativeAudioManager)
    vm.on(MANAGER, "<init>", "(J)V", |this, a| {
        let Some(this) = this.cloned() else {
            return;
        };
        let native = a.first().map(|v| v.as_i64()).unwrap_or(0);
        log::debug!("voice chat: audio manager created");
        with_frame(|| {
            jcall_on!(
                local(&this);
                MANAGER,
                "nativeCacheAudioParameters",
                "(IIIZZZZZZZIIJ)V",
                fn(i32, i32, i32, u8, u8, u8, u8, u8, u8, u8, i32, i32, i64),
                SAMPLE_RATE,
                1, // output channels
                1, // input channels
                0, // hardware echo cancellation
                0, // hardware gain control
                0, // hardware noise suppression
                1, // low-latency output (selects OpenSL ES)
                1, // low-latency input (selects OpenSL ES)
                0, // pro audio
                0, // AAudio
                FRAMES_PER_BUFFER,
                FRAMES_PER_BUFFER,
                native
            );
        });
    });
    vm.on(MANAGER, "init", "()Z", |_, _| true);
    vm.on(MANAGER, "dispose", "()V", |_, _| log::debug!("voice chat: audio manager disposed"));
    // Android's MODE_IN_COMMUNICATION, which the real class always sets.
    vm.on(MANAGER, "isCommunicationModeEnabled", "()Z", |_, _| true);
    vm.on(MANAGER, "isDeviceBlacklistedForOpenSLESUsage", "()Z", |_, _| false);
    // Muting is done by WebRTC itself (it stops sending); nothing to do here.
    vm.on(MANAGER, "setMicrophoneMute", "(Z)V", |_, a| {
        log::debug!("voice chat: microphone mute {}", a.first().map(|v| v.as_i64() != 0).unwrap_or(false));
    });
}
