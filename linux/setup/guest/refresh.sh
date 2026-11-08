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
# What a build runs. Only when one of these changes does the next build come out any different.
build_scripts=(build-parts.sh common.sh distro.sh)
kept_a_copy=

fail() {
    echo "The guest's scripts weren't refreshed, and it keeps the ones it had: $1"
    if [ -n "$kept_a_copy" ]; then
        rm -rf "$installed" "$staged"
        mv "$previous" "$installed"
    fi
    # setup-own.sh writes the marker as it copies, and these aren't the scripts it names any more.
    rm -f "$guest_files_marker"
    printf '%s\n' "$1" > "$guest_files_problem"
    exit 1
}

# Whether the installed scripts are already these, file for file. A guest whose install brought them
# has no marker yet, and has nothing to take.
installed_matches() {
    local name
    while IFS= read -r -d '' name; do
        cmp -s "$here/$name" "$installed/$name" || return 1
    done < <(cd "$here" && find . -type f ! -path '*/__pycache__/*' -print0)
}

builds_differently() {
    local name
    for name in "${build_scripts[@]}"; do
        cmp -s "$previous/$name" "$installed/$name" || return 0
    done
    return 1
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
mkdir -p "$(dirname "$guest_files_marker")"
if [ "${1:-}" = --if-changed ]; then
    [ "$(cat "$guest_files_marker" 2>/dev/null)" != "$new_id" ] || exit "$unchanged"
    if [ ! -e "$guest_files_marker" ] && installed_matches; then
        echo "$new_id" > "$guest_files_marker"
        exit "$unchanged"
    fi
fi
rm -rf "$previous"
# Running scripts keep reading their old files, as these are moved or removed, never written over.
cp -a "$installed" "$previous" || fail "keeping a copy of the scripts failed"
kept_a_copy=1
if [ -n "$own_family" ]; then
    bash "$here/setup-own.sh" --refresh || fail "setup-own.sh --refresh failed"
else
    refresh_ubuntu
fi
# New scripts can build more than the old ones did. Then the next build, which may be the one this
# boot is about to start, builds everything again with them.
if builds_differently; then
    rm -f "$root"/lib/modules/*/updates/nvfwd/built-for
fi
rm -rf "$previous" "$guest_files_problem"
echo "$new_id" > "$guest_files_marker"
# logind reads its settings again; the rest take effect as each part starts.
systemctl reload systemd-logind 2>/dev/null || true
echo "The guest's scripts and settings are now this driver image's ($new_id)."
