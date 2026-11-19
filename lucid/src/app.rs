//! The host window and event loop (the stand-in for Android's UI thread).

use std::sync::Arc;

use lucid_android::window::{Backend, NativeWindow, set_window};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};
use winit::window::{CursorGrabMode, Window, WindowId};

/// Callbacks from the window into the engine glue.
pub trait Frontend {
    /// The window exists; the engine may start.
    fn started(&mut self, window: &'static NativeWindow);
    fn resized(&mut self, width: i32, height: i32);
    /// Any other window event (input).
    fn event(&mut self, event: &WindowEvent, window: &'static NativeWindow);
    /// Relative pointer motion (used while the pointer is locked).
    fn pointer_motion(&mut self, dx: f64, dy: f64, window: &'static NativeWindow);
    /// Whether the engine wants the pointer captured (camera control).
    fn wants_pointer_lock(&mut self) -> bool;
    fn closing(&mut self);
}

struct App<F: Frontend> {
    frontend: F,
    window: Option<Arc<Window>>,
    native: Option<&'static NativeWindow>,
    pointer_locked: bool,
}

fn backend_of(window: &Window) -> Option<Backend> {
    let wh = window.window_handle().ok()?.as_raw();
    let dh = window.display_handle().ok()?.as_raw();
    match (wh, dh) {
        (RawWindowHandle::Wayland(w), RawDisplayHandle::Wayland(d)) => Some(Backend::Wayland {
            display: d.display.as_ptr() as usize,
            surface: w.surface.as_ptr() as usize,
        }),
        (RawWindowHandle::Xlib(w), RawDisplayHandle::Xlib(d)) => Some(Backend::Xlib {
            display: d.display?.as_ptr() as usize,
            window: w.window,
        }),
        _ => None,
    }
}

impl<F: Frontend> ApplicationHandler for App<F> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Roblox")
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("lucid: cannot create a window: {e}");
                event_loop.exit();
                return;
            }
        };
        let Some(backend) = backend_of(&window) else {
            eprintln!("lucid: unsupported window system (need Wayland or X11/Xlib)");
            event_loop.exit();
            return;
        };
        let size = window.inner_size();
        log::info!(
            "window: {backend:?} {}x{} scale {}",
            size.width,
            size.height,
            window.scale_factor()
        );
        let native = set_window(
            backend,
            size.width as i32,
            size.height as i32,
            window.scale_factor(),
        );
        // The engine draws its own cursor.
        window.set_cursor_visible(false);
        self.window = Some(window);
        self.native = Some(native);
        self.frontend.started(native);
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let (DeviceEvent::MouseMotion { delta }, Some(native)) = (event, self.native) {
            self.sync_pointer_lock();
            if self.pointer_locked {
                self.frontend.pointer_motion(delta.0, delta.1, native);
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.frontend.closing();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(native) = self.native {
                    native.resize(size.width as i32, size.height as i32);
                }
                self.frontend.resized(size.width as i32, size.height as i32);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(native) = self.native {
                    native.set_scale(scale_factor);
                }
            }
            other => {
                // While locked, motion comes from relative device events.
                if self.pointer_locked && matches!(other, WindowEvent::CursorMoved { .. }) {
                    return;
                }
                if let Some(native) = self.native {
                    self.frontend.event(&other, native);
                }
            }
        }
    }
}

impl<F: Frontend> App<F> {
    fn sync_pointer_lock(&mut self) {
        let want = self.frontend.wants_pointer_lock();
        if want == self.pointer_locked {
            return;
        }
        let Some(window) = &self.window else {
            return;
        };
        let result = if want {
            window
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| window.set_cursor_grab(CursorGrabMode::Confined))
        } else {
            window.set_cursor_grab(CursorGrabMode::None)
        };
        match result {
            Ok(()) => self.pointer_locked = want,
            Err(e) => log::debug!("pointer grab change failed: {e}"),
        }
    }
}

/// Run the event loop on the calling (main) thread until the window closes.
pub fn run<F: Frontend>(frontend: F) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new()?;
    let mut app = App {
        frontend,
        window: None,
        native: None,
        pointer_locked: false,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}
