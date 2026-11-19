//! Mouse, keyboard and text input: host window events translated into the
//! calls Android's input code makes.

use std::sync::Mutex;

use lucid_android::window::NativeWindow;
use lucid_jni::{JObject, JValue, vm, with_frame};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use winit::platform::scancode::PhysicalKeyExtScancode;

use crate::java::jstr;
use crate::jcall;

const INPUT: &str = "com/roblox/engine/jni/NativeInputInterface";
const GL: &str = "com/roblox/engine/jni/NativeGLInterface";
const GL_JAVA: &str = "com/roblox/engine/jni/NativeGLJavaInterface";

/// The engine text box currently being edited (Android shows an invisible
/// `EditText` for it; we keep the equivalent editing state here).
struct TextBox {
    handle: i64,
    text: Vec<char>,
    cursor: usize,
}

static TEXT_BOX: Mutex<Option<TextBox>> = Mutex::new(None);

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
            log::debug!("text box {handle:#x} focused");
            let cursor = text.len();
            *TEXT_BOX.lock().unwrap() = Some(TextBox { handle, text, cursor });
        },
    );
    vm.on(GL_JAVA, "hideKeyboard", "()V", |_, _| {
        log::debug!("text box released");
        *TEXT_BOX.lock().unwrap() = None;
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
                    tb.text = text;
                }
            }
        },
    );
    vm.on(GL_JAVA, "onLuaTextBoxPropertyChangedCallback", "()V", |_, _| ());
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
fn edit_text(event: &KeyEvent) -> bool {
    let mut guard = TEXT_BOX.lock().unwrap();
    let Some(tb) = guard.as_mut() else {
        return false;
    };
    if event.state != ElementState::Pressed {
        return true;
    }
    let handle = tb.handle;
    let mut changed = false;
    let mut enter = false;
    let mut release = false;
    match &event.logical_key {
        Key::Named(NamedKey::Backspace) => {
            if tb.cursor > 0 {
                tb.cursor -= 1;
                tb.text.remove(tb.cursor);
                changed = true;
            }
        }
        Key::Named(NamedKey::Delete) => {
            if tb.cursor < tb.text.len() {
                tb.text.remove(tb.cursor);
                changed = true;
            }
        }
        Key::Named(NamedKey::ArrowLeft) => {
            tb.cursor = tb.cursor.saturating_sub(1);
            changed = true;
        }
        Key::Named(NamedKey::ArrowRight) => {
            tb.cursor = (tb.cursor + 1).min(tb.text.len());
            changed = true;
        }
        Key::Named(NamedKey::Home) => {
            tb.cursor = 0;
            changed = true;
        }
        Key::Named(NamedKey::End) => {
            tb.cursor = tb.text.len();
            changed = true;
        }
        Key::Named(NamedKey::Enter) => enter = true,
        Key::Named(NamedKey::Escape) => release = true,
        _ => {
            if let Some(text) = &event.text {
                for c in text.chars().filter(|c| !c.is_control()) {
                    tb.text.insert(tb.cursor, c);
                    tb.cursor += 1;
                    changed = true;
                }
            }
        }
    }
    let text: String = tb.text.iter().collect();
    // UTF-16 offset, as Java's selection index would be.
    let cursor = tb.text[..tb.cursor].iter().map(|c| c.len_utf16()).sum::<usize>();
    if enter || release {
        *guard = None;
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
    } else if changed {
        push_text(handle, &text, false, cursor);
    }
    true
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
            WindowEvent::KeyboardInput { event, .. } => {
                if edit_text(event) {
                    return;
                }
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let scancode = event.physical_key.to_scancode().unwrap_or(0) as i32;
                let keycode = android_keycode(code);
                let down = (event.state == ElementState::Pressed) as u8;
                let repeat = event.repeat as u8;
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
