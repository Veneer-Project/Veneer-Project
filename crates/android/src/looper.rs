//! `ALooper`: a per-thread epoll-based event loop.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{c_int, c_void};
use std::sync::Mutex;

const POLL_WAKE: c_int = -1;
const POLL_CALLBACK: c_int = -2;
const POLL_TIMEOUT: c_int = -3;
const POLL_ERROR: c_int = -4;

const EVENT_INPUT: c_int = 1;
const EVENT_OUTPUT: c_int = 2;
const EVENT_ERROR: c_int = 4;
const EVENT_HANGUP: c_int = 8;

type Callback = unsafe extern "C" fn(fd: c_int, events: c_int, data: *mut c_void) -> c_int;

struct Entry {
    ident: c_int,
    callback: Option<Callback>,
    data: usize,
}

pub struct Looper {
    epfd: c_int,
    wake_fd: c_int,
    fds: Mutex<HashMap<c_int, Entry>>,
}

thread_local! {
    static CURRENT: Cell<*mut Looper> = const { Cell::new(std::ptr::null_mut()) };
}

fn to_epoll(events: c_int) -> u32 {
    let mut e = 0;
    if events & EVENT_INPUT != 0 {
        e |= libc::EPOLLIN;
    }
    if events & EVENT_OUTPUT != 0 {
        e |= libc::EPOLLOUT;
    }
    e as u32
}

fn from_epoll(e: u32) -> c_int {
    let e = e as c_int;
    let mut events = 0;
    if e & libc::EPOLLIN != 0 {
        events |= EVENT_INPUT;
    }
    if e & libc::EPOLLOUT != 0 {
        events |= EVENT_OUTPUT;
    }
    if e & libc::EPOLLERR != 0 {
        events |= EVENT_ERROR;
    }
    if e & libc::EPOLLHUP != 0 {
        events |= EVENT_HANGUP;
    }
    events
}

unsafe extern "C" fn for_thread() -> *mut Looper {
    CURRENT.with(|c| c.get())
}

unsafe extern "C" fn prepare(_opts: c_int) -> *mut Looper {
    CURRENT.with(|c| {
        if c.get().is_null() {
            let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
            let wake_fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
            let mut ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: wake_fd as u64,
            };
            unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, wake_fd, &mut ev) };
            // Loopers are leaked: they live as long as their thread, and the
            // guest may keep references after the thread is gone.
            c.set(Box::into_raw(Box::new(Looper {
                epfd,
                wake_fd,
                fds: Mutex::new(HashMap::new()),
            })));
        }
        c.get()
    })
}

unsafe extern "C" fn acquire(_l: *mut Looper) {}
unsafe extern "C" fn release(_l: *mut Looper) {}

unsafe extern "C" fn wake(l: *mut Looper) {
    let one: u64 = 1;
    unsafe { libc::write((*l).wake_fd, &one as *const u64 as *const c_void, 8) };
}

unsafe extern "C" fn add_fd(
    l: *mut Looper,
    fd: c_int,
    ident: c_int,
    events: c_int,
    callback: Option<Callback>,
    data: *mut c_void,
) -> c_int {
    let l = unsafe { &*l };
    let mut ev = libc::epoll_event {
        events: to_epoll(events),
        u64: fd as u64,
    };
    let mut fds = l.fds.lock().unwrap();
    let op = if fds.contains_key(&fd) {
        libc::EPOLL_CTL_MOD
    } else {
        libc::EPOLL_CTL_ADD
    };
    if unsafe { libc::epoll_ctl(l.epfd, op, fd, &mut ev) } != 0 {
        return -1;
    }
    fds.insert(
        fd,
        Entry {
            ident: if callback.is_some() { POLL_CALLBACK } else { ident },
            callback,
            data: data as usize,
        },
    );
    1
}

unsafe extern "C" fn remove_fd(l: *mut Looper, fd: c_int) -> c_int {
    let l = unsafe { &*l };
    if l.fds.lock().unwrap().remove(&fd).is_none() {
        return 0;
    }
    unsafe { libc::epoll_ctl(l.epfd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut()) };
    1
}

unsafe extern "C" fn poll_once(
    timeout_ms: c_int,
    out_fd: *mut c_int,
    out_events: *mut c_int,
    out_data: *mut *mut c_void,
) -> c_int {
    let l = CURRENT.with(|c| c.get());
    if l.is_null() {
        return POLL_ERROR;
    }
    let l = unsafe { &*l };
    let mut evs = [libc::epoll_event { events: 0, u64: 0 }; 16];
    let n = loop {
        let n = unsafe { libc::epoll_wait(l.epfd, evs.as_mut_ptr(), evs.len() as c_int, timeout_ms) };
        if n >= 0 {
            break n as usize;
        }
        if unsafe { *libc::__errno_location() } != libc::EINTR {
            return POLL_ERROR;
        }
    };
    if n == 0 {
        return POLL_TIMEOUT;
    }
    let mut result = POLL_WAKE;
    let mut ident_hit: Option<(c_int, c_int, c_int, usize)> = None;
    for ev in &evs[..n] {
        let fd = ev.u64 as c_int;
        let events = from_epoll(ev.events);
        if fd == l.wake_fd {
            let mut buf = 0u64;
            unsafe { libc::read(fd, &mut buf as *mut u64 as *mut c_void, 8) };
            continue;
        }
        let entry = {
            let fds = l.fds.lock().unwrap();
            fds.get(&fd).map(|e| (e.ident, e.callback, e.data))
        };
        let Some((ident, callback, data)) = entry else {
            continue;
        };
        match callback {
            Some(cb) => {
                result = POLL_CALLBACK;
                if unsafe { cb(fd, events, data as *mut c_void) } == 0 {
                    unsafe { remove_fd(l as *const Looper as *mut Looper, fd) };
                }
            }
            None => {
                if ident_hit.is_none() {
                    ident_hit = Some((ident, fd, events, data));
                }
            }
        }
    }
    if let Some((ident, fd, events, data)) = ident_hit {
        unsafe {
            if !out_fd.is_null() {
                *out_fd = fd;
            }
            if !out_events.is_null() {
                *out_events = events;
            }
            if !out_data.is_null() {
                *out_data = data as *mut c_void;
            }
        }
        return ident;
    }
    result
}

unsafe extern "C" fn poll_all(
    timeout_ms: c_int,
    out_fd: *mut c_int,
    out_events: *mut c_int,
    out_data: *mut *mut c_void,
) -> c_int {
    loop {
        let r = unsafe { poll_once(timeout_ms, out_fd, out_events, out_data) };
        if r != POLL_CALLBACK {
            return r;
        }
    }
}

pub fn symbols(m: &mut HashMap<&'static str, usize>) {
    m.insert("ALooper_forThread", for_thread as usize);
    m.insert("ALooper_prepare", prepare as usize);
    m.insert("ALooper_acquire", acquire as usize);
    m.insert("ALooper_release", release as usize);
    m.insert("ALooper_wake", wake as usize);
    m.insert("ALooper_addFd", add_fd as usize);
    m.insert("ALooper_removeFd", remove_fd as usize);
    m.insert("ALooper_pollOnce", poll_once as usize);
    m.insert("ALooper_pollAll", poll_all as usize);
}
