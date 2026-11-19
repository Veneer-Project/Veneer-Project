//! `libmediandk.so`: only the exported data (format key strings) for now.
//! The codec entry points are left unresolved until video playback is done.

use std::collections::HashMap;
use std::ffi::c_char;

/// `extern const char* AMEDIAFORMAT_KEY_*`: the symbol is a pointer variable.
#[allow(dead_code)]
struct Key(*const c_char);
unsafe impl Sync for Key {}

macro_rules! keys {
    ($m:expr, $( $sym:ident = $val:expr ),* $(,)?) => {$(
        {
            static K: Key = Key($val.as_ptr());
            $m.insert(stringify!($sym), &K as *const Key as usize);
        }
    )*};
}

pub fn symbols(m: &mut HashMap<&'static str, usize>) {
    keys!(m,
        AMEDIAFORMAT_KEY_BIT_RATE = c"bitrate",
        AMEDIAFORMAT_KEY_CHANNEL_COUNT = c"channel-count",
        AMEDIAFORMAT_KEY_COLOR_FORMAT = c"color-format",
        AMEDIAFORMAT_KEY_FRAME_RATE = c"frame-rate",
        AMEDIAFORMAT_KEY_HEIGHT = c"height",
        AMEDIAFORMAT_KEY_I_FRAME_INTERVAL = c"i-frame-interval",
        AMEDIAFORMAT_KEY_MIME = c"mime",
        AMEDIAFORMAT_KEY_SAMPLE_RATE = c"sample-rate",
        AMEDIAFORMAT_KEY_STRIDE = c"stride",
        AMEDIAFORMAT_KEY_WIDTH = c"width",
    );
}
