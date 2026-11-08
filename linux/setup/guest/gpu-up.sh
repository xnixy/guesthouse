#!/bin/bash
# Runs on every boot, before the desktop. Finds out whether this boot gets the GPU, and sets it up.
# A guest must always boot, so whatever goes wrong here turns the GPU off, says why, and exits 0.
set -uo pipefail
here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
. "$here/common.sh"
. "$here/amd/amd-common.sh"
kernel=$(uname -r)
target=$root/lib/modules/$kernel/updates/nvfwd
build_log=$log_folder/build.log
first_check_seconds=60
selinux_label=system_u:object_r:lib_t:s0
mkdir -p "$state_folder" "$log_folder" "$driver"

gpu_off() {
    echo "GPU off: $1"
    tell_host "nvfwd-gpu off $1"
    "$here/link-driver.sh" unlink
    [ -z "${gdm_rules_override:-}" ] || rm -f "$gdm_rules_override"
    mountpoint -q "$driver" && umount "$driver"
    printf 'gpu=off\nreason=%s\n' "$1" > "$state_file"
    mkdir -p "$root/run/motd.d"
    printf 'This guest has no GPU on this boot: %s\n' "$1" > "$root/run/motd.d/50-nvfwd"
    exit 0
}

device_present() {
    local device
    for device in "$root"/sys/bus/pci/devices/*; do
        if [ "$(cat "$device/vendor" 2>/dev/null)" = "$nvfwd_vendor" ] &&
            [ "$(cat "$device/device" 2>/dev/null)" = "$nvfwd_device" ]; then
            nvfwd_pci=$device
            return 0
        fi
    done
    return 1
}

# The call slots are BAR 2 and the window BAR 4, side by side: the range from one to the other.
cache_device_memory() {
    local slots window start end
    slots=$(sed -n 3p "$nvfwd_pci/resource" 2>/dev/null)
    window=$(sed -n 5p "$nvfwd_pci/resource" 2>/dev/null)
    # shellcheck disable=SC2086  # three numbers each
    set -- $slots $window
    [ $# = 6 ] && [ $(($4)) != 0 ] || return 0
    start=$(($1 < $4 ? $1 : $4))
    end=$(($2 > $5 ? $2 : $5))
    "$here/window-cache.sh" "$start" $((end + 1 - start))
}

# The host leaves its reason in fw_cfg when it starts a VM with no GPU.
host_reason() {
    local reason
    modprobe qemu_fw_cfg
    reason=$(tr '\n' ' ' < "$root/sys/firmware/qemu_fw_cfg/by_name/opt/nvfwd/gpu-off-reason/raw" 2>/dev/null)
    reason=${reason% }
    echo "no nvfwd device. ${reason:-The VM was started without one, and the host gave no reason.}"
}

# With SELinux, the whole image is labelled as libraries, which any program may map.
mount_image() {
    local disk options=ro,nodev,nosuid
    disk=$(blkid -L NVFWD_DRIVER) || return 1
    [ ! -e "$root/sys/fs/selinux/enforce" ] || options=$options,context=$selinux_label
    mount -t iso9660 -o "$options" "$disk" "$driver"
}

# The version only says the host's driver is the one we built for. On a version nobody has tried, the
# allow list or the device may refuse what it needs, and that only shows when something uses the GPU.
first_check() {
    local name=$1 program=$2 output
    output=$(timeout -k 5 "$first_check_seconds" "$built/$program" 2>&1)
    if [ $? != 0 ] || ! echo "$output" | grep -q ' 0 wrong'; then
        gpu_off "$name failed its first check on driver $version (tested: $tested): $(echo "$output" | tail -n 1). The host's calls.log says what the device refused"
    fi
}

# Mutter reads the cached mode list without probing, and with fbdev=0 nothing has probed yet.
probe_connectors() {
    local status
    for status in "$root"/sys/class/drm/card[0-9]-*/status; do
        [ ! -e "$status" ] || echo detect > "$status"
    done
}

# Links from an earlier boot must not outlive it, whatever this boot decides.
"$here/link-driver.sh" unlink
udevadm settle --timeout=30
# On an AMD card's GPU there's nothing of NVIDIA's to bring up, and amd-up.sh says how that one went.
[ -z "$(amd_mode)" ] || exit 0
device_present || gpu_off "$(host_reason)"
mount_image || gpu_off "the host offered no driver image"

version=$(value_of "$driver/manifest" version)
image_id=$(value_of "$driver/manifest" id)
tested=$(value_of "$driver/manifest" tested)
[ -n "$version" ] && [ -n "$image_id" ] || gpu_off "the driver image has no usable manifest"

built_for="$image_id $kernel"
if [ "$(cat "$target/built-for" 2>/dev/null)" != "$built_for" ]; then
    echo "Building for driver $version on kernel $kernel"
    if ! "$here/build-parts.sh" "$driver/sources.tar" "$version" > "$build_log" 2>&1; then
        gpu_off "the build for driver $version on kernel $kernel failed, see $build_log"
    fi
    echo "$built_for" > "$target/built-for"
fi

# GDM's rules act as the nvidia module loads, so whether they're wanted is settled before that.
if [ -n "$own_family" ]; then
    . "$here/own-desktop.sh"
    plain_reason=$(plain_desktop_reason "$built_for")
    [ -n "$plain_reason" ] || hide_gdm_rules
fi
cache_device_memory
modprobe nvidia || gpu_off "the nvidia module would not load"
host_version=$("$here/driver-version.py" 2>&1) || gpu_off "the host's driver did not answer: $host_version"
# The module cannot unload, but with no user-mode files linked nothing in the guest will use it.
[ "$host_version" = "$version" ] ||
    gpu_off "the image holds driver $version, but the host has $host_version loaded"

"$here/link-driver.sh" link || gpu_off "the driver could not be linked in"
# Before nvidia-drm: if these fail the plain desktop has to come up, and that module can't unload.
first_check CUDA cudavec
first_check Vulkan vkcompute

# What nvidia-drm leans on. nvidia-drm's options are in /etc/modprobe.d/nvfwd.conf.
load_nvidia_drm() {
    modprobe -a drm_kms_helper drm_ttm_helper video drm_display_helper
    modprobe nvidia-drm
}

gpu_on() {
    printf 'gpu=on\nversion=%s\ntested=%s\nbuilt_for=%s\n' "$version" "$tested" "$built_for" > "$state_file"
    tell_host "nvfwd-gpu on $version"
}

# A desktop someone installed goes on the GPU where it can, and stays on the plain screen where it can't.
# The GPU is on for programs either way.
own_desktop() {
    if [ -n "$plain_reason" ]; then
        gpu_on
        say_desktop plain "$plain_reason"
        echo "GPU on: driver $version, for programs. The desktop stays on the plain screen: $plain_reason"
    elif ! load_nvidia_drm; then
        rm -f "$gdm_rules_override"
        gpu_on
        say_desktop plain "nvidia-drm would not load"
        echo "GPU on: driver $version, for programs. nvidia-drm would not load, so the desktop stays on the plain screen"
    else
        write_session_settings
        hide_plain_cards
        probe_connectors
        gpu_on
        say_desktop gpu
        echo "GPU on: driver $version, with the desktop on it"
    fi
    exit 0
}

[ -z "$own_family" ] || own_desktop
load_nvidia_drm || gpu_off "nvidia-drm would not load"
hide_plain_cards
probe_connectors
gpu_on
echo "GPU on: driver $version"
