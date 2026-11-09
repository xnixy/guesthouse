#!/bin/bash
# A whole install and uninstall with a real release's files, in a fake HOME. Only curl, sudo and chown
# are stand-ins: everything else is this PC's real tools and libraries. The rest of the filesystem is
# read-only, so nothing can reach the real HOME, and at the end HOME must be as it began.
#
# The root step puts QEMU's bridge helper in /usr/local/libexec/guesthouse. Here that's an empty folder of
# the test's own, mounted there in the sandbox only, and the step runs as the test's user. So the
# setuid bit is set but the owner stays the user, as chown only writes down what it was asked.
#
# Usage: tests/install/real-run.sh RELEASE_FOLDER SCRATCH_FOLDER
# RELEASE_FOLDER is build-release.sh's --out, and SCRATCH_FOLDER must not exist yet.
set -eu

HERE=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH='' cd -- "$HERE/../.." && pwd)
RELEASE=$1
T=$2
VM_SECONDS=8
FIRMWARE=/usr/share/edk2/ovmf/OVMF_CODE.fd

mkdir "$T"
H=$T/home
mkdir -p "$H" "$T/bin" "$T/runtime" "$T/releases" "$T/libexec"
ln -s "$HERE/stand-ins/curl" "$HERE/stand-ins/sudo" "$HERE/stand-ins/chown" "$T/bin/"
HELPER=$T/libexec/guesthouse/qemu-bridge-helper
: >"$T/log"

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO/Cargo.toml" | head -n 1)
mkdir "$T/releases/v$version"
cp "$RELEASE/guesthouse-$version-x86_64.tar.xz" "$RELEASE/guesthouse-$version-source.tar.xz" "$T/releases/v$version/"
payload=$(sed -n "s/  guesthouse-$version-x86_64.tar.xz$//p" "$RELEASE/SHA256SUMS")
source=$(sed -n "s/  guesthouse-$version-source.tar.xz$//p" "$RELEASE/SHA256SUMS")
sed -e "s/^RELEASE_VERSION=.*/RELEASE_VERSION=\"$version\"/" -e "s/^PAYLOAD_SHA256=.*/PAYLOAD_SHA256=\"$payload\"/" \
    -e "s/^SOURCE_SHA256=.*/SOURCE_SHA256=\"$source\"/" "$REPO/install.sh" >"$T/install.sh"

sandboxed() {
    env -i HOME="$H" PATH="$T/bin:/usr/bin" XDG_RUNTIME_DIR="$T/runtime" STAND_IN_LOG="$T/log" \
        FAKE_RELEASES="$T/releases" TERM=dumb \
        bwrap --ro-bind / / --dev-bind /dev /dev --bind "$T" "$T" --bind "$T/libexec" /usr/local/libexec "$@"
}

snapshot() {
    (cd "$H" && find . -printf '%y %m %p %l\n' | sort && find . -type f -exec sha256sum {} + | sort -k 2)
}

current=$H/.local/lib/guesthouse/current

snapshot >"$T/before"
printf '== HOME before: %s entries\n' "$(wc -l <"$T/before")"

sandboxed python3 -I "$HERE/tty.py" 'yes\n' sh "$T/install.sh"
printf '\n== installed, %s files, %s\n' "$(grep -c '^file' "$H/.local/lib/guesthouse/manifest")" "$(du -sh "$H" | cut -f 1)"
grep -E '^(format|version|mode)' "$H/.local/lib/guesthouse/manifest"
ls -la "$H/.local/lib/guesthouse"
cat "$H/.local/share/applications/io.github.xnixy.Guesthouse.desktop"

printf '\n== the bridge helper is in place\n'
[ -u "$HELPER" ] || { printf "it isn't there, or isn't setuid\n"; exit 1; }
cmp -s "$HELPER" "$current/qemu/build/qemu-bridge-helper" || { printf "it isn't the release's\n"; exit 1; }
want=$(sha256sum "$current/qemu/build/qemu-bridge-helper" | cut -d ' ' -f 1)
grep -q "^chown 0:0 /usr/local/libexec/guesthouse/qemu-bridge-helper.new$" "$T/log" ||
    { printf "root wasn't asked to own it\n"; exit 1; }
[ "$(awk -F '\t' '$1 == "root-file" { print $2 " " $3 }' "$H/.local/lib/guesthouse/manifest")" = \
    "$want /usr/local/libexec/guesthouse/qemu-bridge-helper" ] || { printf "the manifest doesn't list it\n"; exit 1; }
printf 'setuid, the release'"'"'s, %s, and listed\n' "$want"

printf '\n== the real programs start from the install\n'
sandboxed "$current/qemu/build/qemu-system-x86_64" -version | head -n 1
sandboxed env "GUESTHOUSE_PROGRAM=$current" "$current/guesthouse-check"
sandboxed env "GUESTHOUSE_PROGRAM=$current" "$current/guesthouse" --help | head -n 2

printf '\n== a plain VM with no GPU, on the installed QEMU, reaches its firmware\n'
qemu-img create -f qcow2 "$T/throwaway.qcow2" 64M >/dev/null
sandboxed timeout "$VM_SECONDS" "$current/qemu/build/qemu-system-x86_64" -L "$current/qemu/pc-bios" \
    -machine q35,accel=kvm -m 256 -display none -monitor none -serial file:"$T/serial.txt" \
    -drive if=pflash,format=raw,readonly=on,file="$FIRMWARE" -drive file="$T/throwaway.qcow2",if=virtio -nodefaults || true
printf 'the VM wrote %s bytes to its serial port\n' "$(wc -c <"$T/serial.txt")"

printf '\n== sudo was asked for: %s\n' "$(grep -c '^sudo' "$T/log" || true)"

sandboxed python3 -I "$HERE/tty.py" 'yes\n' sh "$current/uninstall.sh"
if [ -n "$(ls -A "$T/libexec")" ]; then
    printf '\nthe uninstall left this in /usr/local/libexec:\n'
    ls -lR "$T/libexec"
    exit 1
fi
printf '\n== the uninstall removed the bridge helper and its folder\n'
snapshot >"$T/after"
printf '\n== HOME after: %s entries\n' "$(wc -l <"$T/after")"
if cmp -s "$T/before" "$T/after"; then
    printf 'HOME is exactly as it began\n'
else
    diff "$T/before" "$T/after"
    exit 1
fi
