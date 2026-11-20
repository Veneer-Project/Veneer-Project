//! Veneer's name and icon on the desktop. What people see is called Veneer;
//! the app ID, launcher file and icon name keep the codename `lucid`.
//!
//! How a desktop finds a window's icon:
//! - Wayland (GNOME, KDE Plasma, Budgie 10.10+ and others): a window cannot
//!   carry its own icon. The compositor matches the window's app ID to a
//!   launcher named `<app id>.desktop` and shows that launcher's `Icon=`.
//!   Compositors with the `xdg-toplevel-icon-v1` protocol (KDE Plasma 6.1+
//!   and some others; not GNOME) also accept an icon from the window itself.
//! - X11 (GNOME on Xorg, Budgie 10, Xfce, ...): the window's `WM_CLASS` is
//!   matched to a launcher's `StartupWMClass=`; when no launcher matches,
//!   the icon stored on the window itself (`_NET_WM_ICON`) is used.
//!
//! So every window gets the app ID / `WM_CLASS` [`APP_ID`] and, where the
//! window system allows, its own icon; `veneer desktop-entry` installs the
//! launcher and the icon theme entries for the rest (GNOME on Wayland).

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::paths;

/// The title of lucid's windows.
pub const NAME: &str = "Veneer";
/// Wayland app ID and X11 `WM_CLASS`; also the launcher's and the icon's file
/// name. Never shown to people, so it keeps the codename.
pub const APP_ID: &str = "lucid";

/// The logo (`assets/logo.svg`, the same image as the repository's icon),
/// rendered at the usual icon theme sizes.
const ICONS: &[(u32, &[u8])] = &[
    (16, include_bytes!("../../assets/icons/16.png")),
    (24, include_bytes!("../../assets/icons/24.png")),
    (32, include_bytes!("../../assets/icons/32.png")),
    (48, include_bytes!("../../assets/icons/48.png")),
    (64, include_bytes!("../../assets/icons/64.png")),
    (128, include_bytes!("../../assets/icons/128.png")),
    (256, include_bytes!("../../assets/icons/256.png")),
    (512, include_bytes!("../../assets/icons/512.png")),
];
const ICON_SVG: &[u8] = include_bytes!("../../assets/logo.svg");

/// One embedded icon as 8-bit RGBA (straight alpha): `(width, height, pixels)`.
fn decode(png: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut reader = png::Decoder::new(io::Cursor::new(png)).read_info().ok()?;
    let mut rgba = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut rgba).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    rgba.truncate(info.buffer_size());
    Some((info.width, info.height, rgba))
}

/// The icon stored on the window itself on X11.
pub fn window_icon() -> Option<winit::window::Icon> {
    let (_, png) = ICONS.iter().find(|(size, _)| *size == 128)?;
    let (w, h, rgba) = decode(png)?;
    winit::window::Icon::from_rgba(rgba, w, h).ok()
}

pub use wayland::{Icon as WaylandIcon, set as set_wayland_icon};

/// The window's own icon on Wayland, through `xdg-toplevel-icon-v1`.
mod wayland {
    use std::ffi::c_void;
    use std::os::fd::{AsFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::FileExt;

    use wayland_client::backend::{Backend, ObjectId};
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_buffer, wl_registry, wl_shm, wl_shm_pool};
    use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
    use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
    use wayland_protocols::xdg::toplevel_icon::v1::client::{
        xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1, xdg_toplevel_icon_v1::XdgToplevelIconV1,
    };

    use super::{APP_ID, ICONS, decode};

    pub struct State;

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

    delegate_noop!(State: ignore wl_shm::WlShm);
    delegate_noop!(State: wl_shm_pool::WlShmPool);
    delegate_noop!(State: ignore wl_buffer::WlBuffer);
    // Preferred sizes; every size up to 256 is offered anyway.
    delegate_noop!(State: ignore XdgToplevelIconManagerV1);
    delegate_noop!(State: XdgToplevelIconV1);

