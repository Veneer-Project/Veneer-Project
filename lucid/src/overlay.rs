//! The native text overlay.
//!
//! On Android, text being typed into an engine text box is not drawn by the
//! engine: the app lays a native `EditText` exactly over the box and the
//! engine hides its own text while that is active. This module is that
//! `EditText`: a transparent Wayland sub-surface over the game surface,
//! with the text and caret drawn in software using Roblox's UI font.

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};
use lucid_android::window::Backend as WindowBackend;
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_subcompositor,
    wl_subsurface, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop};

use crate::paths;

/// Where and how to draw, in the engine's dp units (window-relative).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub font_size: f32,
    /// Android colour int (0xAARRGGBB).
    pub color: u32,
    /// 0 = left, 1 = right, 2 = centre (Roblox `TextXAlignment`).
    pub x_align: i32,
    pub password: bool,
}

enum Command {
    Show(Geometry),
    Text { text: String, cursor: usize, selection: Option<(usize, usize)> },
    Hide,
}

static SENDER: OnceLock<Option<Sender<Command>>> = OnceLock::new();

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: wl_compositor::WlCompositor);
delegate_noop!(State: wl_subcompositor::WlSubcompositor);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: wl_subsurface::WlSubsurface);
delegate_noop!(State: wl_region::WlRegion);

/// One shared-memory frame.
struct Frame {
    buffer: wl_buffer::WlBuffer,
    map: *mut u8,
    len: usize,
    _fd: OwnedFd,
}

impl Drop for Frame {
    fn drop(&mut self) {
        self.buffer.destroy();
        unsafe { libc::munmap(self.map as *mut libc::c_void, self.len) };
    }
}

struct Overlay {
    queue: wayland_client::EventQueue<State>,
    qh: QueueHandle<State>,
    shm: wl_shm::WlShm,
    surface: wl_surface::WlSurface,
    subsurface: wl_subsurface::WlSubsurface,
    font: FontVec,
    scale: i32,
    geometry: Option<Geometry>,
    text: String,
    cursor: usize,
    selection: Option<(usize, usize)>,
    frame: Option<Frame>,
}

fn load_font() -> Option<FontVec> {
    for name in ["BuilderSans-Regular.otf", "SourceSansPro-Regular.ttf"] {
        for dir in ["content/fonts", "fonts"] {
            if let Ok(bytes) = std::fs::read(paths::assets_dir().join(dir).join(name)) {
                if let Ok(font) = FontVec::try_from_vec(bytes) {
                    return Some(font);
                }
            }
        }
    }
    None
}

