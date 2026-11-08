# Sourced by the scripts here. NVFWD_ROOT is empty in a guest. The tests point it at a scratch folder.
# shellcheck shell=bash disable=SC2034  # the scripts that source this use these
root=${NVFWD_ROOT:-}
driver=$root/opt/nv/driver        # the host's read-only image is mounted here
built=$root/opt/nv/built          # what this guest built from the image's sources
layers=$root/opt/nv/layer         # the Vulkan layers. Their manifests name this folder
state_folder=$root/run/nvfwd
state_file=$state_folder/state    # gpu=on or gpu=off, and why. The desktop and the check read it
log_folder=$root/var/log/nvfwd
done_marker=$root/var/lib/nvfwd/first-boot-done
libraries=$root/usr/lib/x86_64-linux-gnu
nvfwd_vendor=0x1234
nvfwd_device=0x11ea
# Where the guest's own scripts are installed, and which driver image's copy of them they are.
installed=$root/usr/lib/nvfwd
guest_files_marker=$root/var/lib/nvfwd/guest-files
guest_files_problem=$root/var/lib/nvfwd/guest-files-problem
# Written by setup-own.sh, in a guest someone installed by hand. Ubuntu's own install has none.
own_install=$root/etc/nvfwd/own-install.conf
own_done_marker=$root/var/lib/nvfwd/own-setup-done

# Reads one key from a file of key=value lines.
value_of() {
    sed -n "s/^$2=//p" "$1" | head -n 1
}

# The package family of a guest installed by hand, or empty for Ubuntu's own install.
own_family=
if [ -e "$own_install" ]; then
    own_family=$(value_of "$own_install" family)
    libraries=$root$(value_of "$own_install" libraries)
fi

# The app on the host reads lines from here to say whether the guest has its GPU, and its desktop.
serial=$root/dev/ttyS0
plain_card_rules=$root/run/udev/rules.d/71-nvfwd-plain-card.rules

# A guest with no serial port says nothing, and that's no reason to stop.
tell_host() {
    [ -w "$serial" ] && printf '%s\n' "$1" >> "$serial" 2>/dev/null
    return 0
}

# Mutter would take the plain card as a second screen and drive both. It is only for a boot with no GPU.
# udev keeps a tag once given, so this lasts until the guest boots again.
hide_plain_cards() {
    local card
    mkdir -p "$(dirname "$plain_card_rules")"
    : > "$plain_card_rules"
    for card in "$root"/sys/class/drm/card[0-9]; do
        [ -e "$card" ] || continue
        [ "$(cat "$card/device/device" 2>/dev/null)" = "$nvfwd_device" ] && continue
        echo "SUBSYSTEM==\"drm\", KERNEL==\"$(basename "$card")\", TAG+=\"mutter-device-ignore\"" >> "$plain_card_rules"
    done
    udevadm control --reload
    udevadm trigger --subsystem-match=drm --action=change
    udevadm settle --timeout=10
}

# A short id for a folder of guest scripts, which changes with any file in it. Usage: guest_files_id FOLDER
guest_files_id() {
    (cd "$1" && find . -type f ! -path '*/__pycache__/*' -print0 | LC_ALL=C sort -z | xargs -0 sha256sum |
        sha256sum | cut -c1-16)
}