    /// Keep for as long as the window lives: the compositor may read the
    /// icon's buffers until the icon is replaced.
    pub struct Icon {
        _queue: EventQueue<State>,
        _icon: XdgToplevelIconV1,
        _buffers: Vec<wl_buffer::WlBuffer>,
    }

    /// Give the toplevel (winit's `xdg_toplevel` proxy on `display`) Veneer's
    /// icon, by theme name and as pixels. `None` if the compositor does not
    /// support it.
    pub fn set(display: usize, toplevel: *mut c_void) -> Result<Option<Icon>, Box<dyn std::error::Error>> {
        // Share the window's connection, with our own event queue (as the text overlay does).
        let conn = Connection::from_backend(unsafe { Backend::from_foreign_display(display as *mut _) });
        let (globals, queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let Ok(manager) = globals.bind::<XdgToplevelIconManagerV1, _, _>(&qh, 1..=1, ()) else {
            return Ok(None);
        };
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
        let toplevel_id = unsafe { ObjectId::from_ptr(XdgToplevel::interface(), toplevel.cast()) }?;
        let toplevel = XdgToplevel::from_id(&conn, toplevel_id)?;

        // Every size up to 256 px, as premultiplied ARGB8888 (little endian:
        // B, G, R, A), one after another in a single shared-memory file.
        let mut images = Vec::new();
        for (size, png) in ICONS.iter().filter(|(size, _)| *size <= 256) {
            let (w, h, rgba) = decode(png).ok_or("bad embedded icon")?;
            if (w, h) != (*size, *size) {
                return Err("embedded icon has the wrong size".into());
            }
            let mut argb = Vec::with_capacity(rgba.len());
            for px in rgba.chunks_exact(4) {
                let a = px[3] as u32;
                let pre = |c: u8| ((c as u32 * a + 127) / 255) as u8;
                argb.extend_from_slice(&[pre(px[2]), pre(px[1]), pre(px[0]), px[3]]);
            }
            images.push((*size, argb));
        }
        let len: usize = images.iter().map(|(_, px)| px.len()).sum();
        let fd = unsafe { libc::memfd_create(c"lucid-icon".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        file.set_len(len as u64)?;
        let pool = shm.create_pool(file.as_fd(), len as i32, &qh, ());

        let icon = manager.create_icon(&qh, ());
        // A theme icon named `lucid` (from `veneer desktop-entry`) may be
        // preferred by the compositor; the pixels are the fallback.
        icon.set_name(APP_ID.to_string());
        let mut buffers = Vec::new();
        let mut offset = 0;
        for (size, argb) in &images {
            file.write_all_at(argb, offset as u64)?;
            let s = *size as i32;
            let buffer = pool.create_buffer(offset as i32, s, s, s * 4, wl_shm::Format::Argb8888, &qh, ());
            icon.add_buffer(&buffer, 1);
            buffers.push(buffer);
            offset += argb.len();
        }
        // Applied with the window's next commit (the game commits every frame).
        manager.set_icon(&toplevel, Some(&icon));
        // Neither destroys the icon or its buffers.
        pool.destroy();
        manager.destroy();
        conn.flush()?;
        Ok(Some(Icon { _queue: queue, _icon: icon, _buffers: buffers }))
    }
}

fn launcher_path() -> PathBuf {
    paths::data_home().join("applications").join(format!("{APP_ID}.desktop"))
}

fn hicolor() -> PathBuf {
    paths::data_home().join("icons/hicolor")
}

fn icon_paths() -> Vec<PathBuf> {
    let root = hicolor();
    let mut v: Vec<PathBuf> = ICONS
        .iter()
        .map(|(s, _)| root.join(format!("{s}x{s}/apps/{APP_ID}.png")))
        .collect();
    v.push(root.join(format!("scalable/apps/{APP_ID}.svg")));
    v
}

/// One argument of a launcher's `Exec=` line, escaped as the Desktop Entry
/// specification asks: quoted if it has reserved characters, `%` doubled,
/// and backslashes doubled again because the value is itself a string.
fn exec_arg(arg: &str) -> Option<String> {
    if arg.chars().any(|c| c.is_control()) {
        return None;
    }
    let reserved = |c: char| " \t\"'\\><~|&;$*?#()`".contains(c);
    let mut out = if arg.chars().any(reserved) {
        let mut q = String::from("\"");
        for c in arg.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                q.push('\\');
            }
            q.push(c);
        }
        q.push('"');
        q
    } else {
        arg.to_string()
    };
    out = out.replace('%', "%%");
    Some(out.replace('\\', "\\\\"))
}

fn launcher(exe: &Path) -> io::Result<String> {
    let exe = exe
        .to_str()
        .and_then(exec_arg)
        .ok_or_else(|| io::Error::other(format!("cannot put {} in a launcher", exe.display())))?;
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.5\n\
         Name={NAME}\n\
         Comment=Run the Android build of Roblox on Linux\n\
         Keywords=Roblox;\n\
         Exec={exe} run\n\
         Icon={APP_ID}\n\
         Terminal=false\n\
         Categories=Game;\n\
         StartupWMClass={APP_ID}\n\
         SingleMainWindow=true\n"
    ))
}

