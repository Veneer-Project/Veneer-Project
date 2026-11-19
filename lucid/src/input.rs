//! Mouse, keyboard and text input: host window events translated into the
//! calls Android's input code makes.

use std::sync::Mutex;

use lucid_android::window::NativeWindow;
use lucid_jni::{JObject, JValue, vm, with_frame};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use winit::platform::scancode::PhysicalKeyExtScancode;

use smithay_clipboard::Clipboard;

use crate::java::jstr;
use crate::jcall;
use crate::overlay::{self, Geometry};

const INPUT: &str = "com/roblox/engine/jni/NativeInputInterface";
const GL: &str = "com/roblox/engine/jni/NativeGLInterface";
const GL_JAVA: &str = "com/roblox/engine/jni/NativeGLJavaInterface";

/// The engine text box currently being edited (Android shows an invisible
/// `EditText` for it; we keep the equivalent editing state here).
struct TextBox {
    handle: i64,
    text: Vec<char>,
    cursor: usize,
    /// The other end of the selection, if any.
    anchor: Option<usize>,
}

impl TextBox {
    fn selection(&self) -> Option<(usize, usize)> {
        self.anchor
            .filter(|a| *a != self.cursor)
            .map(|a| (a.min(self.cursor), a.max(self.cursor)))
    }

    fn selected_text(&self) -> Option<String> {
        self.selection().map(|(a, b)| self.text[a..b].iter().collect())
    }

