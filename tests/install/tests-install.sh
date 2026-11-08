# What ends up where.

t_it_installs_where_the_app_looks() {
    release 1.0 windows
    give_everything
    install_version 1.0
    check_exit 0
    root=$H/.local/lib/guesthouse
    [ "$(readlink "$root/current")" = 1.0 ] || fail "current isn't 1.0"
    check_there "$root/1.0/qemu/build/qemu-system-x86_64"
    check_there "$root/1.0/guest/setup.ps1"
    check_there "$root/1.0/uninstall.sh"
    check_there "$root/manifest"
    check_there "$H/.local/share/applications/io.github.xnixy.Guesthouse.desktop"
    check_there "$H/.local/share/icons/hicolor/scalable/apps/io.github.xnixy.Guesthouse.svg"
    check_there "$H/.local/share/icons/hicolor/symbolic/apps/io.github.xnixy.Guesthouse-symbolic.svg"
    [ -z "$(find "$root" -name '.incoming*')" ] || fail "staging was left behind"
}

t_the_launcher_finds_the_program_with_no_help_from_path() {
    release 1.0
    give_everything
    install_version 1.0
    grep -qxF "Exec=env \"GUESTHOUSE_PROGRAM=$H/.local/lib/guesthouse/current\" \"$H/.local/lib/guesthouse/current/guesthouse\"" \
        "$H/.local/share/applications/io.github.xnixy.Guesthouse.desktop" || fail "the launcher's Exec is wrong"
}

t_a_home_with_a_space_works() {
    release 1.0
    give_everything
    H="$T/my home"
    mkdir -p "$H"
    snapshot >"$T/before"
    install_version 1.0
    check_exit 0
    check_there "$H/.local/lib/guesthouse/current/guesthouse"
    answering 'yes\n' "$SHELL_UNDER_TEST" "$H/.local/lib/guesthouse/current/uninstall.sh"
    check_exit 0
    check_home_is "$T/before"
}

installed_app() {
    sh "$H/.local/lib/guesthouse/1.0/guesthouse"
}

t_a_new_gtk_keeps_the_app_with_hdr() {
    export FAKE_GTK=4.20
    release 1.0 windows
    give_everything
    install_version 1.0
    check_exit 0
    [ "$(installed_app)" = "guesthouse 1.0" ] || fail "it isn't the app with HDR"
    check_gone "$H/.local/lib/guesthouse/1.0/guesthouse-no-hdr"
}

t_gtk_4_16_is_new_enough_for_hdr() {
    export FAKE_GTK=4.16
    release 1.0 windows
    give_everything
    install_version 1.0
    [ "$(installed_app)" = "guesthouse 1.0" ] || fail "it isn't the app with HDR"
}

t_an_older_gtk_gets_the_app_without_hdr() {
    export FAKE_GTK=4.14
    release 1.0 windows
    give_everything
    install_version 1.0
    check_exit 0
    [ "$(installed_app)" = "guesthouse-no-hdr 1.0" ] || fail "it isn't the app without HDR"
}

t_gtk_it_cannot_read_gets_the_app_that_runs_anywhere() {
    export FAKE_GTK=
    release 1.0 windows
    give_everything
    install_version 1.0
    [ "$(installed_app)" = "guesthouse-no-hdr 1.0" ] || fail "it isn't the app without HDR"
}

t_a_card_that_cannot_run_still_installs_plain_vms() {
    release 1.0 "windows linux"
    give_everything
    export FAKE_GPU_REFUSED="No supported card."
    install_version 1.0
    check_exit 0
    check_said "plain VMs only"
    check_said "No supported card."
    check_there "$H/.local/lib/guesthouse/current/qemu/build/qemu-system-x86_64"
    check_gone "$H/.local/lib/guesthouse/1.0/guest"
    check_gone "$H/.local/lib/guesthouse/1.0/linux"
}

t_an_amd_card_keeps_the_linux_files_when_nvidias_cannot_run() {
    release 1.0 "windows linux"
    give_everything
    export FAKE_GPU_REFUSED="No supported card."
    export FAKE_AMD=yes
    install_version 1.0
    check_exit 0
    check_said "its AMD card can be a Linux guest's GPU"
    check_there "$H/.local/lib/guesthouse/1.0/linux"
}

