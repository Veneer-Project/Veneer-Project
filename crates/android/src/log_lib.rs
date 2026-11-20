//! `liblog.so`: Android logging, routed to the host logger.

use std::collections::HashMap;
use std::ffi::{CStr, VaList, c_char, c_int};

unsafe extern "C" {
    fn vsnprintf(buf: *mut c_char, size: usize, fmt: *const c_char, ap: VaList) -> c_int;
}

fn emit(prio: c_int, tag: *const c_char, text: &str) {
    let tag = if tag.is_null() {
        "".into()
    } else {
        unsafe { CStr::from_ptr(tag) }.to_string_lossy()
    };
    let level = match prio {
        ..=2 => log::Level::Trace,
        3 => log::Level::Debug,
        4 => log::Level::Info,
        5 => log::Level::Warn,
        _ => log::Level::Error,
    };
    log::log!(target: "android", level, "[{tag}] {}", text.trim_end());
}

unsafe fn format(fmt: *const c_char, ap: VaList) -> String {
    let mut buf = vec![0u8; 4096];
    let n = unsafe { vsnprintf(buf.as_mut_ptr() as *mut c_char, buf.len(), fmt, ap) };
    let n = (n.max(0) as usize).min(buf.len() - 1);
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

unsafe extern "C" fn log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int {
    emit(prio, tag, &unsafe { CStr::from_ptr(text) }.to_string_lossy());
    1
}

unsafe extern "C" fn log_buf_write(
    _buf: c_int,
    prio: c_int,
    tag: *const c_char,
    text: *const c_char,
) -> c_int {
    unsafe { log_write(prio, tag, text) }
}

unsafe extern "C" fn log_print(prio: c_int, tag: *const c_char, fmt: *const c_char, ap: ...) -> c_int {
    emit(prio, tag, &unsafe { format(fmt, ap) });
    1
}

unsafe extern "C" fn log_vprint(
    prio: c_int,
    tag: *const c_char,
    fmt: *const c_char,
    ap: VaList,
) -> c_int {
    emit(prio, tag, &unsafe { format(fmt, ap) });
    1
}

unsafe extern "C" fn log_assert(
    cond: *const c_char,
    tag: *const c_char,
    fmt: *const c_char,
    ap: ...
) -> ! {
    let text = if fmt.is_null() {
        if cond.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(cond) }.to_string_lossy().into_owned()
        }
    } else {
        unsafe { format(fmt, ap) }
    };
    emit(7, tag, &text);
    eprintln!("veneer: guest __android_log_assert: {text}");
    std::process::abort();
}

unsafe extern "C" fn is_loggable(_prio: c_int, _tag: *const c_char, _default: c_int) -> c_int {
    1
}

pub fn symbols(m: &mut HashMap<&'static str, usize>) {
    m.insert("__android_log_write", log_write as usize);
    m.insert("__android_log_buf_write", log_buf_write as usize);
    m.insert("__android_log_print", log_print as usize);
    m.insert("__android_log_vprint", log_vprint as usize);
    m.insert("__android_log_assert", log_assert as usize);
    m.insert("__android_log_is_loggable", is_loggable as usize);
}
