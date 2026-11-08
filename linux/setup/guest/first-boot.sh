#!/bin/bash
# The part of the install that needs a running system: everything that does not depend on the host's
# driver. If it fails, the next boot tries again, and the marker only appears when all of it worked.
set -euo pipefail
here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
. "$here/common.sh"
export DEBIAN_FRONTEND=noninteractive

install_packages() {
    apt-get update
    # shellcheck disable=SC2046  # one package a word
    apt-get install -y "linux-headers-$(uname -r)" $(grep -v '^#' "$here/files/packages.txt")
    # With recommends, GNOME pulls in much that nothing here needs.
    # shellcheck disable=SC2046
    apt-get install -y --no-install-recommends $(grep -v '^#' "$here/files/packages-no-recommends.txt")
}

install_settings() {
    install -m 644 "$here/files/modprobe-nvfwd.conf" /etc/modprobe.d/nvfwd.conf
    install -m 644 "$here/files/70-nvfwd.rules" /etc/udev/rules.d/70-nvfwd.rules
    install -D -m 644 "$here/files/logind-nvfwd.conf" /etc/systemd/logind.conf.d/90-nvfwd.conf
    install -D -m 644 "$here/files/grub-nvfwd.cfg" /etc/default/grub.d/90-nvfwd.cfg
    update-grub
}

# Xwayland's glamor would wait on itself: see the wrapper. A diversion keeps it through upgrades.
wrap_xwayland() {
    dpkg-divert --local --rename --divert /usr/bin/Xwayland.real --add /usr/bin/Xwayland
    install -m 755 "$here/Xwayland" /usr/bin/Xwayland
}

# A login brings a user manager, whose socket units would take PipeWire's path from the session's own.
mask_user_sound() {
    systemctl --global mask pipewire.socket pipewire.service pipewire-pulse.socket pipewire-pulse.service wireplumber.service
}

# Ubuntu's firefox package is a snap, which is untested here. This is Mozilla's own build.
install_firefox() {
    local folder=/opt/ff
    mkdir -p "$folder"
    curl -fsSL 'https://download.mozilla.org/?product=firefox-latest&os=linux64&lang=en-US' -o "$folder/ff.tar"
    tar -xf "$folder/ff.tar" -C "$folder"
    rm "$folder/ff.tar"
    install -m 644 "$here/firefox.desktop" /usr/share/applications/firefox.desktop
}

enable_services() {
    systemctl enable "$here/units/nvfwd-gpu.service" "$here/units/nvfwd-desktop.service" \
        "$here/units/nvfwd-amd.service"
    systemctl link "$here/units/nvfwd-mesa-build.service"
    systemctl set-default graphical.target
}

# A later driver image's scripts and settings, once refresh.sh has put the scripts in place: the same
# files as a first boot installs, with no packages, no Firefox and no restart.
refresh() {
    install -m 644 "$here/files/modprobe-nvfwd.conf" "$root/etc/modprobe.d/nvfwd.conf"
    install -m 644 "$here/files/70-nvfwd.rules" "$root/etc/udev/rules.d/70-nvfwd.rules"
    install -D -m 644 "$here/files/logind-nvfwd.conf" "$root/etc/systemd/logind.conf.d/90-nvfwd.conf"
    if ! cmp -s "$here/files/grub-nvfwd.cfg" "$root/etc/default/grub.d/90-nvfwd.cfg"; then
        install -D -m 644 "$here/files/grub-nvfwd.cfg" "$root/etc/default/grub.d/90-nvfwd.cfg"
        update-grub
    fi
    # Only if the first boot diverted it: the wrapper needs Xwayland.real beside it.
    [ ! -e "$root/usr/bin/Xwayland.real" ] || install -m 755 "$here/Xwayland" "$root/usr/bin/Xwayland"
    ln -sf "$here/check.sh" "$root/usr/local/sbin/nvfwd-check"
    systemctl daemon-reload
    systemctl enable "$here/units/nvfwd-gpu.service" "$here/units/nvfwd-desktop.service"
}

if [ "${1:-}" = --refresh ]; then
    refresh
    exit 0
fi

# On an AMD card, Mesa is built again with native context after each of Ubuntu's updates of it.
install_mesa_hook() {
    install -m 644 "$here/files/hooks/apt-nvfwd-mesa.conf" /etc/apt/apt.conf.d/90nvfwd-mesa
}

install_packages
install_settings
install_mesa_hook
wrap_xwayland
mask_user_sound
install_firefox
mkdir -p "$log_folder" "$(dirname "$done_marker")"
ln -sf "$here/check.sh" /usr/local/sbin/nvfwd-check
enable_services
touch "$done_marker"
# With no reload: a reload would forget this unit while it runs, and with it the timeout, and systemd
# would kill this script. The marker already keeps it from running again.
systemctl disable --no-reload nvfwd-first-boot.service
# Restart, for the new default target and kernel settings. The GPU step runs from there.
systemctl --no-block reboot