t_a_libdrm_too_old_for_qemu_stops_the_install() {
    release 1.0
    give_everything
    printf 'amdgpu_device_initialize' >"$T/libraries/libdrm_amdgpu.so.1"
    install_version 1.0
    check_exit 1
    check_said "libdrm on this system is older than 2.4.121"
    check_gone "$H/.local/lib/guesthouse/1.0"
}

t_a_qemu_that_cannot_load_stops_the_install_and_says_why() {
    release 1.0
    give_everything
    tree=$T/releases/tree-1.0
    printf '#!/bin/sh\necho "libepoxy.so.0: cannot open shared object file" >&2\nexit 127\n' \
        >"$tree/guesthouse-1.0-x86_64/qemu/build/qemu-system-x86_64"
    tar -C "$tree" -cJf "$T/releases/v1.0/guesthouse-1.0-x86_64.tar.xz" guesthouse-1.0-x86_64
    wanted=$(sha256sum "$T/releases/v1.0/guesthouse-1.0-x86_64.tar.xz" | cut -d ' ' -f 1)
    sed -i "s/^PAYLOAD_SHA256=.*/PAYLOAD_SHA256=\"$wanted\"/" "$(script_for 1.0)"
    install_version 1.0
    check_exit 1
    check_said "QEMU can't start on this system: libepoxy.so.0: cannot open shared object file"
}

t_a_release_with_no_gpu_part_says_so() {
    release 1.0
    give_everything
    install_version 1.0
    check_exit 0
    check_said "no GPU part"
}

t_a_check_that_will_not_run_means_plain_vms() {
    release 1.0 windows
    give_everything
    tree=$T/releases/tree-1.0
    rm -f "$tree/guesthouse-1.0-x86_64/guesthouse-check"
    tar -C "$tree" -cJf "$T/releases/v1.0/guesthouse-1.0-x86_64.tar.xz" guesthouse-1.0-x86_64
    wanted=$(sha256sum "$T/releases/v1.0/guesthouse-1.0-x86_64.tar.xz" | cut -d ' ' -f 1)
    sed -i "s/^PAYLOAD_SHA256=.*/PAYLOAD_SHA256=\"$wanted\"/" "$(script_for 1.0)"
    install_version 1.0
    check_exit 0
    check_gone "$H/.local/lib/guesthouse/1.0/guest"
}

t_a_glibc_too_old_is_said_plainly() {
    release 1.0
    give_everything
    printf '#!/bin/sh\necho glibc 2.20\n' >"$T/bin/getconf.new"
    chmod +x "$T/bin/getconf.new"
    mv -f "$T/bin/getconf.new" "$T/bin/getconf"
    snapshot >"$T/before"
    install_version 1.0
    check_exit 1
    check_said "needs glibc 2.30"
    check_home_is "$T/before"
}


# A source bundle whose build script just packs what make-release made, to prove the path around it.
fake_source_bundle() {
    local bundle=$T/releases/guesthouse-1.0-source
    mkdir -p "$bundle/packaging"
    cat >"$bundle/packaging/build-release.sh" <<EOF
#!/bin/sh
mkdir -p "\$2"
cp "$T/releases/built.tar.xz" "\$2/guesthouse-1.0-x86_64.tar.xz"
echo "built from source"
EOF
    tar -C "$T/releases" -cJf "$T/releases/v1.0/guesthouse-1.0-source.tar.xz" guesthouse-1.0-source
    wanted=$(sha256sum "$T/releases/v1.0/guesthouse-1.0-source.tar.xz" | cut -d ' ' -f 1)
    sed -i "s/^SOURCE_SHA256=.*/SOURCE_SHA256=\"$wanted\"/" "$(script_for 1.0)"
}

t_from_source_builds_and_installs_the_same_way() {
    release 1.0
    give_everything
    give_tools cargo ninja meson
    mv "$T/releases/v1.0/guesthouse-1.0-x86_64.tar.xz" "$T/releases/built.tar.xz"
    fake_source_bundle
    install_version 1.0 --from-source
    check_exit 0
    check_said "built from source"
    check_there "$H/.local/lib/guesthouse/current/guesthouse"
    check_not_log "x86_64.tar.xz"
}

t_from_source_with_no_tools_says_what_it_needs() {
    release 1.0
    give_everything
    fake_source_bundle
    snapshot >"$T/before"
    install_version 1.0 --from-source
    check_exit 1
    check_said "building needs cargo, ninja and meson"
    check_home_is "$T/before"
}
