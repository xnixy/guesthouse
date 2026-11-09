# QEMU's bridge helper, which root puts outside HOME. The stand-in sudo runs as the test's user, so
# "setuid" here is only the bit, and chown is written down.

helper_line() {
    grep "^root-file" "$H/.local/lib/guesthouse/manifest"
}

t_the_bridge_helper_goes_in_setuid_and_is_listed() {
    release 1.0
    give_everything
    rm -rf "$T/system"
    install_version 1.0
    check_exit 0
    check_said "QEMU's bridge helper in $T/system/libexec/guesthouse, setuid root"
    [ -u "$(helper_path)" ] || fail "the helper isn't setuid"
    check_same "$(helper_path)" "$H/.local/lib/guesthouse/current/qemu/build/qemu-bridge-helper" "it isn't the release's"
    check_log "chown 0:0 $(helper_path).new"
    check_gone "$(helper_path).new"
    [ "$(helper_line | cut -f 3)" = "$(helper_path)" ] || fail "the manifest doesn't list it"
}

t_a_helper_in_place_needs_no_root() {
    release 1.0
    give_everything
    install_version 1.0
    check_exit 0
    check_said "Nothing here needs root."
    check_not_log "sudo"
}

t_a_copy_that_isnt_the_release_s_is_never_made_setuid() {
    release 1.0
    give_everything
    rm -rf "$T/system"
    # Root's check against the hash taken before it ran, as if the file had been swapped since.
    sed -i 's|sha256sum -c --quiet|sed s/^./0/ \| sha256sum -c --quiet|' "$(script_for 1.0)"
    install_version 1.0
    check_exit 1
    check_gone "$(helper_path)"
    check_gone "$(helper_path).new"
}

t_running_the_same_version_again_mends_the_helper() {
    release 1.0
    give_everything
    install_version 1.0
    check_exit 0
    chmod 755 "$(helper_path)"
    install_version 1.0
    check_exit 0
    check_not_said "is the latest release"
    [ -u "$(helper_path)" ] || fail "the helper isn't setuid again"
    : >"$T/log"
    install_version 1.0
    check_said "is the latest release"
    check_not_log "sudo"
}

t_an_update_with_a_new_helper_puts_it_in() {
    release 1.0
    HELPER_VERSION=2 release 1.1
    give_everything
    install_version 1.0
    install_version 1.1
    check_exit 0
    check_log "sudo sh -c { f="
    check_same "$(helper_path)" "$H/.local/lib/guesthouse/current/qemu/build/qemu-bridge-helper" "it's still the old one"
    [ "$(helper_line | wc -l)" -eq 1 ] || fail "the manifest lists the helper more than once"
}

t_uninstall_takes_the_helper_away_as_root() {
    release 1.0
    give_everything
    rm -rf "$T/system"
    install_version 1.0
    answering 'yes\n' "$SHELL_UNDER_TEST" "$(uninstaller)"
    check_exit 0
    check_said "This removes it, as root:"
    check_log "sudo sh -c rm -f -- \"$(helper_path)\""
    check_gone "$(helper_path)"
    check_gone "$T/system/libexec/guesthouse"
}

t_uninstall_leaves_a_helper_another_install_replaced() {
    release 1.0
    give_everything
    rm -rf "$T/system"
    install_version 1.0
    give_helper 9
    answering 'yes\n' "$SHELL_UNDER_TEST" "$(uninstaller)"
    check_exit 0
    check_not_said "This removes it, as root:"
    check_there "$(helper_path)"
}

# The manifest is the user's to write, so root never removes a path it names.
t_uninstall_never_lets_the_manifest_aim_root() {
    release 1.0
    give_everything
    install_version 1.0
    printf 'precious\n' >"$T/precious"
    printf 'root-file\t%s\t%s\n' "$(sha256sum "$T/precious" | cut -d ' ' -f 1)" "$T/precious" \
        >>"$H/.local/lib/guesthouse/manifest"
    answering 'yes\n' "$SHELL_UNDER_TEST" "$(uninstaller)"
    check_exit 0
    check_there "$T/precious"
    check_not_log "sudo"
}
