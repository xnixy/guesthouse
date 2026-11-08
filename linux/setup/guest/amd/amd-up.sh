#!/bin/bash
# Runs on every boot, before the desktop. On a boot on an AMD card it says which way the guest's 3D
# goes, and on native context with a Mesa that can't, it starts the build. The desktop comes up on
# the virtio GPU either way, as it can draw with the distro's Mesa while the build runs.
set -uo pipefail
here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
. "$here/../common.sh"
. "$here/amd-common.sh"
mode=$(amd_mode)
[ -n "$mode" ] || exit 0
mkdir -p "$state_folder"
printf 'gpu=amd\nmode=%s\n' "$mode" > "$state_file"
if [ "$mode" = venus ]; then
    report_renderer venus
    exit 0
fi
# A distro's Mesa may have native context already, as Arch's does. Then there's nothing to build.
gl=$(gl_renderer)
if is_native_gl "$gl"; then
    report_renderer native
    exit 0
fi
if mesa_is_ours || mesa_has_native_context; then
    say_amd "failed GL is on ${gl:-nothing} with Mesa $(mesa_version), which has native context"
    exit 0
fi
echo "Mesa $(mesa_version) has no native context, so it's built again for it."
systemctl start --no-block nvfwd-mesa-build.service
