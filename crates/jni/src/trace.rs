//! Where live local references come from (diagnostics, `LUCID_JNI_TRACE=1`).
//!
//! Every local reference created while tracing is labelled with the thread
//! that created it, what created it (the Java method whose result it is, or
//! the JNI function's source line), its object type, and whether it went
//! into a frame or the thread's implicit bottom frame. [`report`] counts the
//! labels of the references still alive: what keeps growing there is what
//! leaks. Off by default: it takes a global lock per reference.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LUCID_JNI_TRACE").is_some_and(|v| v != "0"))
}

/// Live references by pointer (the same object can be referenced several
/// times, so each pointer has a stack of labels).
static LIVE: Mutex<Option<HashMap<usize, Vec<String>>>> = Mutex::new(None);
static ANNOUNCED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static THREAD: String = thread_label();
}

fn thread_label() -> String {
    let mut name = [0u8; 32];
    let ok = unsafe { libc::pthread_getname_np(libc::pthread_self(), name.as_mut_ptr().cast(), name.len()) } == 0;
    let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
    let name = if ok { String::from_utf8_lossy(&name[..end]).into_owned() } else { String::new() };
    format!("{name} (tid {})", unsafe { libc::gettid() })
}

/// A new local reference `ptr`; `label` says what created it.
pub(crate) fn created(ptr: usize, bottom: bool, label: impl FnOnce() -> String) {
    if !enabled() {
        return;
    }
    if !ANNOUNCED.swap(true, Ordering::Relaxed) {
        log::info!("JNI: tracing where local references come from (LUCID_JNI_TRACE)");
    }
    let thread = THREAD.try_with(|t| t.clone()).unwrap_or_else(|_| "exiting thread".into());
    let frame = if bottom { "bottom frame" } else { "pushed frame" };
    let key = format!("{thread} | {} | {frame}", label());
    LIVE.lock().unwrap().get_or_insert_with(HashMap::new).entry(ptr).or_default().push(key);
}

/// A local reference `ptr` was released.
pub(crate) fn released(ptr: usize) {
    if !enabled() {
        return;
    }
    let mut live = LIVE.lock().unwrap();
    if let Some(map) = live.as_mut() {
        if let Some(keys) = map.get_mut(&ptr) {
            keys.pop();
            if keys.is_empty() {
                map.remove(&ptr);
            }
        }
    }
}

/// The most common labels among live references, if tracing.
pub fn report(top: usize) -> Option<String> {
    if !enabled() {
        return None;
    }
    let live = LIVE.lock().unwrap();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for key in live.iter().flat_map(|m| m.values()).flatten() {
        *counts.entry(key.as_str()).or_default() += 1;
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let lines: Vec<String> = counts.iter().take(top).map(|(k, n)| format!("  {n:5}  {k}")).collect();
    Some(lines.join("\n"))
}
