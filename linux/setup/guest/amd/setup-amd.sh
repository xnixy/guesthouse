# Sourced by setup-own.sh when the image is for an AMD card, after its own functions. It sets up a
# Linux someone installed for the AMD card's GPU: the tools that check it, the scripts and their units,
# the package manager's hook for Mesa updates, and Mesa built with native context, here and now.
# shellcheck shell=bash disable=SC2154  # sourced by setup-own.sh, which sets these
. "$here/amd/amd-common.sh"

check_system_for_amd() {
    [ "$(id -u)" = 0 ] || refuse "it has to run as root. Run it again with sudo."
    [ "$(uname -m)" = x86_64 ] || refuse "this only sets up x86_64 guests."
    [ -d "$root/run/systemd/system" ] || refuse "this guest doesn't run systemd, which checks the GPU at each boot."
    [ ! -e "$root/run/ostree-booted" ] ||
        refuse "this is an image-based system (ostree), whose /usr can't take packages this way."
    [ -n "$(amd_mode)" ] || refuse "this boot isn't on an AMD card. Choose the AMD card in the machine's settings, and start it again."
}

install_amd_packages() {
    family_refresh "$family"
    # shellcheck disable=SC2046  # one package a word
    family_install "$family" $(grep -v '^#' "$here/files/amd-packages-$family.txt") ||
        refuse "$(package_manager "$family") could not install what the check needs, as it says above."
}

install_mesa_hook() {
    case $family in
    debian) install -D -m 644 "$here/files/hooks/apt-nvfwd-mesa.conf" "$root/etc/apt/apt.conf.d/90nvfwd-mesa" ;;
    fedora)
        install -D -m 644 "$here/files/hooks/nvfwd-mesa.actions" \
            "$root/etc/dnf/libdnf5-plugins/actions.d/nvfwd-mesa.actions"
        ;;
    arch) install -D -m 644 "$here/files/hooks/91-nvfwd-mesa.hook" "$root/etc/pacman.d/hooks/91-nvfwd-mesa.hook" ;;
    suse) echo "zypper has no hook here, so after an update of Mesa the next boot builds it again." ;;
    esac
}

install_amd_units() {
    install -D -m 644 "$here/units/nvfwd-amd.service" "$units/nvfwd-amd.service"
    install -D -m 644 "$here/units/nvfwd-mesa-build.service" "$units/nvfwd-mesa-build.service"
    if selinux_on && command -v semanage > /dev/null; then
        semanage fcontext -a -t bin_t '/usr/lib/nvfwd(/.*)?' 2>/dev/null ||
            semanage fcontext -m -t bin_t '/usr/lib/nvfwd(/.*)?'
        restorecon -R "$installed" "$units/nvfwd-amd.service" "$units/nvfwd-mesa-build.service" \
            "$root/etc/dnf" 2>/dev/null || true
    fi
    systemctl daemon-reload
    systemctl enable nvfwd-amd.service
}

# Here, with its progress on the terminal, so the person who ran this sees it through.
build_mesa_now() {
    local mode
    mode=$(amd_mode)
    if mesa_has_native_context || mesa_is_ours; then
        echo "Mesa $(mesa_version) has native context already."
    elif ! "$installed/amd/mesa-build.sh"; then
        say_amd "failed $(sed 's/\.$//' "$amd_state/build-failure" 2>/dev/null || echo the Mesa build failed), see $mesa_log"
        echo
        echo "The GPU still works: shut the guest down and start it again, and it goes on with Venus, which needs no build."
        exit 1
    fi
    [ "$mode" = native ] || { echo "Start the VM again, and it goes on native context."; return 0; }
    if ! report_renderer native; then
        echo "Native context doesn't work with this Mesa. Shut the guest down and start it again, and it goes on with Venus."
        exit 1
    fi
    echo "Native context works: $(gl_renderer)."
}

setup_amd() {
    check_system_for_amd
    kernel=$(uname -r)
    choose_family
    own_family=$family
    install_amd_packages
    copy_scripts
    install_mesa_hook
    install_amd_units
    build_mesa_now
    touch "$own_done_marker"
    echo
    echo "Done. Log out and in again, or reboot, and the desktop runs on the new Mesa."
}