    /// Remove the selected text. Returns whether anything was removed.
    fn delete_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        self.text.drain(a..b);
        self.cursor = a;
        self.anchor = None;
        true
    }

    fn insert(&mut self, text: &str) {
        self.delete_selection();
        for c in text.chars().filter(|c| !c.is_control()) {
            self.text.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    /// Start of the word before `from` (for Ctrl+Left / Ctrl+Backspace).
    fn word_left(&self, from: usize) -> usize {
        let mut i = from;
        while i > 0 && self.text[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.text[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn word_right(&self, from: usize) -> usize {
        let n = self.text.len();
        let mut i = from;
        while i < n && !self.text[i].is_whitespace() {
            i += 1;
        }
        while i < n && self.text[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Move the caret, extending the selection when `select` (Shift) is held.
    fn move_to(&mut self, pos: usize, select: bool) {
        if select {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = pos.min(self.text.len());
    }
}

static TEXT_BOX: Mutex<Option<TextBox>> = Mutex::new(None);

/// Read a `NativeTextBoxInfo` object into overlay geometry.
fn geometry_of(info: Option<&lucid_jni::Ref>) -> Option<Geometry> {
    let i = info?.as_instance()?;
    let f = |name: &str| i.get(name).map(|v| v.as_f64() as f32).unwrap_or(0.0);
    let n = |name: &str| i.get(name).map(|v| v.as_i64()).unwrap_or(0);
    let g = Geometry {
        x: f("x"),
        y: f("y"),
        width: f("width"),
        height: f("height"),
        font_size: f("fontSize"),
        color: n("textColor") as u32,
        x_align: n("xAlignment") as i32,
        // Enum.TextInputType.Password
        password: n("textInputType") == 5,
    };
    (g.width > 0.0 && g.height > 0.0).then_some(g)
}

/// Push the current text box contents to the overlay.
fn show_text(tb: &TextBox) {
    let text: String = tb.text.iter().collect();
    overlay::set_text(&text, tb.cursor, tb.selection());
}

/// Register the text box callbacks the engine calls.
pub fn install() {
    let vm = vm();
    vm.on(
        GL_JAVA,
        "showKeyboard",
        "(JZ[BLcom/roblox/engine/jni/model/NativeTextBoxInfo;)V",
        |_, a| {
            let handle = a[0].as_i64();
            let text: Vec<char> = a
                .get(2)
                .and_then(JValue::as_object)
                .and_then(|o| o.as_prim_array())
                .map(|p| String::from_utf8_lossy(p.bytes()).chars().collect())
                .unwrap_or_default();
            log::debug!("text box {handle:#x} focused (native overlay: {})", a[1].as_i64() != 0);
            let cursor = text.len();
            let tb = TextBox { handle, text, cursor, anchor: None };
            // With "native overlay" set, the engine leaves drawing the
            // text being edited to the platform.
            if a[1].as_i64() != 0 {
                show_text(&tb);
                if let Some(g) = geometry_of(a.get(3).and_then(JValue::as_object)) {
                    overlay::show(g);
                }
            }
            *TEXT_BOX.lock().unwrap() = Some(tb);
        },
    );
    // The engine describes the focused text box by constructing this.
    const INFO: &str = "com/roblox/engine/jni/model/NativeTextBoxInfo";
    const NAMES: [&str; 19] = [
        "x", "y", "width", "height", "fontSize", "multiline", "xAlignment", "yAlignment",
        "textColor", "font", "textInputType", "returnKeyType", "manualFocusRelease",
        "textWrapped", "editable", "paddingLeft", "paddingTop", "paddingRight", "paddingBottom",
    ];
    for sig in ["(FFFFFZIIIIIIZZZ)V", "(FFFFFZIIIIIIZZZFFFF)V"] {
        vm.on(INFO, "<init>", sig, |this, a| {
            if let Some(i) = this.and_then(|t| t.as_instance()) {
                for (name, v) in NAMES.iter().zip(a) {
                    i.set(name, v.clone());
                }
                log::debug!("text box info {a:?}");
            }
        });
    }
    vm.on(GL_JAVA, "hideKeyboard", "()V", |_, _| {
        log::debug!("text box released");
        *TEXT_BOX.lock().unwrap() = None;
        overlay::hide();
    });
    vm.on(
        GL_JAVA,
        "onLuaTextBoxChangedCallback",
        "(Ljava/lang/String;)V",
        |_, a| {
            if let Some(tb) = TEXT_BOX.lock().unwrap().as_mut() {
                let text: Vec<char> = a[0].as_str().unwrap_or("").chars().collect();
                if text != tb.text {
                    tb.cursor = tb.cursor.min(text.len());
                    tb.anchor = None;
                    tb.text = text;
                    show_text(tb);
                }
            }
        },
    );
    // The box moved or was restyled: fetch the new geometry (on another
    // thread, since this is called from inside the engine).
    vm.on(GL_JAVA, "onLuaTextBoxPropertyChangedCallback", "()V", |_, _| {
        if TEXT_BOX.lock().unwrap().is_none() {
            return;
        }
        std::thread::spawn(|| {
            let info = with_frame(|| {
                let r = jcall!(
                    GL,
                    "nativeGetTextBoxInfo",
                    "()Lcom/roblox/engine/jni/model/NativeTextBoxInfo;",
                    fn() -> JObject,
                )?;
                unsafe { lucid_jni::from_raw(r) }
            });
            if let Some(g) = geometry_of(info.as_ref()) {
                if TEXT_BOX.lock().unwrap().is_some() {
                    overlay::show(g);
                }
            }
        });
    });
}

/// Android `KEYCODE_*` for a physical key.
fn android_keycode(code: KeyCode) -> i32 {
    use KeyCode::*;
    match code {
        KeyA => 29, KeyB => 30, KeyC => 31, KeyD => 32, KeyE => 33, KeyF => 34, KeyG => 35,
        KeyH => 36, KeyI => 37, KeyJ => 38, KeyK => 39, KeyL => 40, KeyM => 41, KeyN => 42,
        KeyO => 43, KeyP => 44, KeyQ => 45, KeyR => 46, KeyS => 47, KeyT => 48, KeyU => 49,
        KeyV => 50, KeyW => 51, KeyX => 52, KeyY => 53, KeyZ => 54,
        Digit0 => 7, Digit1 => 8, Digit2 => 9, Digit3 => 10, Digit4 => 11, Digit5 => 12,
        Digit6 => 13, Digit7 => 14, Digit8 => 15, Digit9 => 16,
        Enter => 66, Escape => 111, Backspace => 67, Tab => 61, Space => 62,
        Minus => 69, Equal => 70, BracketLeft => 71, BracketRight => 72, Backslash => 73,
        Semicolon => 74, Quote => 75, Backquote => 68, Comma => 55, Period => 56, Slash => 76,
        CapsLock => 115,
        F1 => 131, F2 => 132, F3 => 133, F4 => 134, F5 => 135, F6 => 136, F7 => 137,
        F8 => 138, F9 => 139, F10 => 140, F11 => 141, F12 => 142,
        ArrowUp => 19, ArrowDown => 20, ArrowLeft => 21, ArrowRight => 22,
        ShiftLeft => 59, ShiftRight => 60, ControlLeft => 113, ControlRight => 114,
        AltLeft => 57, AltRight => 58, SuperLeft => 117, SuperRight => 118,
        Delete => 112, Insert => 124, Home => 122, End => 123, PageUp => 92, PageDown => 93,
        Numpad0 => 144, Numpad1 => 145, Numpad2 => 146, Numpad3 => 147, Numpad4 => 148,
        Numpad5 => 149, Numpad6 => 150, Numpad7 => 151, Numpad8 => 152, Numpad9 => 153,
        NumpadDivide => 154, NumpadMultiply => 155, NumpadSubtract => 156, NumpadAdd => 157,
        NumpadDecimal => 158, NumpadEnter => 160, NumLock => 143,
        PrintScreen => 120, ScrollLock => 116, Pause => 121,
        _ => 0,
    }
}

#[derive(Default)]
pub struct Input {
    /// Last pointer position in dp.
    x: f32,
    y: f32,
    lock_checked: Option<std::time::Instant>,
    lock_wanted: bool,
    ctrl: bool,
    shift: bool,
    clipboard: Option<Clipboard>,
    /// Keys the engine currently believes are down: (scancode, keycode).
    held: Vec<(i32, i32)>,
}

fn push_text(handle: i64, text: &str, enter: bool, cursor: usize) {
    with_frame(|| {
        jcall!(
            GL,
            "syncTextboxTextAndCursorPosition2",
            "(Ljava/lang/String;I)V",
            fn(JObject, i32),
            jstr(text),
            cursor as i32
        );
        jcall!(
            GL,
            "nativePassText",
            "(JLjava/lang/String;ZI)V",
            fn(i64, JObject, u8, i32),
            handle,
            jstr(text),
            enter as u8,
            cursor as i32
        );
    });
}

/// Edit the focused text box with a key press. Returns false if no text box
/// has focus (the key should then go to the game as a normal key event).
fn edit_text(event: &KeyEvent, ctrl: bool, shift: bool, clipboard: Option<&Clipboard>) -> bool {
    let mut guard = TEXT_BOX.lock().unwrap();
    let Some(tb) = guard.as_mut() else {
        return false;
    };
    if event.state != ElementState::Pressed {
        // Let releases through, so a key that was held when the text box
        // took focus (like the "/" that opens chat) does not stay down.
        return false;
    }
    let handle = tb.handle;
    let before = (tb.text.clone(), tb.cursor, tb.anchor);
    let mut enter = false;
    let mut release = false;
    let len = tb.text.len();
    // Shortcuts are matched on the character, so they follow the layout.
    let letter = match &event.logical_key {
        Key::Character(c) if ctrl => c.to_lowercase().chars().next(),
        _ => None,
    };
    match (&event.logical_key, letter) {
        (_, Some('a')) => {
            tb.anchor = Some(0);
            tb.cursor = len;
        }
        (_, Some('c')) => {
            if let (Some(cb), Some(text)) = (clipboard, tb.selected_text()) {
                cb.store(text);
            }
        }
        (_, Some('x')) => {
            if let (Some(cb), Some(text)) = (clipboard, tb.selected_text()) {
                cb.store(text);
                tb.delete_selection();
            }
        }
        (_, Some('v')) => {
            if let Some(text) = clipboard.and_then(|cb| cb.load().ok()) {
                tb.insert(&text);
            }
        }
        (_, Some(_)) => {}
        (Key::Named(NamedKey::Backspace), _) => {
            if !tb.delete_selection() && tb.cursor > 0 {
                let from = if ctrl { tb.word_left(tb.cursor) } else { tb.cursor - 1 };
                tb.text.drain(from..tb.cursor);
                tb.cursor = from;
            }
        }
        (Key::Named(NamedKey::Delete), _) => {
            if !tb.delete_selection() && tb.cursor < len {
                let to = if ctrl { tb.word_right(tb.cursor) } else { tb.cursor + 1 };
                tb.text.drain(tb.cursor..to);
            }
        }
        (Key::Named(NamedKey::ArrowLeft), _) => {
            let pos = match tb.selection() {
                // Without Shift, a selection collapses to its start.
                Some((a, _)) if !shift => a,
                _ if ctrl => tb.word_left(tb.cursor),
                _ => tb.cursor.saturating_sub(1),
            };
            tb.move_to(pos, shift);
        }
        (Key::Named(NamedKey::ArrowRight), _) => {
            let pos = match tb.selection() {
                Some((_, b)) if !shift => b,
                _ if ctrl => tb.word_right(tb.cursor),
                _ => tb.cursor + 1,
            };
            tb.move_to(pos, shift);
        }
        (Key::Named(NamedKey::Home), _) => tb.move_to(0, shift),
        (Key::Named(NamedKey::End), _) => tb.move_to(len, shift),
        (Key::Named(NamedKey::Enter), _) => enter = true,
        (Key::Named(NamedKey::Escape), _) => release = true,
        _ => {
            if let Some(text) = &event.text {
                if text.chars().any(|c| !c.is_control()) {
                    tb.insert(text);
                }
            }
        }
    }
    let text_changed = tb.text != before.0;
    let changed = text_changed || tb.cursor != before.1 || tb.anchor != before.2;
    let text: String = tb.text.iter().collect();
    // UTF-16 offset, as Java's selection index would be.
    let cursor = tb.text[..tb.cursor].iter().map(|c| c.len_utf16()).sum::<usize>();
    if enter || release {
        *guard = None;
        overlay::hide();
    } else if changed {
        show_text(tb);
    }
    drop(guard);

    if enter {
        with_frame(|| {
            jcall!(GL, "nativeReturnPressedFromOnScreenKeyboard", "(J)V", fn(i64), handle);
        });
        push_text(handle, &text, true, cursor);
    } else if release {
        with_frame(|| {
            jcall!(GL, "nativeReleaseFocus", "(J)V", fn(i64), handle);
        });
    } else if text_changed || tb_cursor_moved(before.1, &before.0, cursor) {
        push_text(handle, &text, false, cursor);
    }
    true
}

/// Whether the caret's UTF-16 offset differs from where it was.
fn tb_cursor_moved(old_cursor: usize, old_text: &[char], new_utf16: usize) -> bool {
    old_text[..old_cursor.min(old_text.len())].iter().map(|c| c.len_utf16()).sum::<usize>() != new_utf16
}

impl Input {
    /// Whether the engine has locked the mouse to the window centre
    /// (polled, like the Android app does, at most every 50 ms).
    pub fn wants_pointer_lock(&mut self) -> bool {
        let now = std::time::Instant::now();
        if self.lock_checked.is_none_or(|t| now.duration_since(t).as_millis() >= 50) {
            self.lock_checked = Some(now);
            self.lock_wanted = with_frame(|| {
                jcall!(INPUT, "nativeGetMainWindowIsMouseLockedCenter", "()Z", fn() -> u8,)
            })
            .unwrap_or(0)
                != 0;
        }
        self.lock_wanted
    }

    /// Relative motion while the pointer is captured.
    pub fn relative_motion(&mut self, dx: f64, dy: f64, window: &NativeWindow) {
        let scale = 160.0 / window.density().max(1) as f32;
        let (dx, dy) = (dx as f32 * scale, dy as f32 * scale);
        let (x, y) = (self.x, self.y);
        with_frame(|| {
            jcall!(INPUT, "nativePassMouseMove", "(FFFF)V", fn(f32, f32, f32, f32), x, y, dx, dy);
        });
    }

    pub fn handle(&mut self, event: &WindowEvent, window: &NativeWindow) {
        let scale = 160.0 / window.density().max(1) as f32;
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = (position.x as f32 * scale, position.y as f32 * scale);
                let (dx, dy) = (x - self.x, y - self.y);
                self.x = x;
                self.y = y;
                with_frame(|| {
                    jcall!(INPUT, "nativePassMouseMove", "(FFFF)V", fn(f32, f32, f32, f32), x, y, dx, dy);
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // Android's `getActionButton() - 1`.
                let b = match button {
                    MouseButton::Left => 0,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 3,
                    _ => return,
                };
                let pressed = (*state == ElementState::Pressed) as u8;
                let (x, y) = (self.x, self.y);
                with_frame(|| {
                    jcall!(INPUT, "nativePassMouseButton", "(FFZI)V", fn(f32, f32, u8, i32), x, y, pressed, b);
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let d = match delta {
                    MouseScrollDelta::LineDelta(_, y) => *y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 40.0,
                };
                let (x, y) = (self.x, self.y);
                with_frame(|| {
                    jcall!(INPUT, "nativePassMouseWheel", "(FFF)V", fn(f32, f32, f32), x, y, d);
                });
            }
            WindowEvent::Focused(false) => {
                // The window system keeps the key-up events of whatever was
                // held when focus left (e.g. Alt of Alt+Tab), so release
                // everything ourselves or the engine sees those keys as
                // stuck down.
                for (scancode, keycode) in std::mem::take(&mut self.held) {
                    with_frame(|| {
                        jcall!(GL, "nativePassKeyEvent", "(ZIIZ)V", fn(u8, i32, i32, u8), 0, scancode, keycode, 0);
                    });
                }
                self.ctrl = false;
                self.shift = false;
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.ctrl = mods.state().control_key();
                self.shift = mods.state().shift_key();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if self.clipboard.is_none() {
                    // The clipboard shares the window's Wayland connection.
                    if let lucid_android::window::Backend::Wayland { display, .. } = window.backend {
                        self.clipboard = Some(unsafe { Clipboard::new(display as *mut _) });
                    }
                }
                if edit_text(event, self.ctrl, self.shift, self.clipboard.as_ref()) {
                    return;
                }
                let code = match event.physical_key {
                    PhysicalKey::Code(code) => code,
                    PhysicalKey::Unidentified(_) => KeyCode::Fn,
                };
                let mut scancode = event.physical_key.to_scancode().unwrap_or(0) as i32;
                let mut keycode = android_keycode(code);
                // The engine reads keys by position, but "/" (which opens
                // chat) sits on a different key on some layouts (ABNT2,
                // most ISO ones): follow the typed character for it.
                if matches!(&event.logical_key, Key::Character(c) if c.as_str() == "/") {
                    scancode = 53; // KEY_SLASH
                    keycode = 76; // KEYCODE_SLASH
                }
                let down = (event.state == ElementState::Pressed) as u8;
                let repeat = event.repeat as u8;
                self.held.retain(|k| *k != (scancode, keycode));
                if down != 0 {
                    self.held.push((scancode, keycode));
                }
                with_frame(|| {
                    jcall!(GL, "nativePassKeyEvent", "(ZIIZ)V", fn(u8, i32, i32, u8), down, scancode, keycode, repeat);
                });
            }
            _ => {}
        }
    }
}

/// Debug aid: `LUCID_TEST_KEYS="scan:key,scan:key"` taps those key pairs,
/// one every 4 seconds, starting 25 seconds after launch.
pub fn spawn_test_keys() {
    let Ok(spec) = std::env::var("LUCID_TEST_KEYS") else {
        return;
    };
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(25));
        for pair in spec.split(',') {
            let mut it = pair.split(':').map(|v| v.parse::<i32>().unwrap_or(0));
            let (scan, key) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
            eprintln!("TESTKEY {} scan {scan} key {key}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
            for down in [1u8, 0] {
                with_frame(|| {
                    jcall!(GL, "nativePassKeyEvent", "(ZIIZ)V", fn(u8, i32, i32, u8), down, scan, key, 0);
                });
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
            std::thread::sleep(std::time::Duration::from_secs(8));
        }
    });
}

/// Debug aid: `LUCID_TEST_CLICK="x,y;x,y"` clicks those points (in dp),
/// one every 3 seconds, starting 20 seconds after launch.
pub fn spawn_test_clicks() {
    let Ok(spec) = std::env::var("LUCID_TEST_CLICK") else {
        return;
    };
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(20));
        for point in spec.split(';') {
            let mut it = point.split(',').map(|v| v.parse::<f32>().unwrap_or(0.0));
            let (x, y) = (it.next().unwrap_or(0.0), it.next().unwrap_or(0.0));
            log::info!("test click at {x},{y}");
            with_frame(|| {
                jcall!(INPUT, "nativePassMouseMove", "(FFFF)V", fn(f32, f32, f32, f32), x, y, 0.0, 0.0);
            });
            for down in [1u8, 0] {
                std::thread::sleep(std::time::Duration::from_millis(80));
                with_frame(|| {
                    jcall!(INPUT, "nativePassMouseButton", "(FFZI)V", fn(f32, f32, u8, i32), x, y, down, 0);
                });
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
            // Optionally type into whatever got focus and capture the
            // result, to check that typed text is visible.
            if let Ok(text) = std::env::var("LUCID_TEST_TYPE") {
                let handle = TEXT_BOX.lock().unwrap().as_ref().map(|t| t.handle);
                if let Some(handle) = handle {
                    log::info!("test typing into {handle:#x}");
                    for n in 1..=text.chars().count() {
                        let part: String = text.chars().take(n).collect();
                        push_text(handle, &part, false, n);
                        overlay::set_text(&part, n, None);
                        std::thread::sleep(std::time::Duration::from_millis(120));
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
                with_frame(|| {
                    jcall!(GL, "takeScreenshot", "()V", fn(),);
                });
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
    });
}
