#!/bin/sh
# One install and uninstall on whatever distro this is, under its own /bin/sh, as a user with no
# rights. It's meant for a throwaway container, and in-containers.sh starts those.
#
# Usage: in-container.sh REPO WANTED, where WANTED is a piece of the root command this distro must show.
# It reads two answers from the terminal, and /usr/share/qemu/firmware must be writable.
set -eu

repo=$1
wanted=$2
here=$repo/tests/install
T=$(mktemp -d)
H=$T/home
mkdir -p "$H" "$T/bin" "$T/releases" "$T/libraries"
for stand_in in curl sudo ldconfig; do
    ln -s "$here/stand-ins/$stand_in" "$T/bin/"
done
for manager in dnf apt-get pacman zypper; do
    cp "$here/stand-ins/package-manager" "$T/bin/$manager"
done
command -v xz >/dev/null 2>&1 || ln -s "$here/stand-ins/xz" "$T/bin/xz"
printf 'adw_new_thing' >"$T/libraries/libadwaita-1.so.0"
printf 'gtk_new_thing' >"$T/libraries/libgtk-4.so.1"
printf 'pipewire' >"$T/libraries/libpipewire-0.3.so.0"
for library in libepoxy.so.0 libgbm.so.1 libEGL.so.1 libdrm.so.2; do printf 'gl' >"$T/libraries/$library"; done
printf 'amdgpu_device_initialize2' >"$T/libraries/libdrm_amdgpu.so.1"

PATH=$T/bin:$PATH
HOME=$H
STAND_IN_LOG=$T/log
FAKE_RELEASES=$T/releases
FAKE_LIBRARIES=$T/libraries
FAKE_FIRMWARE=/usr/share/qemu/firmware
# A GTK new enough for the app with HDR, which is the one this checks for.
FAKE_GTK=4.20
export PATH HOME STAND_IN_LOG FAKE_RELEASES FAKE_LIBRARIES FAKE_FIRMWARE FAKE_GTK
: >"$T/log"

sh "$here/make-release.sh" "$repo" "$T/releases" 1.0
sed -n 2p /etc/os-release
ls -l /bin/sh

listing() {
    (cd "$H" && find . -printf '%y %p %l\n' | sort)
}

listing >"$T/before"
sh "$T/releases/install-1.0.sh" --dry-run >"$T/plan"
grep -F -- "$wanted" "$T/plan"
[ "$(listing)" = "$(cat "$T/before")" ]
echo "dry run: changed nothing"

sh "$T/releases/install-1.0.sh" </dev/null | tail -n 4
[ "$("$H/.local/lib/guesthouse/current/guesthouse")" = "guesthouse 1.0" ]
echo "installed, and it runs"

sh "$H/.local/lib/guesthouse/current/uninstall.sh" </dev/null | tail -n 2
[ "$(listing)" = "$(cat "$T/before")" ]
echo "HOME is as it began"
