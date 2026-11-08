#!/bin/bash
# Puts this driver image's guest scripts and settings in place of the ones a guest was set up with, the
# way a fresh setup would, so a guest takes the fixes that come with each new image. gpu-up.sh runs it at
# each boot from the mounted image; on a guest set up before that, run it once by hand, as root:
#     sudo bash /opt/nv/driver/setup/refresh.sh
# and reboot. (If this boot has no GPU, mount the image there first: mount -o ro LABEL=NVFWD_DRIVER /opt/nv/driver.)
# Usage: refresh.sh [--if-changed]. Exit 0: refreshed. 3: nothing to do. 1: it failed, and the guest
# keeps the scripts it had.
set -uo pipefail
here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
. "$here/common.sh"
unchanged=3
previous=$root/var/lib/nvfwd/guest-files.previous
staged=$installed.new

fail() {
    echo "The guest's scripts weren't refreshed, and it keeps the ones it had: $1"
    if [ -d "$previous" ]; then
        rm -rf "$installed" "$staged"
        mv "$previous" "$installed"
    fi
    printf '%s\n' "$1" > "$guest_files_problem"
    exit 1
}

# Ubuntu's own install: these scripts over a copy of the installed ones, which keeps the files the
# install's seed brought and the image has no copy of, such as the Xwayland wrapper. Then the settings.
refresh_ubuntu() {
    rm -rf "$staged"
    if ! cp -a "$installed" "$staged" || ! cp -R "$here/." "$staged/"; then
        fail "copying the scripts failed"
    fi
    rm -rf "$staged/__pycache__" "$installed"
    mv "$staged" "$installed" || fail "putting the scripts in place failed"
    "$installed/first-boot.sh" --refresh || fail "installing the settings failed"
}

[ "$(id -u)" = 0 ] || { echo "Run it as root." >&2; exit 1; }
[ -d "$installed" ] || { echo "This guest has no scripts of ours to refresh." >&2; exit 1; }
new_id=$(guest_files_id "$here") || exit 1
if [ "${1:-}" = --if-changed ] && [ "$(cat "$guest_files_marker" 2>/dev/null)" = "$new_id" ]; then
    exit "$unchanged"
fi
mkdir -p "$(dirname "$guest_files_marker")"
rm -rf "$previous"
# Running scripts keep reading their old files, as these are moved or removed, never written over.
cp -a "$installed" "$previous" || fail "keeping a copy of the scripts failed"
if [ -n "$own_family" ]; then
    bash "$here/setup-own.sh" --refresh || fail "setup-own.sh --refresh failed"
else
    refresh_ubuntu
fi
rm -rf "$previous" "$guest_files_problem"
echo "$new_id" > "$guest_files_marker"
# logind reads its settings again; the rest take effect as each part starts.
systemctl reload systemd-logind 2>/dev/null || true
echo "The guest's scripts and settings are now this driver image's ($new_id)."
