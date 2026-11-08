#!/bin/bash
# Sets up the forwarded GPU in a Linux guest someone installed by hand, or its AMD card's GPU. Run it
# once, as root, from the image the app attaches when the machine's Linux GPU setting is on:
#     sudo mount -o ro LABEL=NVFWD_DRIVER /mnt
#     sudo bash /mnt/setup/setup-own.sh
# Run it again after updating the app, to take its new scripts. It installs what the build needs,
# builds our modules once, and from then on gpu-up.sh brings the GPU up at each boot, as in Ubuntu.
set -euo pipefail
here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
. "$here/common.sh"
. "$here/distro.sh"
. "$here/own-desktop.sh"
image=$(dirname "$here")
installed=$root/usr/lib/nvfwd
units=$root/etc/systemd/system
# EFI's global variable: its fifth byte is 1 when Secure Boot is on.
secure_boot_variable=$root/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c

refuse() {
    echo "Not set up: $1" >&2
    exit 1
}

secure_boot_on() {
    [ -e "$secure_boot_variable" ] && [ "$(od -An -tu1 -j4 -N1 "$secure_boot_variable" | tr -d ' ')" = 1 ]
}

selinux_on() {
    [ -e "$root/sys/fs/selinux/enforce" ]
}

check_system() {
    [ "$(id -u)" = 0 ] || refuse "it has to run as root. Run it again with sudo."
    [ "$(uname -m)" = x86_64 ] || refuse "the GPU is only forwarded to x86_64 guests."
    [ -d "$root/run/systemd/system" ] || refuse "this guest doesn't run systemd, which brings the GPU up at each boot."
    [ -e "$image/manifest" ] && [ -e "$image/sources.tar" ] ||
        refuse "run it from the driver image, as /mnt/setup/setup-own.sh after mounting LABEL=NVFWD_DRIVER on /mnt."
    # An image-based system has a read-only /usr, and its own way to add packages.
    [ ! -e "$root/run/ostree-booted" ] ||
        refuse "this is an image-based system (ostree), whose /usr can't take packages this way."
    secure_boot_on && refuse "Secure Boot is on, and it won't load a module that isn't signed. Turn it off in the guest's firmware."
    return 0
}

choose_family() {
    local name
    name=$(os_value "$root/etc/os-release" PRETTY_NAME 2>/dev/null) || true
    [ -n "$name" ] || name=$(os_value "$root/etc/os-release" NAME 2>/dev/null) || true
    family=$(distro_family "$root/etc/os-release" 2>/dev/null) ||
        refuse "${name:-this distribution} isn't one this knows. It knows $known_families."
    command -v "$(package_manager "$family")" > /dev/null ||
        refuse "${name:-this distribution} has no $(package_manager "$family"), which it needs."
    echo "This is ${name:-a $family system}, so packages come from $(package_manager "$family")."
}

install_packages() {
    local packages headers follower
    headers=$(headers_package "$family" "$kernel")
    follower=$(headers_follower "$family" "$kernel")
    # shellcheck disable=SC2207  # one package a word
    packages=($(family_packages "$family" "$here") "$headers")
    selinux_on && packages+=(policycoreutils-python-utils)
    family_refresh "$family"
    family_install "$family" "${packages[@]}" ||
        refuse "$(package_manager "$family") could not install what the build needs, as it says above."
    if [ -n "$follower" ]; then
        family_install "$family" "$follower" ||
            echo "There is no $follower here, so install the headers by hand with each new kernel."
    fi
    [ -e "$root/lib/modules/$kernel/build/Makefile" ] ||
        refuse "there are no headers for the running kernel, $kernel. If the kernel was updated since this boot, reboot and run this again."
}

copy_scripts() {
    local desktop
    desktop=
    [ ! -e "$own_install" ] || desktop=$(value_of "$own_install" desktop)
    rm -rf "$installed"
    mkdir -p "$installed" "$(dirname "$own_install")" "$log_folder" "$(dirname "$own_done_marker")"
    cp -R "$here/." "$installed/"
    printf 'family=%s\nlibraries=%s\n' "$family" "$(libraries_folder "$family")" > "$own_install"
    # Someone who put their desktop back on the plain screen keeps it there.
    [ -z "$desktop" ] || echo "desktop=$desktop" >> "$own_install"
}

install_hook() {
    case $(hook_kind "$family") in
    kernel-install)
        install -D -m 755 "$here/files/hooks/90-nvfwd.install" "$root/etc/kernel/install.d/90-nvfwd.install"
        echo "A new kernel is built for as kernel-install adds it, and at its first boot if that missed it."
        ;;
    kernel-postinst)
        install -D -m 755 "$here/files/hooks/kernel-postinst" "$root/etc/kernel/postinst.d/nvfwd"
        install -D -m 755 "$here/files/hooks/kernel-postinst" "$root/etc/kernel/header_postinst.d/nvfwd"
        echo "A new kernel is built for once its headers are in, and at its first boot if that missed it."
        ;;
    pacman)
        install -D -m 644 "$here/files/hooks/90-nvfwd.hook" "$root/etc/pacman.d/hooks/90-nvfwd.hook"
        echo "A new kernel is built for as pacman installs its headers, and at its first boot if that missed it."
        ;;
    none)
        echo "A new kernel is built for at its first boot, which then takes a minute or two longer."
        ;;
    esac
}