impl Overlay {
    fn new(display: usize, parent: usize, scale: f64) -> Result<Overlay, Box<dyn std::error::Error>> {
        // Share the window's Wayland connection, with our own event queue.
        let backend = unsafe { Backend::from_foreign_display(display as *mut _) };
        let conn = Connection::from_backend(backend);
        let (globals, queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ())?;
        let subcompositor: wl_subcompositor::WlSubcompositor = globals.bind(&qh, 1..=1, ())?;
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;

        let parent_id =
            unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), parent as *mut _) }?;
        let parent = wl_surface::WlSurface::from_id(&conn, parent_id)?;

        let surface = compositor.create_surface(&qh, ());
        let subsurface = subcompositor.get_subsurface(&surface, &parent, &qh, ());
        subsurface.set_desync();
        // Empty input region: clicks go through to the game.
        let region = compositor.create_region(&qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        let scale = (scale.round() as i32).max(1);
        surface.set_buffer_scale(scale);

        Ok(Overlay {
            queue,
            qh,
            shm,
            surface,
            subsurface,
            font: load_font().ok_or("no UI font found in the game assets")?,
            scale,
            geometry: None,
            text: String::new(),
            cursor: 0,
            selection: None,
            frame: None,
        })
    }

    fn flush(&mut self) {
        let _ = self.queue.roundtrip(&mut State);
    }

    fn hide(&mut self) {
        self.geometry = None;
        self.surface.attach(None, 0, 0);
        self.surface.commit();
        self.flush();
        self.frame = None;
    }

    fn draw(&mut self) {
        let Some(g) = self.geometry else {
            return;
        };
        let s = self.scale;
        let (w, h) = ((g.width.ceil() as i32 * s).max(1), (g.height.ceil() as i32 * s).max(1));
        if g.width < 4.0 || g.height < 4.0 {
            return;
        }
        let stride = w * 4;
        let len = (stride * h) as usize;
        let fd = unsafe { libc::memfd_create(c"lucid-text".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 || unsafe { libc::ftruncate(fd, len as libc::off_t) } != 0 {
            return;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return;
        }
        let pixels = unsafe { std::slice::from_raw_parts_mut(map as *mut u32, (w * h) as usize) };
        pixels.fill(0);

        let shown: String = if g.password {
            "\u{2022}".repeat(self.text.chars().count())
        } else {
            self.text.clone()
        };
        let (r, gr, b) = ((g.color >> 16) & 0xff, (g.color >> 8) & 0xff, g.color & 0xff);

        // Roblox text sizes are the font's line height in pixels.
        let font = self.font.as_scaled(PxScale::from(g.font_size * s as f32));
        let baseline = (h as f32 - font.height()) / 2.0 + font.ascent();
        // Lay the glyphs out; `stops[i]` is the x position before char i.
        let mut pen = 0.0f32;
        let mut glyphs = Vec::new();
        let mut stops = vec![0.0f32];
        let mut last = None;
        for c in shown.chars() {
            let id = font.glyph_id(c);
            if let Some(prev) = last {
                pen += font.kern(prev, id);
            }
            glyphs.push((id, pen));
            pen += font.h_advance(id);
            stops.push(pen);
            last = Some(id);
        }
        let stop = |i: usize| stops[i.min(stops.len() - 1)];
        let caret = stop(self.cursor);
        let text_width = pen;
        // Alignment, then scroll so the caret stays inside the box.
        let margin = 2.0 * s as f32;
        let mut origin = match g.x_align {
            1 => w as f32 - text_width - margin,
            2 => (w as f32 - text_width) / 2.0,
            _ => margin,
        };
        if origin + caret > w as f32 - margin {
            origin = w as f32 - margin - caret;
        }
        if origin + caret < margin {
            origin = margin - caret;
        }

        let (top, bottom) = ((baseline - font.ascent()) as i32, (baseline - font.descent()) as i32);
        // Selection highlight, behind the glyphs.
        if let Some((from, to)) = self.selection.filter(|(a, b)| a != b) {
            let (x0, x1) = ((origin + stop(from)) as i32, (origin + stop(to)) as i32);
            for y in top.max(0)..bottom.min(h) {
                for x in x0.max(0)..x1.min(w) {
                    // Premultiplied translucent blue.
                    pixels[(y * w + x) as usize] = 0x99_1f_5c_99;
                }
            }
        }
        let mut blend = |x: i32, y: i32, coverage: f32| {
            if x < 0 || y < 0 || x >= w || y >= h {
                return;
            }
            let a = (coverage.clamp(0.0, 1.0) * 255.0) as u32;
            if a == 0 {
                return;
            }
            let p = &mut pixels[(y * w + x) as usize];
            // Source-over, premultiplied ARGB.
            let inv = 255 - a;
            let ch = |shift: u32, src: u32| ((src * a + ((*p >> shift) & 0xff) * inv) / 255) << shift;
            *p = ch(24, 255) | ch(16, r) | ch(8, gr) | ch(0, b);
        };
        for (id, x) in glyphs {
            let glyph = id.with_scale_and_position(font.scale(), point(origin + x, baseline));
            if let Some(outline) = self.font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, c| {
                    blend(bounds.min.x as i32 + gx as i32, bounds.min.y as i32 + gy as i32, c)
                });
            }
        }
        // Caret.
        let cx = (origin + caret) as i32;
        for y in top..bottom {
            for dx in 0..s.max(1) {
                blend(cx + dx, y, 1.0);
            }
        }

        let pool = self.shm.create_pool(fd.as_fd(), len as i32, &self.qh, ());
        let buffer = pool.create_buffer(0, w, h, stride, wl_shm::Format::Argb8888, &self.qh, ());
        pool.destroy();
        self.subsurface.set_position(g.x.round() as i32, g.y.round() as i32);
        self.surface.attach(Some(&buffer), 0, 0);
        self.surface.damage(0, 0, i32::MAX, i32::MAX);
        self.surface.commit();
        self.flush();
        // The previous frame can go now that a new one is attached.
        self.frame = Some(Frame {
            buffer,
            map: map as *mut u8,
            len,
            _fd: fd,
        });
    }
}

fn sender() -> Option<&'static Sender<Command>> {
    SENDER
        .get_or_init(|| {
            let window = lucid_android::window::window()?;
            let WindowBackend::Wayland { display, surface } = window.backend else {
                log::warn!("typed text is only shown on Wayland for now");
                return None;
            };
            let scale = window.density() as f64 / 160.0;
            let (tx, rx) = channel::<Command>();
            std::thread::spawn(move || {
                let mut overlay = match Overlay::new(display, surface, scale) {
                    Ok(o) => o,
                    Err(e) => {
                        log::warn!("text overlay unavailable: {e}");
                        return;
                    }
                };
                for cmd in rx {
                    match cmd {
                        Command::Show(g) => {
                            overlay.geometry = Some(g);
                            overlay.draw();
                        }
                        Command::Text { text, cursor, selection } => {
                            overlay.text = text;
                            overlay.cursor = cursor;
                            overlay.selection = selection;
                            overlay.draw();
                        }
                        Command::Hide => overlay.hide(),
                    }
                }
            });
            Some(tx)
        })
        .as_ref()
}

/// Show the overlay (or move/restyle it).
pub fn show(g: Geometry) {
    if let Some(tx) = sender() {
        let _ = tx.send(Command::Show(g));
    }
}

/// Set the text, the caret position and the selected range (in characters).
pub fn set_text(text: &str, cursor: usize, selection: Option<(usize, usize)>) {
    if let Some(tx) = sender() {
        let _ = tx.send(Command::Text {
            text: text.to_string(),
            cursor,
            selection,
        });
    }
}

pub fn hide() {
    if let Some(tx) = SENDER.get().and_then(|s| s.as_ref()) {
        let _ = tx.send(Command::Hide);
    }
}
