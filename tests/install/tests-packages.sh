# What it asks of root, on each distro.

expect_command() {
    local id=$1 like=$2 wanted=$3
    release 1.0
    fake_os "$id" "$like"
    no_terminal "$SHELL_UNDER_TEST" "$(script_for 1.0)" --dry-run
    check_exit 0
    check_said "$wanted"
}

t_fedora_packages() {
    expect_command fedora "" "sudo sh -c 'dnf install -y gtk4 libadwaita pipewire-libs libepoxy mesa-libgbm mesa-libEGL libdrm qemu-img edk2-ovmf libcap-ng && "
}

t_debian_packages() {
    expect_command debian "" "sudo sh -c 'apt-get update && apt-get install -y --no-remove libgtk-4-1 libadwaita-1-0 libpipewire-0.3-0t64 libepoxy0 libgbm1 libegl1 libdrm2 libdrm-amdgpu1 qemu-utils ovmf libcap-ng0 && "
}

t_ubuntu_packages() {
    expect_command ubuntu debian "apt-get install -y --no-remove libgtk-4-1"
}

t_arch_packages() {
    expect_command arch "" "sudo sh -c 'pacman -S --needed --noconfirm gtk4 libadwaita libpipewire libepoxy mesa libglvnd libdrm qemu-img edk2-ovmf libcap-ng && "
}

t_a_derivative_uses_its_parents_names() {
    expect_command nobara "rhel fedora" "dnf install -y gtk4"
}

t_opensuse_packages() {
    expect_command opensuse-tumbleweed "opensuse suse" "sudo sh -c 'zypper --non-interactive install libgtk-4-1"
}

t_gpu_machines_bring_their_tools() {
    release 1.0 "windows linux"
    no_terminal "$SHELL_UNDER_TEST" "$(script_for 1.0)" --dry-run
    check_said "edk2-ovmf libcap-ng 7zip genisoimage swtpm swtpm-tools openssl openssh bubblewrap gcc && "
}

t_root_runs_once_and_exactly_what_was_shown() {
    release 1.0
    install_version 1.0
    check_exit 0
    [ "$(grep -c '^sudo' "$T/log")" -eq 1 ] || fail "sudo ran more than once"
    shown=$(sed -n "s/^  sudo sh -c '\(.*\)'$/\1/p" "$T/said")
    [ "$(grep '^sudo' "$T/log")" = "sudo sh -c $shown" ] || fail "what ran isn't what was shown"
    check_log "dnf install -y gtk4"
}

t_no_root_when_everything_is_here() {
    release 1.0
    give_everything
    install_version 1.0
    check_exit 0
    check_said "Nothing here needs root."
    check_not_log "sudo"
}

t_a_failed_package_install_installs_nothing() {
    release 1.0
    export SUDO_FAILS=yes
    snapshot >"$T/before"
    install_version 1.0
    check_exit 1
    check_home_is "$T/before"
}

# Root adds no packages there, but /usr/local still takes the bridge helper.
t_an_immutable_system_gets_user_files_and_a_list() {
    release 1.0
    OSTREE=yes
    install_version 1.0
    check_exit 0
    check_said "sudo rpm-ostree install gtk4"
    check_not_log "dnf"
    check_log "sudo sh -c { f="
    [ -u "$(helper_path)" ] || fail "the helper isn't setuid"
    check_there "$H/.local/lib/guesthouse/current/guesthouse"
    ! grep -q "^package" "$H/.local/lib/guesthouse/manifest" || fail "the manifest lists packages it never added"
}

t_an_unknown_distro_gets_a_list_and_no_commands() {
    release 1.0
    fake_os plan9
    no_terminal "$SHELL_UNDER_TEST" "$(script_for 1.0)" --dry-run
    check_exit 0
    check_said "I don't know your distro's names"
    check_not_said "sudo sh -c 'dnf"
    check_not_said "sudo sh -c 'apt"
}

t_a_closed_kvm_joins_the_one_root_block_and_comes_back_out() {
    release 1.0
    give_everything
    KVM_CLOSED=yes
    : >"$T/closed-kvm"
    chmod 000 "$T/closed-kvm"
    snapshot >"$T/empty"
    install_version 1.0
    check_exit 0
    check_said "sudo sh -c 'usermod -aG kvm"
    check_said "Log out and in again"
    [ "$(grep -c '^sudo' "$T/log")" -eq 1 ] || fail "sudo ran more than once"
    answering 'yes\n' "$SHELL_UNDER_TEST" "$(uninstaller)"
    check_exit 0
    check_said "sudo gpasswd -d"
    check_log "gpasswd -d"
    check_home_is "$T/empty"
}
