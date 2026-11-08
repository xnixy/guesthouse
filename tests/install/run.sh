#!/bin/bash
# Tests install.sh and uninstall.sh with no network, no root and no real install.
#
# Each test gets a folder of its own with a fake HOME in it, and runs in a bubblewrap sandbox where the
# rest of the filesystem is read-only. So a script that wrote anywhere else would fail the test, and the
# real HOME can't be reached. sudo, curl and the package managers are stand-ins, and PATH holds nothing
# else but the plain tools listed in `make_toolbox`.
#
# Usage: tests/install/run.sh [TEST...]. TEST_ROOT says where the folders go.
set -u

HERE=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH='' cd -- "$HERE/../.." && pwd)
TEST_ROOT=${TEST_ROOT:-$(mktemp -d)}
SHELL_UNDER_TEST=${SHELL_UNDER_TEST:-sh}
TOOLS='awk basename bash cat chmod cmp comm cp cut df dirname du env find getconf grep head id ldd ln mkdir mktemp mv readlink rm rmdir sed setsid sh sha256sum sleep sort tail tar touch tr uname wc xz'

passed=0
failed=0

fail() {
    printf '  FAILED: %s\n' "$*"
    test_failed=yes
}

check_said() {
    grep -qF -- "$1" "$T/said" || fail "it didn't say: $1"
}

check_not_said() {
    ! grep -qF -- "$1" "$T/said" || fail "it shouldn't have said: $1"
}

check_exit() {
    [ "$status" -eq "$1" ] || fail "exit $status, not $1"
}

check_log() {
    grep -qF -- "$1" "$T/log" || fail "the stand-ins' log lacks: $1"
}

check_not_log() {
    ! grep -qF -- "$1" "$T/log" 2>/dev/null || fail "the stand-ins' log has: $1"
}

check_there() {
    [ -e "$1" ] || [ -L "$1" ] || fail "$1 isn't there"
}

check_gone() {
    [ ! -e "$1" ] && [ ! -L "$1" ] || fail "$1 is still there"
}

check_same() {
    cmp -s "$1" "$2" || fail "$3"
}

# Every name, type, mode, link and file's hash under HOME.
snapshot() {
    (cd "$H" && find . -printf '%y %m %p %l\n' | sort
        find . -type f -exec sha256sum {} + | sort -k 2)
}

check_home_is() {
    snapshot >"$T/now"
    diff "$1" "$T/now" >"$T/difference" || {
        head -20 "$T/difference"
        fail "HOME isn't what it was"
    }
}

make_toolbox() {
    mkdir -p "$T/bin"
    for tool in $TOOLS; do
        ln -s "$(command -v "$tool")" "$T/bin/$tool"
    done
    for stand_in in curl sudo ldconfig; do
        ln -s "$HERE/stand-ins/$stand_in" "$T/bin/$stand_in"
    done
    for manager in dnf apt-get pacman zypper usermod gpasswd; do
        ln -s "$HERE/stand-ins/package-manager" "$T/bin/$manager"
    done
}

# Pretends a tool is on this PC.
give_tools() {
    for tool in "$@"; do
        printf '#!/bin/sh\n' >"$T/bin/$tool"
        chmod +x "$T/bin/$tool"
    done
}

# Libraries with every symbol the pretend releases ask for, and firmware, so nothing needs installing.
give_everything() {
    give_tools qemu-img 7z genisoimage swtpm isoinfo openssl ssh-keygen bwrap gcc
    export LIBRARIES_PRESENT=yes
    FIRMWARE_HERE=yes
}

fake_os() {
    printf 'ID=%s\nID_LIKE="%s"\nPRETTY_NAME="%s"\n' "$1" "${2:-}" "$1 test" >"$T/os-release"
}

new_test() {
    T=$TEST_ROOT/$1
    H="$T/home"
    mkdir -p "$H" "$T/releases" "$T/runtime" "$T/libraries"
    : >"$T/log"
    printf 'adw_new_thing' >"$T/libraries/libadwaita-1.so.0"
    printf 'gtk_new_thing' >"$T/libraries/libgtk-4.so.1"
    printf 'pipewire' >"$T/libraries/libpipewire-0.3.so.0"
    for library in libepoxy.so.0 libgbm.so.1 libEGL.so.1 libdrm.so.2; do printf 'gl' >"$T/libraries/$library"; done
    printf 'amdgpu_device_initialize2' >"$T/libraries/libdrm_amdgpu.so.1"
    unset LIBRARIES_PRESENT SUDO_FAILS FAKE_GPU_REFUSED FAKE_GTK FAKE_AMD
    FIRMWARE_HERE=no
    OSTREE=no
    KVM_CLOSED=no
    WITHOUT_NAMESPACE=no
    fake_os fedora
    make_toolbox
    test_failed=no
}

