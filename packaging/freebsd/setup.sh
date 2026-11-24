#!/bin/sh
# Prepare FreeBSD (13 or newer, amd64) to run Veneer's Linux build through
# FreeBSD's Linux compatibility layer (the "Linuxulator"). EXPERIMENTAL.
#
# Run as root:  sh setup.sh
#
# It enables the Linux compatibility layer (with /proc, /sys and /dev in
# /compat/linux, which Veneer needs), installs a Rocky Linux 9 userland and
# the Linux libraries Veneer loads at run time, then checks the rest of the
# system and says what is missing. Nothing outside this list is changed.
# Afterwards, as your normal user, run the x86_64 Linux build of Veneer
# (veneer-<version>-x86_64-linux from the releases page) like any program.

set -u

if [ "$(id -u)" -ne 0 ]; then
    echo "Run this as root (for example: su -m root -c 'sh setup.sh')." >&2
    exit 1
fi
if [ "$(uname -s)" != FreeBSD ]; then
    echo "This script is for FreeBSD." >&2
    exit 1
fi
if [ "$(uname -m)" != amd64 ]; then
    echo "Only amd64 is covered: Veneer's arm64 build is not tried on FreeBSD." >&2
    exit 1
fi

echo "== Linux compatibility layer"
sysrc linux_enable=YES
# The rc script mounts linprocfs, linsysfs, devfs, fdescfs and tmpfs under
# /compat/linux when this is on (the default; set explicitly to be sure).
sysrc linux_mounts_enable=YES
service linux restart || service linux start

echo "== Linux userland and libraries"
# Required (names checked against the ports tree): the userland (glibc
# 2.34, what Veneer's build needs), EGL/GLES (libglvnd), Mesa with the
# Vulkan drivers (dri), the Vulkan loader, Wayland and PulseAudio clients.
required="linux_base-rl9 linux-rl9-libglvnd linux-rl9-dri linux-rl9-vulkan
          linux-rl9-wayland linux-rl9-pulseaudio-libs"
# Optional: the embedded browser (GTK 3 + WebKitGTK 4.0, which Veneer also
# accepts; without them web pages open in the system browser) and the
# keyring (without it the login is kept in a private file).
optional="linux-rl9-gtk3 linux-rl9-webkit2gtk3 linux-rl9-libsecret"

missing=""
install_list=""
for p in $required $optional; do
    if pkg rquery '%n' "$p" >/dev/null 2>&1 && [ -n "$(pkg rquery '%n' "$p")" ]; then
        install_list="$install_list $p"
    else
        missing="$missing $p"
    fi
done
if [ -n "$install_list" ]; then
    # shellcheck disable=SC2086
    pkg install -y $install_list
fi

echo
echo "== Checks"
ok=1
for p in $missing; do
    case " $required " in
        *" $p "*) echo "MISSING (required): package $p is not in the repository;"
                  echo "  find its current name with: pkg search linux-rl9 | grep -i <name>"
                  ok=0 ;;
        *)        echo "note: optional package $p is not in the repository" ;;
    esac
done
for lib in libvulkan.so.1 libEGL.so.1 libwayland-client.so.0 libwayland-egl.so.1 libpulse-simple.so.0; do
    if ! ls /compat/linux/usr/lib64/"$lib" >/dev/null 2>&1; then
        echo "MISSING: /compat/linux/usr/lib64/$lib"
        ok=0
    fi
done
if [ ! -e /compat/linux/proc/self/maps ]; then
    echo "MISSING: /compat/linux/proc is not mounted (service linux restart)"
    ok=0
fi
# The GPU: the native DRM driver must be loaded (drm-kmod), and the user in
# the video group, for Linux Mesa to reach it.
if ! ls /dev/dri/renderD* >/dev/null 2>&1; then
    echo "MISSING: no /dev/dri/renderD* - install graphics/drm-kmod and add the"
    echo "  driver to kld_list (i915kms for Intel, amdgpu for AMD), then reboot"
    ok=0
fi
echo "note: your user must be in the video group (pw groupmod video -m <user>)"
echo "note: Veneer needs a Wayland session (or X11) and a PulseAudio or"
echo "  PipeWire sound server (audio/pulseaudio, or sound via OSS through it)."

echo
if [ "$ok" -eq 1 ]; then
    echo "Done. As your normal user, run: ./veneer-<version>-x86_64-linux run"
else
    echo "Done, with the problems listed above."
    exit 1
fi