/// Tell the desktop that launchers and icons changed. Most desktops notice
/// by themselves; these cover the ones that keep caches.
fn refresh() {
    let hicolor = hicolor();
    // Icon themes are rescanned when the theme directory's time changes.
    let _ = std::fs::File::open(&hicolor).and_then(|f| f.set_modified(std::time::SystemTime::now()));
    // A stale GTK icon cache would hide the new icon, so rebuild it if one exists.
    if hicolor.join("icon-theme.cache").exists() {
        quiet(Command::new("gtk-update-icon-cache").args(["-q", "-f", "-t"]).arg(&hicolor));
    }
    // KDE Plasma reads launchers through its own cache (KService).
    if !quiet(&mut Command::new("kbuildsycoca6")) {
        quiet(&mut Command::new("kbuildsycoca5"));
    }
}

/// Run a helper that may not be installed; whether it ran.
fn quiet(cmd: &mut Command) -> bool {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, contents)
}

/// Install the launcher (pointing at this binary) and the icons.
pub fn install() -> io::Result<(PathBuf, PathBuf)> {
    let exe = std::env::current_exe()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let launcher_path = launcher_path();
    for ((_, png), path) in ICONS.iter().zip(icon_paths()) {
        write(&path, png)?;
    }
    write(icon_paths().last().unwrap(), ICON_SVG)?;
    write(&launcher_path, launcher(&exe)?.as_bytes())?;
    refresh();
    Ok((launcher_path, exe))
}

/// Remove what [`install`] wrote; how many files were removed.
pub fn remove() -> io::Result<usize> {
    let mut removed = 0;
    for path in std::iter::once(launcher_path()).chain(icon_paths()) {
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    refresh();
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_arg_escaping() {
        assert_eq!(exec_arg("/usr/bin/lucid").unwrap(), "/usr/bin/lucid");
        assert_eq!(
            exec_arg("/home/a/Área de trabalho/lucid").unwrap(),
            "\"/home/a/Área de trabalho/lucid\""
        );
        // `$` and `\` are escaped inside the quotes, then `\` doubled again.
        assert_eq!(exec_arg("/a b/$x\\y").unwrap(), "\"/a b/\\\\$x\\\\\\\\y\"");
        assert_eq!(exec_arg("/a/100%").unwrap(), "/a/100%%");
        assert!(exec_arg("/a\nb").is_none());
    }

    #[test]
    fn window_icon_decodes() {
        assert!(window_icon().is_some());
    }
}
