# Veneer

An open-source runtime, written in Rust, that runs the official **Android x86-64
build of Roblox** on desktop Linux — the same idea as the closed-source
[Sober](https://sober.vinegarhq.org/), but open.

No Android and no Java are involved. Veneer loads Roblox's native library
(`libroblox.so`, taken from the APK) straight into a normal Linux process and
supplies everything that library expects from Android: a bionic-compatible
libc, the NDK system libraries, and a fake Java VM in which the app's Java side
is reimplemented in Rust.

**Name and codename.** The project is called Veneer; its codename is Lucid.
The name is used for everything outside the code: the `veneer` command, the
window title, the app menu entry, the Discord presence and the folders on
disk (`~/.local/share/veneer` and the others below). The codename stays
inside: the crates (`lucid`, `lucid-linker`, ...), the `LUCID_*` environment
variables and the device identity reported to Roblox. Folders from before
the rename (`~/.local/share/veneer`, `~/.cache/veneer`, `~/.config/veneer`) are
moved to the new names on the first start, so the installed game and the
login carry over.

## Status

Tested on one machine: Debian 13, GNOME on Wayland, Intel HD 520 (Mesa/Vulkan),
Roblox 2.745.1142 and 2.747.1486.

Working (confirmed by use):

- Automatic download and update of the game from Google Play at startup
  (updated 2.745.1142 to 2.747.1486 this way)
- Install from APK files, or from an existing Sober install
- Only APKs signed by Roblox are installed (see "Only genuine APKs" below)
- Other programs cannot attach to Veneer or read its memory (see "Process
  protection")
- The login is kept in the desktop keyring (GNOME Keyring), moved there
  from the old file (see "Where the login is kept")
- Engine boot, home page, login (the session persists across restarts)
- Joining experiences, from the app or directly with `--place`
- Leaving back to the home page
- Vulkan rendering, audio, keyboard, mouse, camera / mouse lock
- Voice chat: speaking (microphone), hearing others, mute / unmute (see
  "Voice chat and the microphone")
- Chat and other text boxes: typing, selection, clipboard, shortcuts
- F11 fullscreen
- In-game settings are saved between sessions
- Keys no longer stick after Alt+Tab
- Discord Rich Presence: "In the menu", or the experience's name and icon
- Screens that are web pages on Android (private servers and similar) open
  in an embedded browser window that shares the app's login
- Starting a game from an embedded web page (for example joining a private
  server)
- Window title, icon and app menu entry (`veneer desktop-entry`) on
  GNOME (Wayland)

Written but not confirmed:

- X11 (only Wayland has been run; text overlay and clipboard are Wayland-only)
- The window's own icon (X11, and `xdg-toplevel-icon` on KDE Plasma and
  others): not run on a desktop that uses it; only GNOME (Wayland), which
  takes the icon from the launcher, has been seen

Not implemented:

- Gamepads, touch, in-game video, camera
- The "fullscreen" switch inside Roblox's own settings (use F11)
- Mouse selection inside text boxes, multi-line text boxes (drawn as one line)
- Crash reporting (Crashpad is deliberately never started)

## Install

Every [release](https://github.com/butdoge33/Veneer-Project/releases) has
ready-made x86-64 builds: a `.deb` (Debian, Ubuntu), an `.rpm` (Fedora,
openSUSE), and the plain `veneer` binary (also as a `.tar.gz` with the
launcher and icons). The packages install the app menu entry and icons, so
`veneer desktop-entry` is not needed with them.

## Build and run

Needs a stable Rust toolchain (built with 1.100) and, at run time, the usual
desktop libraries: `libvulkan.so.1`, `libwayland-client`, `libwayland-egl`,
`libEGL`, `libpulse-simple.so.0`. They are loaded with `dlopen`, so no `-dev`
packages are needed to build.
The embedded browser additionally uses GTK 3 and WebKitGTK
(`libwebkit2gtk-4.1.so.0`) if present; without them web pages open in the
system browser.

```sh
cargo build
./target/debug/veneer run                       # download/update the game if needed, then start
./target/debug/veneer update                    # only download/update
./target/debug/veneer install --from-sober      # manual install; or: install base.apk split_config.x86_64.apk
./target/debug/veneer run --place 1818          # join a place directly
./target/debug/veneer probe                     # boot the engine without a window
```

For an optimized build, use `--release`; the binary is then in
`target/release` instead:

```sh
cargo build --release
./target/release/veneer run                     # download/update the game if needed, then start
./target/release/veneer update                  # only download/update
./target/release/veneer install --from-sober    # manual install; or: install base.apk split_config.x86_64.apk
./target/release/veneer run --place 1818        # join a place directly
./target/release/veneer probe                   # boot the engine without a window
```

### App menu and icon

The window is titled "Veneer". To give it the Veneer icon and add Veneer to
the app menu, run once (with the binary you want the menu to start):

```sh
./target/release/veneer desktop-entry           # or ./target/debug/veneer desktop-entry
./target/release/veneer desktop-entry --remove  # undo
```

This writes `~/.local/share/applications/veneer.desktop`, which starts
`veneer run` from the binary's current path, and the logo (the same image as
the repository icon) into `~/.local/share/icons/hicolor/`. Run it again if
the binary moves, for example when switching from the debug to the release
build.

Whether the window needs it depends on the desktop:

- GNOME on Wayland: a window cannot carry its own icon there. GNOME matches
  the window's app ID, `veneer`, to `veneer.desktop` and shows that launcher's
  icon; without the launcher the window has a generic icon.
- Wayland desktops with the `xdg-toplevel-icon` protocol (KDE Plasma 6.1+
  and some others): the window sends its own icon, so it shows without the
  launcher. The launcher still adds Veneer to the app menu.
- X11 (GNOME on Xorg, Budgie 10, Xfce and others): the window carries its own
  icon too.

After `veneer desktop-entry`, most desktops pick up the change right away;
Veneer also refreshes the GTK icon cache if you have one and KDE's launcher
cache (`kbuildsycoca6`/`kbuildsycoca5`). If the icon still does not show, log
out and back in.

On every start, `veneer run` checks Google Play for the current Roblox release
and downloads it if the game is missing or out of date (`--no-update` or
`LUCID_NO_UPDATE=1` skips this). Google Play needs a signed-in account, so
Veneer asks the Aurora Store project's public token service for an anonymous
one, then downloads straight from Google. The login is kept and reused on
later starts, so the token service is only contacted when it has expired (and
is retried a few times if it is busy). If that fails, Veneer prints a
warning and starts the version already installed; if nothing is installed it
stops and tells you to install manually.

The first build compiles OpenSSL from source (needs `perl` and `make`); use
`cargo build --no-default-features` to link the system `libssl` instead.

Where things live:

| Path | Contents |
|---|---|
| `~/.local/share/veneer/app/` | Libraries and assets unpacked from the APK, `version` |
| `~/.local/share/veneer/files/` | The app's private files (Android `filesDir`) |
| `~/.local/share/veneer/prefs/` | Android `SharedPreferences`, one JSON file each |
| `~/.local/share/veneer/cookies` | The login session, only when there is no keyring (mode 0600) — treat as a password |
| `~/.local/share/veneer/play_login.json` | The anonymous Google Play login from the last update check, reused until it expires |
| `~/.local/share/veneer/logs/latest.log` | Debug log of the last run, overwritten each start |
| `~/.cache/veneer/` | The app's cache directory |
| `~/.local/share/applications/veneer.desktop`, `~/.local/share/icons/hicolor/*/apps/veneer.*` | Launcher and icons, only if you ran `veneer desktop-entry` |
| `~/.config/veneer/fflags.json` | Optional fast-flag overrides, e.g. `{"DFIntTaskSchedulerTargetFps": 144}` (not created by Veneer, see below) |

Veneer only reads `~/.config/veneer/`; it never creates it, so the directory
does not exist until you make it. To override fast flags:

```sh
mkdir -p ~/.config/veneer
echo '{"DFIntTaskSchedulerTargetFps": 144}' > ~/.config/veneer/fflags.json
```

The next start logs `applying N fast flag overrides from …`.

Environment variables:

| Variable | Effect |
|---|---|
| `LUCID_LOG` | Terminal verbosity: `error`, `warn`, `info` (default), `debug`, `trace` |
| `LUCID_FFLAGS` | Extra fast-flag overrides as a JSON object |
| `LUCID_DISCORD` | Set to `0` to turn Discord Rich Presence off (it is on by default) |
| `LUCID_FMOD_AUDIOTRACK` | Set to `1` to play game sound the older way (Android `AudioTrack`); there is no microphone then, so voice chat cannot hear you |
| `LUCID_DISCORD_CLIENT_ID` | Use a different Discord application ID |
| `LUCID_LENIENT` | Calls to unresolved imports return 0 instead of aborting |
| `LUCID_NO_GUEST_SIGNALS` | Ignore signal handlers the engine tries to install |
| `LUCID_JNI_TRACE` | Set to `1` to log where live JNI local references come from (debug log, once a minute and at exit) |

## Voice chat and the microphone

Roblox plays all sound and records the microphone through FMOD, its sound
engine, which on Android uses OpenSL ES; Veneer provides OpenSL ES on top of
PulseAudio (PipeWire works too). In the system's sound settings the streams
show up under "Veneer" ("Game audio" / "Voice chat microphone").

The microphone is only open while voice chat is recording (unmuted), from
your default input device, and the terminal says so each time:
`microphone opened (voice chat)` and `microphone closed`. Roblox asks for
microphone permission; Veneer allows it, as desktops do not ask apps.

## Where the login is kept

Your Roblox login (the session cookies) is kept in the desktop's keyring:
GNOME Keyring, KWallet or any other Secret Service, as the item "Veneer:
Roblox login". There it is encrypted with your login password, locked while
you are logged out, and not in any file that a stealer searching your disk
(by file name or for Roblox's cookie text) could find. A login saved by an
older version in `~/.local/share/veneer/cookies` is moved into the keyring
on the next start, and the file is deleted once the keyring has it.

Without a keyring (libsecret missing, or no Secret Service running) the
login stays in that file, readable only by you, and Veneer says so once.

What it does not protect against: malware already running as your user
while you are logged in. It can ask the keyring itself (GNOME Keyring does
not check which program asks), or read the game's memory. No program can
fully protect against that; only sandboxes such as Flatpak get closer.

## Process protection

Veneer's processes (the game and the embedded browser) are marked so that
other programs running as your user cannot attach to them as a debugger or
read or change their memory. The kernel refuses it (`ptrace`,
`/proc/<pid>/mem`, `process_vm_readv` / `process_vm_writev`), whatever the
system's ptrace setting. The game engine cannot turn this off.

## Only genuine APKs

The game's native code runs with your user's full permissions, so a modified
APK (cheats, malware) could do anything you can. Before installing, Veneer
checks every APK's Android signature (APK Signature Scheme v2, v3 and v3.1,
which covers every byte of the file) and installs it only if it was signed
by Roblox. This applies to Google Play downloads, `veneer install` and Sober
installs alike, and happens before the current install is touched, so a
refused APK leaves the game as it was. There is deliberately no option to
skip it.

- Roblox's original signing certificate is pinned by its SHA-256,
  `44932ea35a17a267372d71b54d1a0cb3da0dca5113e94406ae2fe18090ba1477`
  (Roblox Corporation, 2014; APKMirror lists the same fingerprint for every
  Roblox release).
- Roblox has since moved Android 13+ to a newer key (2026). That key is
  accepted because Roblox's APKs carry Android's proof-of-rotation, in which
  the original key signs the new certificate; Veneer checks that chain.
- Limits: the original key is 1024-bit RSA, weak by today's standards, and
  everything else rests on it. And Veneer is open source: someone can build
  a copy without the check. It stops modified APKs being installed by
  Veneer as published, not a modified Veneer.

## How it works

```
crates/linker    ELF loader for bionic shared objects (Android packed relocations,
                 RELR, GNU hash, constructors) plus the guest-facing libdl API.
crates/bionic    libc/libm personality: most symbols forward to glibc; the ones
                 whose ABI differs (stdio __sF, sigaction, sem_t, setjmp,
                 getaddrinfo, sysconf, ...) are reimplemented.
crates/android   NDK libraries: liblog, libandroid (assets, ALooper, ANativeWindow,
                 AConfiguration), EGL and Vulkan mapped onto Wayland/X11.
crates/jni       A fake JavaVM/JNIEnv with a small object model. "Java" methods
                 are Rust closures registered with `vm().on(class, name, sig, f)`;
                 anything unregistered logs `UNIMPLEMENTED java method ...` once
                 and returns zero/null.
crates/discord   Discord IPC client (no dependencies).
lucid/           The launcher (builds the `veneer` binary):
  install.rs     unpack APKs            startup.rs   the Android app's startup sequence
  apk_signature.rs  APK signature check (only Roblox-signed APKs are installed)
  update.rs      Google Play download and update check
  runtime.rs     load engine, run       java.rs      static callback classes, jcall! macro
  app.rs         winit window/events    game.rs      join / leave experiences
  input.rs       keys, mouse, text      overlay.rs   native text overlay (Wayland sub-surface)
  audio.rs       FMOD AudioTrack output prefs.rs     SharedPreferences
  opensles.rs    OpenSL ES (FMOD sound and microphone) on PulseAudio
  pulse.rs       PulseAudio playback / recording streams
  voice.rs       WebRTC's WebRtcAudioManager (voice chat)
  permissions.rs the PermissionsProtocol (microphone permission)
  cookies.rs     login persistence      presence.rs  Discord presence
  keyring.rs     Secret Service (libsecret) access
  flags.rs       default fast flags     crash.rs     fatal signal reporter
  bus.rs         engine message bus     webview.rs   web pages (app side)
  webview_helper.rs  the embedded browser process (`veneer webview`, GTK + WebKitGTK)
  device.rs      device identity        paths.rs     directories
```

Startup, in order: register system libraries → load `libroblox.so` and run its
constructors → `JNI_OnLoad` → the calls `RobloxApplication.onCreate` makes →
open the window → settings natives, `nativeGameGlobalInit` → fetch client
settings from Roblox's CDN and pass them to `nativeInitClientSettings` → app
bridge init → `nativeAppBridgeV2StartAppWithParams` with the surface. The engine
then renders on its own threads; the main thread only runs the window event
loop and forwards input.

## Notes for whoever works on this next

Things learned the hard way; read these before debugging.

- **Each thread needs its own `JNIEnv`.** The engine replaces the function
  table pointer of the env it is handed with an instrumented copy. With one
  shared env it wraps its own wrappers and recurses until the stack overflows.
- **Value objects are read by getter or field name.** Parameter objects
  (`InitParams`, `StartAppParams`, `PlatformParams`, ...) are plain instances
  with fields set by name; a no-argument method call named like a field returns
  that field. Field lookups ignore the type signature.
- **Overloaded natives use the long JNI name** (`Java_..._name__<args>`);
  `Vm::native` falls back to it.
- **Keys are read by scan code only.** `nativePassKeyEvent(down, scancode,
  keycode, repeat)` ignores the Android keycode: the Linux evdev scan code goes
  through two tables in the engine. `/` is sent by character so it opens chat
  on non-US layouts.
- **Release held keys when focus is lost**, or modifiers stick (Alt after
  Alt+Tab made "1" take a capture).
- **The engine does not draw text being typed.** On Android a native `EditText`
  is laid over the text box and the engine hides its own text
  (`showKeyboard(handle, overlay, bytes, NativeTextBoxInfo)`). `overlay.rs` is
  that `EditText`. Text goes back with `syncTextboxTextAndCursorPosition2` then
  `nativePassText`. The engine converts strings with `String.getBytes`.
- **Game flow.** Lua publishes a JSON launch request on the message bus
  (`JNIExperienceProtocol.getLaunchId`, `MessageBus.doSubscribeRaw`). Join is
  `nativeAppBridgeV2PauseApp` → `nativeAppBridgeV2StartGameWithParam`. On
  `gameDidLeave`: `nativeOnFragmentStop`, `gameBackgrounded`,
  `nativeAppBridgeV2LeaveGame`, then `nativeAppBridgeV2StartAppWithParams` again.
- **Login cookies** are owned by the engine: read with
  `nativeGetCookiesInNetscapeFormat` after `DID_LOG_IN` and on exit, replayed
  with `nativeSetMultipleCookies` at startup.
- **Do not poke engine memory.** Everything here goes through the interfaces
  the Android app itself uses; keep it that way (it is also what keeps the
  project on the right side of Roblox's anti-tamper).
- The engine's `fork`/`exec`/`system` are denied on purpose.

Debugging tools:

- `~/.local/share/veneer/logs/latest.log` always has debug-level output,
  including the engine's own log lines (`android: [Roblox] ...`).
- `grep UNIMPLEMENTED` in that log lists Java methods the engine wanted.
- A crash prints the faulting address as `libroblox.so+0xOFFSET` plus guest
  code addresses found on the stack; feed offsets to `objdump -d`.
- Test hooks, so the window need not be touched (all start ~20-25 s in):
  `LUCID_TEST_CLICK="x,y;x,y"` clicks points in dp,
  `LUCID_TEST_TYPE="text"` types into whatever the click focused and then
  takes an engine screenshot (saved under `files/picture/`),
  `LUCID_TEST_KEYS="scan:key,..."` taps keys,
  `LUCID_TEST_LEAVE=<seconds>` leaves a game after that long.
  Engine screenshots do not include the text overlay (it is a separate surface).
- Reference material, not part of the repository: the decompiled Java side of
  the APK (jadx output) was kept in `~/Downloads/lucid-work/jadx-out/sources`,
  with a small cross-reference finder in `~/Downloads/lucid-work/tools/xref`.
  Useful starting points there: `com/roblox/engine/jni/*.java`,
  `com/roblox/client/RbxKeyboard.java`, `uh/f.java`, `uh/j0.java`, `yj/k0.java`.

### Known issues and next steps

1. **Google Play download** (`update.rs`) depends on a third-party token
   service (`auroraoss.com/api/auth`), on the `gpapi` crate, and on the device
   profile in `lucid/data/play_device.properties` (an x86-64 Chromebook; the
   "Play Games on PC" x86-64 profile is not offered Roblox). If any of these
   stop working, downloads break and only the manual install remains. Google
   rolls releases out per device class, so the version offered can lag the
   newest one by a release.
2. **X11 support**: text overlay and clipboard need X11 implementations.
3. **Gamepads** (`nativeGamepad*` in `NativeInputInterface`).
4. **Roblox's fullscreen setting**: no Android-side call was found for it.
5. **Video / camera**: 32 `AMediaCodec*`, `AImageReader*`, `AHardwareBuffer*`
   imports are unresolved and abort if called.
6. **Discord logo**: the menu presence refers to an art asset named `logo`.
   Upload `assets/logo.png` under that name in the Discord developer portal
   (Rich Presence → Art Assets) for the application, or it shows no image.
   The presence's title is the application's name, which code cannot set:
   name the application "Veneer" in the portal.

### Roblox engine behaviour (not Veneer bugs)

Things that look like problems in Veneer but come from Roblox's own engine,
which would behave the same on a phone. Documented so nobody chases them in
Veneer's code.

- **A small JNI reference leak while typing.** Each time a text box gets
  focus, the engine creates a `NativeTextBoxInfo` object on one of its own
  threads (to pass to `showKeyboard`) and never deletes its reference. Its
  threads stay attached to the Java VM for the whole session and never
  detach, so on Android these references would stay too. About 2 per minute
  while typing, harmless.
  Traced to the engine (`NewObject` called by engine code on its "RBX Worker"
  threads); Veneer's own reference handling releases correctly (frames,
  `DeleteLocalRef`, detach and thread exit are covered by tests).
  To check it yourself: the debug log has `JNI: N live local references` once
  a minute, and `LUCID_JNI_TRACE=1` lists which thread, call and object type
  the live references come from.

## Legal

Veneer contains no Roblox code or assets; you supply the APK. Roblox is a
trademark of Roblox Corporation, which is not affiliated with this project.
Licensed under GPL-3.0-or-later.