# The whole filesystem read-only, the test's folder writable, and these few things faked.
sandbox() {
    local options=(--ro-bind / / --dev-bind /dev /dev --bind "$T" "$T" --tmpfs /run
        --ro-bind "$T/os-release" /usr/lib/os-release)
    [ "$OSTREE" = no ] || options+=(--ro-bind "$T/os-release" /run/ostree-booted)
    [ "$KVM_CLOSED" = no ] || options+=(--ro-bind "$T/closed-kvm" /dev/kvm)
    mkdir -p "$T/firmware"
    [ "$FIRMWARE_HERE" = no ] || printf '{"targets": [{"architecture": "x86_64"}]}' >"$T/firmware/x.json"
    options+=(--ro-bind "$T/firmware" /usr/share/qemu/firmware)
    local wrapper=("$(command -v bwrap)" "${options[@]}")
    # A process in a namespace can't read /proc/PID/exe of one outside it, so a test with a pretend VM
    # runs with only the fake HOME around it.
    [ "$WITHOUT_NAMESPACE" = no ] || wrapper=()
    env -i HOME="$H" PATH="$T/bin" XDG_RUNTIME_DIR="$T/runtime" STAND_IN_LOG="$T/log" \
        FAKE_RELEASES="$T/releases" FAKE_LIBRARIES="$T/libraries" LIBRARIES_PRESENT="${LIBRARIES_PRESENT:-}" \
        FAKE_FIRMWARE="$T/firmware" SUDO_FAILS="${SUDO_FAILS:-}" FAKE_GPU_REFUSED="${FAKE_GPU_REFUSED:-}" FAKE_AMD="${FAKE_AMD:-}" FAKE_GTK="${FAKE_GTK-4.20}" \
        "${wrapper[@]}" "$@"
}

# Runs a script and keeps what it said and its exit code. ANSWERS is typed into a real terminal.
answering() {
    local answers=$1
    shift
    sandbox "$(command -v python3)" -I "$HERE/tty.py" "$answers" "$@" >"$T/said" 2>&1 </dev/null
    status=$?
}

no_terminal() {
    sandbox setsid "$@" >"$T/said" 2>&1 </dev/null
    status=$?
}

release() {
    sh "$HERE/make-release.sh" "$REPO" "$T/releases" "$@" || exit 1
}

script_for() {
    printf '%s' "$T/releases/install-$1.sh"
}

make_main_name() {
    cp "$(script_for "$1")" "$T/releases/main-install.sh"
}

install_version() {
    answering 'yes\n' "$SHELL_UNDER_TEST" "$(script_for "$1")" "${@:2}"
}

uninstaller() {
    printf '%s' "$REPO/uninstall.sh"
}

current_says() {
    sandbox "$H/.local/lib/guesthouse/current/guesthouse"
}

run_test() {
    printf '%s\n' "$1"
    new_test "$1"
    "$1"
    if [ "$test_failed" = yes ]; then
        failed=$((failed + 1))
    else
        passed=$((passed + 1))
    fi
}

for tests in "$HERE"/tests-*.sh; do
    # shellcheck source=/dev/null
    . "$tests"
done

whole_run=
if [ $# -eq 0 ]; then
    whole_run=yes
    set -- $(declare -F | awk '$3 ~ /^t_/ { print $3 }')
fi
for name in "$@"; do
    run_test "$name"
done
# So the driver's licences and the release's refusals don't rest on anyone remembering to look.
if [ -n "$whole_run" ]; then
    if sh "$HERE/display-driver.sh" >"$TEST_ROOT/display-driver.txt" 2>&1; then
        passed=$((passed + 1))
    else
        printf '  FAILED: display-driver.sh\n'
        grep FAILED "$TEST_ROOT/display-driver.txt"
        failed=$((failed + 1))
    fi
    if sh "$HERE/archives.sh" >"$TEST_ROOT/archives.txt" 2>&1; then
        passed=$((passed + 1))
    else
        printf '  FAILED: archives.sh\n'
        grep FAILED "$TEST_ROOT/archives.txt"
        failed=$((failed + 1))
    fi
fi
printf '%d passed, %d failed\n' "$passed" "$failed"
[ "$failed" -eq 0 ]
