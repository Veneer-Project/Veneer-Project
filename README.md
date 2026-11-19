# lucid

An open-source runtime, written in Rust, that runs the official **Android x86-64
build of Roblox** on desktop Linux — the same idea as the closed-source
[Sober](https://sober.vinegarhq.org/), but open.

No Android and no Java are involved. lucid loads Roblox's native library
(`libroblox.so`, taken from the APK) straight into a normal Linux process and
supplies everything that library expects from Android: a bionic-compatible
libc, the NDK system libraries, and a fake Java VM in which the app's Java side
is reimplemented in Rust.

"lucid" is a working name.

## Status

Tested on one machine: Debian 13, GNOME on Wayland, Intel HD 520 (Mesa/Vulkan),
Roblox 2.745.1142 and 2.747.1486.

Working (confirmed by use):

- Automatic download and update of the game from Google Play at startup
  (updated 2.745.1142 to 2.747.1486 this way)
- Install from APK files, or from an existing Sober install
- Engine boot, home page, login (the session persists across restarts)
- Joining experiences, from the app or directly with `--place`
- Leaving back to the home page
- Vulkan rendering, audio, keyboard, mouse, camera / mouse lock
- Chat and other text boxes: typing, selection, clipboard, shortcuts
- F11 fullscreen
- In-game settings are saved between sessions
- Keys no longer stick after Alt+Tab

Written but not confirmed:

- Discord Rich Presence (opt-in, needs a Discord application ID)
- X11 (only Wayland has been run; text overlay and clipboard are Wayland-only)

Not implemented:

- Gamepads, touch, in-game video, camera, voice chat
- The "fullscreen" switch inside Roblox's own settings (use F11)
- Mouse selection inside text boxes, multi-line text boxes (drawn as one line)
- Crash reporting (Crashpad is deliberately never started)

## Build and run

Needs a stable Rust toolchain (built with 1.100) and, at run time, the usual
desktop libraries: `libvulkan.so.1`, `libwayland-client`, `libwayland-egl`,
`libEGL`, `libpulse-simple.so.0`. They are loaded with `dlopen`, so no `-dev`
packages are needed to build.

```sh
cargo build
./target/debug/lucid run                        # download/update the game if needed, then start
./target/debug/lucid update                     # only download/update
./target/debug/lucid install --from-sober       # manual install; or: install base.apk split_config.x86_64.apk
./target/debug/lucid run --place 1818           # join a place directly
./target/debug/lucid probe                      # boot the engine without a window
```

On every start, `lucid run` checks Google Play for the current Roblox release
and downloads it if the game is missing or out of date (`--no-update` or
`LUCID_NO_UPDATE=1` skips this). Google Play needs a signed-in account, so
lucid asks the Aurora Store project's public token service for an anonymous
one, then downloads straight from Google. If that fails, lucid prints a
warning and starts the version already installed; if nothing is installed it
stops and tells you to install manually.

The first build compiles OpenSSL from source (needs `perl` and `make`); use
`cargo build --no-default-features` to link the system `libssl` instead.

Where things live:

| Path | Contents |
|---|---|
| `~/.local/share/lucid/app/` | Libraries and assets unpacked from the APK, `version` |
| `~/.local/share/lucid/files/` | The app's private files (Android `filesDir`) |
| `~/.local/share/lucid/prefs/` | Android `SharedPreferences`, one JSON file each |
| `~/.local/share/lucid/cookies` | The login session (mode 0600) — treat as a password |
| `~/.local/share/lucid/logs/latest.log` | Debug log of the last run, overwritten each start |
| `~/.cache/lucid/` | The app's cache directory |
| `~/.config/lucid/fflags.json` | Optional fast-flag overrides, e.g. `{"DFIntTaskSchedulerTargetFps": 144}` |

Environment variables:

| Variable | Effect |
|---|---|
| `LUCID_LOG` | Terminal verbosity: `error`, `warn`, `info` (default), `debug`, `trace` |
| `LUCID_FFLAGS` | Extra fast-flag overrides as a JSON object |
| `LUCID_DISCORD_CLIENT_ID` | Enables Discord Rich Presence with that application ID |
| `LUCID_LENIENT` | Calls to unresolved imports return 0 instead of aborting |
| `LUCID_NO_GUEST_SIGNALS` | Ignore signal handlers the engine tries to install |

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
lucid/           The launcher binary:
  install.rs     unpack APKs            startup.rs   the Android app's startup sequence
  update.rs      Google Play download and update check
  runtime.rs     load engine, run       java.rs      static callback classes, jcall! macro
  app.rs         winit window/events    game.rs      join / leave experiences
  input.rs       keys, mouse, text      overlay.rs   native text overlay (Wayland sub-surface)
  audio.rs       FMOD output (Pulse)    prefs.rs     SharedPreferences
  cookies.rs     login persistence      presence.rs  Discord presence
  flags.rs       default fast flags     crash.rs     fatal signal reporter
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

- `~/.local/share/lucid/logs/latest.log` always has debug-level output,
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
6. **Discord**: register an application ID for the project and ship it as the
   default.
7. Local-reference bookkeeping on engine-owned threads is minimal (locals not
   explicitly deleted there are leaked).

## Legal

lucid contains no Roblox code or assets; you supply the APK. Roblox is a
trademark of Roblox Corporation, which is not affiliated with this project.
Licensed under GPL-3.0-or-later.
