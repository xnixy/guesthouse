#!/bin/sh
# Makes a pretend release in FAKE_RELEASES, and an install.sh that names it, the way a real release
# changes the one on main. Its programs are small shell scripts.
#
# Usage: make-release.sh REPO FAKE_RELEASES VERSION [PARTS]
# PARTS is any of `windows` and `linux`. The script for the release is left as install-VERSION.sh.
set -eu

repo=$1
releases=$2
version=$3
parts=${4:-}
name=guesthouse-$version-x86_64
tree=$releases/tree-$version/$name

mkdir -p "$tree/qemu/build" "$tree/qemu/pc-bios" "$tree/share" "$releases/v$version"
for program in guesthouse guesthouse-no-hdr guesthouse-daemon guesthouse-jail guesthouse-task guesthouse-share; do
    printf '#!/bin/sh\necho "%s %s"\n' "$program" "$version" >"$tree/$program"
done
printf '#!/bin/sh\necho "qemu %s"\n' "$version" >"$tree/qemu/build/qemu-system-x86_64"
echo bios >"$tree/qemu/pc-bios/bios.bin"
cat >"$tree/guesthouse-check" <<'CHECK'
#!/bin/sh
printf 'check\tok\tVirtualization\t/dev/kvm can be opened.\t\n'
[ -z "${FAKE_GTK:-}" ] || printf 'gtk\t%s\n' "$FAKE_GTK"
if [ -n "${FAKE_GPU_REFUSED:-}" ]; then
    printf 'check\tfailed\tHost driver branch\t%s\tGPU machines stay off until the branches match.\n' "$FAKE_GPU_REFUSED"
    printf 'gpu\tno\n'
else
    printf 'gpu\tyes\n'
fi
printf 'amd\t%s\n' "${FAKE_AMD:-no}"
CHECK
chmod +x "$tree"/guesthouse* "$tree/qemu/build/qemu-system-x86_64"
cp "$repo/crates/app/data/io.github.xnixy.Guesthouse.desktop" "$tree/share/"
cp -R "$repo/crates/app/data/icons" "$tree/share/"
cp "$repo/install.sh" "$repo/uninstall.sh" "$tree/"
printf 'adw_new_thing\ngtk_new_thing\n' >"$tree/needs-symbols"
for part in $parts; do
    case $part in
        windows) mkdir -p "$tree/guest" && echo script >"$tree/guest/setup.ps1" ;;
        linux) mkdir -p "$tree/linux/setup/seed" && echo seed >"$tree/linux/setup/seed/meta-data" ;;
    esac
done
printf 'version=%s\ncommit=test\nglibc=2.30\nparts=app %s\n' "$version" "$parts" >"$tree/RELEASE"

tar -C "$releases/tree-$version" -cJf "$releases/v$version/$name.tar.xz" "$name"
: >"$releases/v$version/guesthouse-$version-source.tar.xz"
payload=$(sha256sum "$releases/v$version/$name.tar.xz" | cut -d ' ' -f 1)
source=$(sha256sum "$releases/v$version/guesthouse-$version-source.tar.xz" | cut -d ' ' -f 1)
sed -e "s/^RELEASE_VERSION=.*/RELEASE_VERSION=\"$version\"/" \
    -e "s/^PAYLOAD_SHA256=.*/PAYLOAD_SHA256=\"$payload\"/" \
    -e "s/^SOURCE_SHA256=.*/SOURCE_SHA256=\"$source\"/" \
    "$repo/install.sh" >"$releases/install-$version.sh"
