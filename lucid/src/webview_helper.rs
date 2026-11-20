//! The embedded browser, run as a separate process (`lucid webview`).
//!
//! It is a small GTK window with a WebKitGTK view. GTK and WebKit are
//! loaded with `dlopen`, so they are neither a build dependency nor loaded
//! into the game process; if they are missing the caller falls back to the
//! system browser.
//!
//! Protocol with the parent, one message per line:
//!   stdin  first line: JSON `{url, title, user_agent, cookies: [[name, value]..]}`
//!          then:       `ALLOW <n>` | `DENY <n>` | `EVAL <javascript>` | `CLOSE`
//!   stdout             `NAVIGATE <n> <url>` | `BRIDGE <json>` | `LOADED <url>`

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::Write;
use std::sync::Mutex;

type Ptr = *mut c_void;

/// Exit code when GTK/WebKit are not installed.
pub const EXIT_UNAVAILABLE: i32 = 3;

struct Lib(Ptr);

impl Lib {
    fn open(names: &[&CStr]) -> Option<Lib> {
        names.iter().find_map(|n| {
            let h = unsafe { libc::dlopen(n.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
            (!h.is_null()).then_some(Lib(h))
        })
    }

    /// Look a function up. `T` must be the matching `extern "C"` fn type.
    unsafe fn get<T: Copy>(&self, name: &CStr) -> T {
        let p = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        assert!(!p.is_null(), "missing symbol {name:?}");
        assert_eq!(size_of::<T>(), size_of::<Ptr>());
        unsafe { std::mem::transmute_copy(&p) }
    }
}

/// Everything we call, resolved once.
struct Api {
    gtk_main_quit: unsafe extern "C" fn(),
    load_uri: unsafe extern "C" fn(Ptr, *const c_char),
    evaluate: Option<unsafe extern "C" fn(Ptr, *const c_char, isize, *const c_char, *const c_char, Ptr, Ptr, Ptr)>,
    run_javascript: Option<unsafe extern "C" fn(Ptr, *const c_char, Ptr, Ptr, Ptr)>,
    decision_ignore: unsafe extern "C" fn(Ptr),
    get_navigation_action: unsafe extern "C" fn(Ptr) -> Ptr,
    action_get_request: unsafe extern "C" fn(Ptr) -> Ptr,
    action_is_user_gesture: unsafe extern "C" fn(Ptr) -> c_int,
    request_get_uri: unsafe extern "C" fn(Ptr) -> *const c_char,
    result_get_js_value: unsafe extern "C" fn(Ptr) -> Ptr,
    jsc_value_to_string: unsafe extern "C" fn(Ptr) -> *mut c_char,
    g_free: unsafe extern "C" fn(Ptr),
    view_get_uri: unsafe extern "C" fn(Ptr) -> *const c_char,
    get_snapshot: unsafe extern "C" fn(Ptr, c_int, c_int, Ptr, Ptr, Ptr),
    get_snapshot_finish: unsafe extern "C" fn(Ptr, Ptr, Ptr) -> Ptr,
    surface_write_png: Option<unsafe extern "C" fn(Ptr, *const c_char) -> c_int>,
}

struct State {
    api: Api,
    view: Ptr,
    initial_url: String,
    first_navigation_done: bool,
    next_id: u64,
    /// Bytes read from stdin that are not yet a full line.
    pending: Vec<u8>,
    /// Commands read while waiting for a navigation reply.
    queued: Vec<String>,
}

// Only ever touched on the GTK main thread; the mutex is for form.
struct Shared(Mutex<Option<State>>);
unsafe impl Sync for Shared {}
static STATE: Shared = Shared(Mutex::new(None));

fn emit(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Read whatever complete lines are available on stdin, waiting up to
/// `timeout_ms` for the first byte. `None` means the parent went away.
fn read_lines(pending: &mut Vec<u8>, timeout_ms: c_int) -> Option<Vec<String>> {
    let mut fds = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    let ready = unsafe { libc::poll(&mut fds, 1, timeout_ms) };
    if ready > 0 {
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::read(0, buf.as_mut_ptr() as Ptr, buf.len()) };
        if n == 0 {
            return None;
        }
        if n > 0 {
            pending.extend_from_slice(&buf[..n as usize]);
        }
    }
    let mut lines = Vec::new();
    while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
        let line: Vec<u8> = pending.drain(..=pos).collect();
        lines.push(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned());
    }
    Some(lines)
}

fn run_command(state: &mut State, line: &str) {
    if line == "CLOSE" {
        unsafe { (state.api.gtk_main_quit)() };
    } else if let Some(js) = line.strip_prefix("EVAL ") {
        let Ok(js) = CString::new(js) else { return };
        unsafe {
            if let Some(f) = state.api.evaluate {
                let null = std::ptr::null();
                f(state.view, js.as_ptr(), -1, null, null, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            } else if let Some(f) = state.api.run_javascript {
                f(state.view, js.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            }
        }
    }
}

/// Periodic check for commands from the parent.
unsafe extern "C" fn on_tick(_data: Ptr) -> c_int {
    let mut guard = STATE.0.lock().unwrap();
    let Some(state) = guard.as_mut() else { return 0 };
    let mut lines = std::mem::take(&mut state.queued);
    match read_lines(&mut state.pending, 0) {
        Some(more) => lines.extend(more),
        None => {
            unsafe { (state.api.gtk_main_quit)() };
            return 0;
        }
    }
    for line in lines {
        run_command(state, &line);
    }
    1
}

/// Ask the parent whether the app wants to handle this navigation itself.
fn parent_allows(state: &mut State, url: &str) -> bool {
    state.next_id += 1;
    let id = state.next_id;
    emit(&format!("NAVIGATE {id} {url}"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let Some(lines) = read_lines(&mut state.pending, 100) else {
            return false;
        };
        for line in lines {
            if line == format!("ALLOW {id}") {
                return true;
            } else if line == format!("DENY {id}") {
                return false;
            }
            state.queued.push(line);
        }
    }
    // No answer: behave like a normal browser.
    true
}

unsafe extern "C" fn on_decide_policy(_view: Ptr, decision: Ptr, kind: c_int, _data: Ptr) -> c_int {
    // 0: navigation, 1: new window, 2: response.
    if kind > 1 {
        return 0;
    }
    let mut guard = STATE.0.lock().unwrap();
    let Some(state) = guard.as_mut() else { return 0 };
    let (url, gesture) = unsafe {
        let action = (state.api.get_navigation_action)(decision);
        let request = (state.api.action_get_request)(action);
        let uri = (state.api.request_get_uri)(request);
        if uri.is_null() {
            return 0;
        }
        (
            CStr::from_ptr(uri).to_string_lossy().into_owned(),
            (state.api.action_is_user_gesture)(action) != 0,
        )
    };
    let web = url.starts_with("https://") || url.starts_with("http://");
    let internal = ["about:", "blob:", "data:", "javascript:"].iter().any(|p| url.starts_with(p));
    if internal {
        return 0;
    }
    // The page we were asked to show loads without questions.
    if !state.first_navigation_done && url == state.initial_url {
        state.first_navigation_done = true;
        return 0;
    }
    // Script-driven page loads are left alone; anything the user clicked,
    // and every app link (roblox://...), is offered to the app first.
    let allowed = if web && !gesture && kind == 0 {
        true
    } else {
        parent_allows(state, &url)
    };
    if !allowed || !web {
        unsafe { (state.api.decision_ignore)(decision) };
        return 1;
    }
    if kind == 1 {
        // "Open in new window": keep it in this one.
        unsafe {
            (state.api.decision_ignore)(decision);
            if let Ok(c) = CString::new(url) {
                (state.api.load_uri)(state.view, c.as_ptr());
            }
        }
        return 1;
    }
    0
}

/// The page called `__globalRobloxAndroidBridge__.executeRoblox(json)`.
unsafe extern "C" fn on_script_message(_manager: Ptr, result: Ptr, _data: Ptr) {
    let guard = STATE.0.lock().unwrap();
    let Some(state) = guard.as_ref() else { return };
    unsafe {
        let value = (state.api.result_get_js_value)(result);
        let s = (state.api.jsc_value_to_string)(value);
        if !s.is_null() {
            let text = CStr::from_ptr(s).to_string_lossy().replace('\n', " ");
            (state.api.g_free)(s as Ptr);
            emit(&format!("BRIDGE {text}"));
        }
    }
}

unsafe extern "C" fn on_load_changed(view: Ptr, event: c_int, _data: Ptr) {
    // 3: WEBKIT_LOAD_FINISHED
    if event != 3 {
        return;
    }
    let guard = STATE.0.lock().unwrap();
    let Some(state) = guard.as_ref() else { return };
    let uri = unsafe { (state.api.view_get_uri)(view) };
    if !uri.is_null() {
        emit(&format!("LOADED {}", unsafe { CStr::from_ptr(uri) }.to_string_lossy()));
    }
}

unsafe extern "C" fn on_destroy(_window: Ptr, _data: Ptr) {
    if let Some(state) = STATE.0.lock().unwrap().as_ref() {
        unsafe { (state.api.gtk_main_quit)() };
    }
}

// Debug aid: LUCID_WEBVIEW_SNAPSHOT=<file.png> saves what the page looks like.
unsafe extern "C" fn on_snapshot_ready(view: Ptr, result: Ptr, _data: Ptr) {
    let guard = STATE.0.lock().unwrap();
    let Some(state) = guard.as_ref() else { return };
    let Some(path) = std::env::var_os("LUCID_WEBVIEW_SNAPSHOT") else { return };
    unsafe {
        let surface = (state.api.get_snapshot_finish)(view, result, std::ptr::null_mut());
        if let (false, Some(write), Ok(c)) =
            (surface.is_null(), state.api.surface_write_png, CString::new(path.to_string_lossy().as_bytes()))
        {
            write(surface, c.as_ptr());
            emit("SNAPSHOT");
        }
    }
}

unsafe extern "C" fn on_snapshot_timer(_data: Ptr) -> c_int {
    let guard = STATE.0.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        let cb: unsafe extern "C" fn(Ptr, Ptr, Ptr) = on_snapshot_ready;
        // region 0: visible, options 0: none
        unsafe { (state.api.get_snapshot)(state.view, 0, 0, std::ptr::null_mut(), cb as Ptr, std::ptr::null_mut()) };
    }
    0
}

/// Run the browser window until it is closed. Returns the process exit code.
pub fn run() -> i32 {
    // The first stdin line is the configuration.
    let mut pending = Vec::new();
    let config = loop {
        match read_lines(&mut pending, 10_000) {
            Some(mut lines) if !lines.is_empty() => break lines.remove(0),
            Some(_) => continue,
            None => return 2,
        }
    };
    let Ok(config) = serde_json::from_str::<serde_json::Value>(&config) else {
        return 2;
    };
    let text = |k: &str| config.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let (url, title, user_agent) = (text("url"), text("title"), text("user_agent"));
    if !url.starts_with("https://") {
        return 2;
    }

    let (Some(gtk), Some(webkit), Some(gobject), Some(glib)) = (
        Lib::open(&[c"libgtk-3.so.0"]),
        Lib::open(&[c"libwebkit2gtk-4.1.so.0", c"libwebkit2gtk-4.0.so.37"]),
        Lib::open(&[c"libgobject-2.0.so.0"]),
        Lib::open(&[c"libglib-2.0.so.0"]),
    ) else {
        return EXIT_UNAVAILABLE;
    };
    let soup = Lib::open(&[c"libsoup-3.0.so.0", c"libsoup-2.4.so.1"]);
    let jsc = Lib::open(&[c"libjavascriptcoregtk-4.1.so.0", c"libjavascriptcoregtk-4.0.so.18"]);
    let cairo = Lib::open(&[c"libcairo.so.2"]);
    let (Some(soup), Some(jsc)) = (soup, jsc) else {
        return EXIT_UNAVAILABLE;
    };
    let optional = |lib: &Lib, name: &CStr| -> Option<Ptr> {
        let p = unsafe { libc::dlsym(lib.0, name.as_ptr()) };
        (!p.is_null()).then_some(p)
    };

    unsafe {
        let gtk_init_check: unsafe extern "C" fn(Ptr, Ptr) -> c_int = gtk.get(c"gtk_init_check");
        if gtk_init_check(std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
            return EXIT_UNAVAILABLE;
        }
        let window_new: unsafe extern "C" fn(c_int) -> Ptr = gtk.get(c"gtk_window_new");
        let set_title: unsafe extern "C" fn(Ptr, *const c_char) = gtk.get(c"gtk_window_set_title");
        let set_default_size: unsafe extern "C" fn(Ptr, c_int, c_int) = gtk.get(c"gtk_window_set_default_size");
        let container_add: unsafe extern "C" fn(Ptr, Ptr) = gtk.get(c"gtk_container_add");
        let show_all: unsafe extern "C" fn(Ptr) = gtk.get(c"gtk_widget_show_all");
        let gtk_main: unsafe extern "C" fn() = gtk.get(c"gtk_main");
        let signal_connect: unsafe extern "C" fn(Ptr, *const c_char, Ptr, Ptr, Ptr, c_int) -> u64 =
            gobject.get(c"g_signal_connect_data");
        let object_new: unsafe extern "C" fn(usize, *const c_char, ...) -> Ptr = gobject.get(c"g_object_new");
        let timeout_add: unsafe extern "C" fn(u32, Ptr, Ptr) -> u32 = glib.get(c"g_timeout_add");

        // A private, in-memory browsing session: nothing is written to disk.
        let context_new: unsafe extern "C" fn() -> Ptr = webkit.get(c"webkit_web_context_new_ephemeral");
        let context = context_new();
        let ucm_new: unsafe extern "C" fn() -> Ptr = webkit.get(c"webkit_user_content_manager_new");
        let ucm = ucm_new();

        // The page talks to the app through this object on Android.
        let script_new: unsafe extern "C" fn(*const c_char, c_int, c_int, Ptr, Ptr) -> Ptr =
            webkit.get(c"webkit_user_script_new");
        let add_script: unsafe extern "C" fn(Ptr, Ptr) = webkit.get(c"webkit_user_content_manager_add_script");
        let register: unsafe extern "C" fn(Ptr, *const c_char) -> c_int =
            webkit.get(c"webkit_user_content_manager_register_script_message_handler");
        let bridge = c"window.__globalRobloxAndroidBridge__ = { executeRoblox: function (s) { window.webkit.messageHandlers.lucid.postMessage(String(s)); } };";
        add_script(ucm, script_new(bridge.as_ptr(), 0, 0, std::ptr::null_mut(), std::ptr::null_mut()));
        register(ucm, c"lucid".as_ptr());

        let view_type: unsafe extern "C" fn() -> usize = webkit.get(c"webkit_web_view_get_type");
        let view = object_new(
            view_type(),
            c"web-context".as_ptr(),
            context,
            c"user-content-manager".as_ptr(),
            ucm,
            std::ptr::null::<c_char>(),
        );

        if !user_agent.is_empty() {
            let get_settings: unsafe extern "C" fn(Ptr) -> Ptr = webkit.get(c"webkit_web_view_get_settings");
            let set_ua: unsafe extern "C" fn(Ptr, *const c_char) = webkit.get(c"webkit_settings_set_user_agent");
            if let Ok(ua) = CString::new(user_agent) {
                set_ua(get_settings(view), ua.as_ptr());
            }
        }

        // Hand the app's login session to the page.
        let cookie_manager: unsafe extern "C" fn(Ptr) -> Ptr = webkit.get(c"webkit_web_context_get_cookie_manager");
        let add_cookie: unsafe extern "C" fn(Ptr, Ptr, Ptr, Ptr, Ptr) = webkit.get(c"webkit_cookie_manager_add_cookie");
        let cookie_new: unsafe extern "C" fn(*const c_char, *const c_char, *const c_char, *const c_char, c_int) -> Ptr =
            soup.get(c"soup_cookie_new");
        let cookie_secure: unsafe extern "C" fn(Ptr, c_int) = soup.get(c"soup_cookie_set_secure");
        let cookie_http_only: unsafe extern "C" fn(Ptr, c_int) = soup.get(c"soup_cookie_set_http_only");
        let cookie_free: unsafe extern "C" fn(Ptr) = soup.get(c"soup_cookie_free");
        let manager = cookie_manager(context);
        for pair in config.get("cookies").and_then(|c| c.as_array()).into_iter().flatten() {
            let (Some(name), Some(value)) = (
                pair.get(0).and_then(|v| v.as_str()).and_then(|s| CString::new(s).ok()),
                pair.get(1).and_then(|v| v.as_str()).and_then(|s| CString::new(s).ok()),
            ) else {
                continue;
            };
            let cookie = cookie_new(name.as_ptr(), value.as_ptr(), c".roblox.com".as_ptr(), c"/".as_ptr(), -1);
            cookie_secure(cookie, 1);
            cookie_http_only(cookie, 1);
            add_cookie(manager, cookie, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            cookie_free(cookie);
        }

        let api = Api {
            gtk_main_quit: gtk.get(c"gtk_main_quit"),
            load_uri: webkit.get(c"webkit_web_view_load_uri"),
            evaluate: optional(&webkit, c"webkit_web_view_evaluate_javascript").map(|p| std::mem::transmute(p)),
            run_javascript: optional(&webkit, c"webkit_web_view_run_javascript").map(|p| std::mem::transmute(p)),
            decision_ignore: webkit.get(c"webkit_policy_decision_ignore"),
            get_navigation_action: webkit.get(c"webkit_navigation_policy_decision_get_navigation_action"),
            action_get_request: webkit.get(c"webkit_navigation_action_get_request"),
            action_is_user_gesture: webkit.get(c"webkit_navigation_action_is_user_gesture"),
            request_get_uri: webkit.get(c"webkit_uri_request_get_uri"),
            result_get_js_value: webkit.get(c"webkit_javascript_result_get_js_value"),
            jsc_value_to_string: jsc.get(c"jsc_value_to_string"),
            g_free: glib.get(c"g_free"),
            view_get_uri: webkit.get(c"webkit_web_view_get_uri"),
            get_snapshot: webkit.get(c"webkit_web_view_get_snapshot"),
            get_snapshot_finish: webkit.get(c"webkit_web_view_get_snapshot_finish"),
            surface_write_png: cairo
                .as_ref()
                .and_then(|c| optional(c, c"cairo_surface_write_to_png"))
                .map(|p| std::mem::transmute(p)),
        };
        let load_uri = api.load_uri;
        *STATE.0.lock().unwrap() = Some(State {
            api,
            view,
            initial_url: url.clone(),
            first_navigation_done: false,
            next_id: 0,
            pending,
            queued: Vec::new(),
        });

        let window = window_new(0);
        let title = if title.is_empty() { "Roblox".to_string() } else { format!("{title} - Roblox") };
        if let Ok(t) = CString::new(title) {
            set_title(window, t.as_ptr());
        }
        set_default_size(window, 1100, 720);
        container_add(window, view);

        let connect = |object: Ptr, signal: &CStr, handler: Ptr| {
            signal_connect(object, signal.as_ptr(), handler, std::ptr::null_mut(), std::ptr::null_mut(), 0);
        };
        let decide: unsafe extern "C" fn(Ptr, Ptr, c_int, Ptr) -> c_int = on_decide_policy;
        let message: unsafe extern "C" fn(Ptr, Ptr, Ptr) = on_script_message;
        let loaded: unsafe extern "C" fn(Ptr, c_int, Ptr) = on_load_changed;
        let destroy: unsafe extern "C" fn(Ptr, Ptr) = on_destroy;
        let tick: unsafe extern "C" fn(Ptr) -> c_int = on_tick;
        connect(view, c"decide-policy", decide as Ptr);
        connect(ucm, c"script-message-received::lucid", message as Ptr);
        connect(view, c"load-changed", loaded as Ptr);
        connect(window, c"destroy", destroy as Ptr);
        timeout_add(100, tick as Ptr, std::ptr::null_mut());
        if std::env::var_os("LUCID_WEBVIEW_SNAPSHOT").is_some() {
            let snap: unsafe extern "C" fn(Ptr) -> c_int = on_snapshot_timer;
            timeout_add(9000, snap as Ptr, std::ptr::null_mut());
        }

        if let Ok(c) = CString::new(url) {
            load_uri(view, c.as_ptr());
        }
        show_all(window);
        gtk_main();
    }
    0
}