install_settings() {
    install -D -m 644 "$here/files/modprobe-nvfwd.conf" "$root/etc/modprobe.d/nvfwd.conf"
    install -D -m 644 "$here/files/70-nvfwd-own.rules" "$root/etc/udev/rules.d/70-nvfwd.rules"
    install -D -m 644 "$here/files/logind-nvfwd.conf" "$root/etc/systemd/logind.conf.d/90-nvfwd.conf"
    # Plasma's power manager asks before it shuts down, unless someone has set it up already.
    if [ -e "$root/usr/lib/systemd/user/plasma-powerdevil.service" ] && [ ! -e "$root/etc/xdg/powerdevilrc" ]; then
        install -D -m 644 "$here/files/powerdevilrc" "$root/etc/xdg/powerdevilrc"
    fi
    install -D -m 644 "$here/units/nvfwd-gpu.service" "$units/nvfwd-gpu.service"
    install -D -m 644 "$here/units/nvfwd-desktop-watch.service" "$units/nvfwd-desktop-watch.service"
    mkdir -p "$root/usr/local/sbin"
    ln -sf /usr/lib/nvfwd/check.sh "$root/usr/local/sbin/nvfwd-check"
    install_hook
    install_desktop
}

# The login screen waits for the GPU step, which puts it on the GPU or leaves it on the plain screen.
# SDDM starts its own compositor itself, so it takes the boot's settings from the same file. Any other
# display manager is left as it is, on the plain screen.
install_desktop() {
    local manager kind
    manager=$(display_manager)
    kind=$(manager_kind "$manager")
    rm -f "$units"/*.service.d/nvfwd.conf
    case $kind in
    gdm | plasmalogin)
        install -D -m 644 "$here/files/display-manager-nvfwd.conf" "$units/$manager.d/nvfwd.conf"
        echo "The login screen and your desktop go on the GPU at each boot that has it. If the login screen doesn't come up there, the guest boots again on the plain screen."
        ;;
    sddm)
        install -D -m 644 "$here/files/sddm-nvfwd.conf" "$units/$manager.d/nvfwd.conf"
        echo "SDDM and Plasma go on the GPU at each boot that has it, if SDDM's login screen is its Wayland one. If it doesn't come up there, the guest boots again on the plain screen."
        ;;
    *)
        if [ -z "$manager" ]; then
            echo "There's no display manager enabled, so there's no desktop to put on the GPU."
        else
            echo "Your display manager is ${manager%.service}. Only GDM, SDDM and Plasma Login are set up for the GPU so far, so your desktop stays on the plain screen."
        fi
        ;;
    esac
}

# Programs that systemd starts from /usr/lib/nvfwd must be labelled as programs, or it runs them
# confined. The image itself gets its label when gpu-up.sh mounts it.
label_for_selinux() {
    selinux_on || return 0
    if ! command -v semanage > /dev/null; then
        echo "SELinux is on, and there's no semanage to label the scripts with. Install policycoreutils-python-utils and run this again."
        return 0
    fi
    semanage fcontext -a -t bin_t '/usr/lib/nvfwd(/.*)?' 2>/dev/null ||
        semanage fcontext -m -t bin_t '/usr/lib/nvfwd(/.*)?'
    restorecon -R "$installed" "$units/nvfwd-gpu.service" "$units/nvfwd-desktop-watch.service" \
        "$units"/*.service.d/nvfwd.conf "$root/etc/modprobe.d/nvfwd.conf" "$root/etc/udev/rules.d/70-nvfwd.rules" \
        "$root/etc/systemd/logind.conf.d" "$root/etc/kernel" "$root/lib/modules/$kernel/updates" 2>/dev/null || true
    echo "SELinux is on: the scripts are labelled as programs, and the driver image is mounted as libraries."
}

build_now() {
    local version image_id log=$log_folder/build.log target=$root/lib/modules/$kernel/updates/nvfwd
    version=$(value_of "$image/manifest" version)
    image_id=$(value_of "$image/manifest" id)
    echo "Building for driver $version on kernel $kernel. This takes a minute or two."
    if ! "$installed/build-parts.sh" "$image/sources.tar" "$version" "$kernel" > "$log" 2>&1; then
        tail -n 20 "$log" >&2
        refuse "the build failed. The whole log is in $log."
    fi
    echo "$image_id $kernel" > "$target/built-for"
}

# The image for an AMD card has no driver: the guest's Mesa is what's set up.
if [ "$(value_of "$image/manifest" gpu 2>/dev/null)" = amd ]; then
    . "$here/amd/setup-amd.sh"
    setup_amd
    exit 0
fi

check_system
kernel=$(uname -r)
choose_family
install_packages
copy_scripts
install_settings
build_now
label_for_selinux
systemctl daemon-reload
systemctl enable nvfwd-gpu.service nvfwd-desktop-watch.service
touch "$own_done_marker"
echo
echo "Done. Reboot, and the GPU comes up during the boot. Then, as root, nvfwd-check says what works."
