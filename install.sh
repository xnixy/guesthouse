#!/bin/sh
# Installs or updates Guesthouse for the user who runs it, under their own folders.
#
# Read this before you run it. It's meant to be read, and it does nothing until the last line. It checks
# this PC, downloads one release into a folder of its own and verifies it, then shows you everything it
# will do and every command that needs root, and waits for you to say yes.
#
# It never installs or changes a GPU driver, kernel arguments or services, and it never needs root for
# anything but packages and QEMU's bridge helper. --dry-run shows the plan and stops. Everything it puts on this PC is listed in
# ~/.local/lib/guesthouse/manifest, and uninstall.sh removes that and nothing else.
#
# The checksums below only prove that the download is the release this file names. They can't protect
# you from this file itself: if you don't trust it, read it first.
set -eu

# A release sets these three, last of all, once its files are up. Until the first one they're empty.
RELEASE_VERSION="0.5.0"
PAYLOAD_SHA256="4b0d53243513659234aeb83a6a4e05cf4f14ee07e46700a7a0fb0467623200b7"
SOURCE_SHA256="b865d9f435e47fc9bdbddd28b232f120c351588ef2c1a672339b828bc5500c43"

REPO_URL=https://github.com/xnixy/guesthouse
RELEASE_URL=$REPO_URL/releases/download
LATEST_SCRIPT_URL=https://raw.githubusercontent.com/xnixy/guesthouse/main/install.sh

APP_ID=io.github.xnixy.Guesthouse
ARCH=x86_64
MANIFEST_FORMAT=1
QEMU_PROGRAM=qemu/build/qemu-system-x86_64
# Bridged networking needs QEMU's bridge helper to run as root. A setuid program in your home folder
# could be swapped or fed a config you wrote, so root puts a copy where only root can write. That's
# the one thing this puts outside your home folder, and /usr/local is writable on immutable systems too.
BRIDGE_HELPER_SOURCE=qemu/build/qemu-bridge-helper
BRIDGE_HELPER_FOLDER=/usr/local/libexec/guesthouse
BRIDGE_HELPER=$BRIDGE_HELPER_FOLDER/qemu-bridge-helper
# The first GTK the app with HDR loads on.
HDR_GTK=4.16
FIRMWARE_DESCRIPTORS=/usr/share/qemu/firmware
# Room for the download, the unpacked files and the version being replaced.
ROOM_NEEDED_MIB=1024
CURL_SECONDS=20
# What --check-update exits with when there's something newer, so the app can tell it from a failure.
UPDATE_AVAILABLE_EXIT=10
TAB=$(printf '\t')
# QEMU's OpenGL and virglrenderer are the host's Mesa. virglrenderer's AMD native context calls
# libdrm_amdgpu's amdgpu_device_initialize2, which came in libdrm 2.4.121.
OPENGL_LIBRARIES="libepoxy.so.0 libgbm.so.1 libEGL.so.1 libdrm.so.2 libdrm_amdgpu.so.1"
NEWEST_LIBDRM_CALL=amdgpu_device_initialize2

DRY_RUN=no
UPDATE=no
CHECK_UPDATE=no
FROM_SOURCE=no
# The app's own process, when the app runs this: it's the one thing from this install that may be running.
FROM_APP=""

STAGING=""
CREATED_DIRS=""
KEEP_CREATED_DIRS=no
ROLLBACK=no
MOVED_ASIDE=""
VERSION_PLACED=no
OLD_LINK=""
LINK_FLIPPED=no
MANIFEST_BEFORE=""
PACKAGES_ADDED=""
HELPER_PLACED=no

say() {
    printf '%s\n' "$*"
}

warn() {
    printf 'install.sh: %s\n' "$*" >&2
}

die() {
    warn "$*"
    exit 1
}

usage() {
    cat <<'EOF'
Usage: install.sh [OPTION]

With nothing installed this installs the release it names, and with an older release installed it
updates to that one.

  --update        Bring an install to the latest release, whichever copy of this script you run
  --check-update  Say whether a newer release exists, and change nothing
  --from-app=PID  For the app, which is PID: it has asked already, so this doesn't ask, and root's
                  password is asked for in a window
  --dry-run       Check everything, download and verify, show the plan, and stop
  --from-source   Build the release's source on this PC, and install that
  --help          Show this
EOF
}

parse_arguments() {
    for argument in "$@"; do
        case $argument in
            --dry-run) DRY_RUN=yes ;;
            --update) UPDATE=yes ;;
            --check-update) CHECK_UPDATE=yes ;;
            --from-source) FROM_SOURCE=yes ;;
            --from-app=*) FROM_APP=${argument#--from-app=} ;;
            --help)
                usage
                exit 0
                ;;
            *)
                usage >&2
                exit 2
                ;;
        esac
    done
    case $FROM_APP in
        *[!0-9]*) die "--from-app takes the app's process number." ;;
    esac
}

# Everything goes in the user's own folders, and a root install would put it in root's.
refuse_root() {
    [ "$(id -u)" -ne 0 ] || die "don't run this as root. It asks for root itself, once, if it needs packages."
}

# The launcher's Exec line quotes the path, and these would need escaping there.
check_home() {
    [ -n "${HOME:-}" ] || die "HOME isn't set."
    case $HOME in
        /*) ;;
        *) die "HOME isn't an absolute path: $HOME" ;;
    esac
    case $HOME in
        *[\"\`\$\\%"$TAB"]* | *'
'*) die "your home folder's name has characters a launcher can't hold: $HOME" ;;
    esac
    [ -d "$HOME" ] || die "your home folder isn't there: $HOME"
}

# The app has the first of these written into it, as ~/.local/lib/guesthouse/current. See paths.rs.
set_paths() {
    LIB_ROOT=$HOME/.local/lib/guesthouse
    CURRENT_LINK=$LIB_ROOT/current
    MANIFEST=$LIB_ROOT/manifest
    case ${XDG_DATA_HOME:-} in
        /*) DATA_HOME=$XDG_DATA_HOME ;;
        *) DATA_HOME=$HOME/.local/share ;;
    esac
    LAUNCHER=$DATA_HOME/applications/$APP_ID.desktop
    ICON_FOLDER=$DATA_HOME/icons
}

os_release_field() {
    sed -n "s/^$1=//p" /etc/os-release 2>/dev/null | head -n 1 | tr -d "\"'"
}

# A derivative says what it's built on in ID_LIKE, and its packages are named the same way.
find_family() {
    FAMILY=unknown
    for word in $(os_release_field ID) $(os_release_field ID_LIKE); do
        case $word in
            fedora) FAMILY=fedora ;;
            debian | ubuntu) FAMILY=debian ;;
            arch) FAMILY=arch ;;
            opensuse* | suse | sles) FAMILY=suse ;;
            nixos) FAMILY=nixos ;;
            *) continue ;;
        esac
        return
    done
}

# Package tools still run on these, but into an image, and only a reboot brings the change in.
find_immutable() {
    IMMUTABLE=no
    LAYER_COMMAND=""
    if [ -e /run/ostree-booted ]; then
        IMMUTABLE=yes
        LAYER_COMMAND="sudo rpm-ostree install"
    elif command -v transactional-update >/dev/null 2>&1; then
        IMMUTABLE=yes
        LAYER_COMMAND="sudo transactional-update pkg install"
    elif command -v steamos-readonly >/dev/null 2>&1; then
        IMMUTABLE=yes
    fi
}

refuse_unsupported() {
    [ "$(uname -m)" = "$ARCH" ] ||
        die "Guesthouse only runs on $ARCH, and this PC is $(uname -m). KVM can't run these guests anywhere else."
    [ "$FAMILY" != nixos ] && [ ! -e /etc/NIXOS ] ||
        die "NixOS isn't supported: the release's programs expect a normal /lib and /usr, and this script can't declare anything in your configuration."
    if ldd --version 2>&1 | grep -qi musl; then
        die "musl isn't supported: the release is built against glibc, and the app needs it."
    fi
}

how_supported() {
    case $FAMILY in
        fedora) say "this is what I run it on" ;;
        debian | arch) say "should work, tested without a GPU" ;;
        suse) say "best effort, and the package names are untested" ;;
        *) say "I don't know this distro, so packages are up to you" ;;
    esac
}

cpu_has_virtualization() {
    grep -Eq '^flags.*\b(vmx|svm)\b' /proc/cpuinfo 2>/dev/null
}

kvm_usable() {
    [ -r /dev/kvm ] && [ -w /dev/kvm ]
}

# The fix is a group, and only when the group is the reason. Most systems open /dev/kvm to whoever's at
# the seat, so this is rare.
kvm_group_would_help() {
    [ -e /dev/kvm ] && ! kvm_usable || return 1
    grep -q '^kvm:' /etc/group 2>/dev/null || return 1
    case " $(id -Gn) " in
        *" kvm "*) return 1 ;;
    esac
}

report_host() {
    say "Checking this PC. Nothing here needs root or changes anything."
    say "  Distro: $(os_release_field PRETTY_NAME) ($(how_supported))"
    [ "$IMMUTABLE" = no ] || say "  Immutable: yes, so this only puts files in your home folder"
    if cpu_has_virtualization; then
        say "  CPU: has virtualization"
    else
        warn "this CPU doesn't show vmx or svm. Turn on SVM or VT-x in the firmware settings, or no VM will run."
    fi
    if kvm_usable; then
        say "  KVM: /dev/kvm can be opened"
    elif [ -e /dev/kvm ]; then
        warn "you can't open /dev/kvm, so VMs would crawl on the CPU alone."
    else
        warn "/dev/kvm isn't here, so VMs would crawl on the CPU alone."
    fi
}

require_tools() {
    for tool in curl tar xz sha256sum sed awk find mktemp df; do
        command -v "$tool" >/dev/null 2>&1 || die "this needs $tool, and it's not installed."
    done
}

# The real name of a folder: /home is a link on Silverblue, and /proc only ever shows real names.
real_folder() {
    (cd "$1" 2>/dev/null && pwd -P) || printf '%s' "$1"
}

# Processes started from this install: their program is in its folder, which is also how the app tells
# its own QEMU from another's. Every install of this user shares the runtime folder, so a socket there
# doesn't say whose VM it is. A process that won't say what it runs is judged by its first word.
list_running() {
    real_root=$(real_folder "$LIB_ROOT")
    for folder in /proc/[0-9]*; do
        [ "${folder#/proc/}" != "$FROM_APP" ] || continue
        program=$(readlink "$folder/exe" 2>/dev/null) ||
            program=$(tr '\0' '\n' 2>/dev/null <"$folder/cmdline" | head -n 1)
        case $program in
            "$real_root"/* | "$LIB_ROOT"/*) say "${folder#/proc/} ${program##*/}" ;;
        esac
    done
}

refuse_while_running() {
    running=$(list_running)
    [ -z "$running" ] || die "Guesthouse is running, so nothing changes yet. Shut its VMs down and close it first:
$running"
}

manifest_value() {
    [ -f "$MANIFEST" ] || return 0
    awk -F "$TAB" -v key="$1" '$1 == key { print $3; exit }' "$MANIFEST"
}

manifest_lists() {
    [ -f "$MANIFEST" ] && awk -F "$TAB" -v path="$1" '$3 == path { found = 1 } END { exit !found }' "$MANIFEST"
}

fetch() {
    curl --proto '=https' --tlsv1.2 -fL --retry 3 --connect-timeout "$CURL_SECONDS" "$@"
}

is_version() {
    printf '%s' "$1" | grep -Eq '^[0-9]+\.[0-9]+[0-9A-Za-z.+-]*$'
}

is_sha256() {
    printf '%s' "$1" | grep -Eq '^[0-9a-f]{64}$'
}

# Read as data, and never run: a --check-update must not run what it downloads.
script_constant() {
    sed -n "s/^$1=\"\(.*\)\"\$/\1/p" | head -n 1
}

# The script on main names the latest release, as a release sets it last of all. That also means an old
# copy of this file finds the right version and its hash without asking anything else.
read_latest_release() {
    text=$(fetch -s --max-time "$CURL_SECONDS" "$LATEST_SCRIPT_URL") || return 1
    RELEASE_VERSION=$(printf '%s\n' "$text" | script_constant RELEASE_VERSION)
    PAYLOAD_SHA256=$(printf '%s\n' "$text" | script_constant PAYLOAD_SHA256)
    SOURCE_SHA256=$(printf '%s\n' "$text" | script_constant SOURCE_SHA256)
    is_version "$RELEASE_VERSION" && is_sha256 "$PAYLOAD_SHA256" && is_sha256 "$SOURCE_SHA256"
}

newer_than() {
    [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n 1)" = "$1" ]
}

# One line for a program to read, and an exit code. Nothing here touches the install.
check_for_update() {
    installed=$(manifest_value version)
    if [ -z "$installed" ]; then
        say "not-installed"
        exit 1
    fi
    if ! read_latest_release 2>/dev/null; then
        say "unknown $installed"
        exit 1
    fi
    if newer_than "$RELEASE_VERSION" "$installed"; then
        say "update-available $installed $RELEASE_VERSION"
        exit "$UPDATE_AVAILABLE_EXIT"
    fi
    say "up-to-date $installed"
    exit 0
}

ensure_dir() {
    [ ! -d "$1" ] || return 0
    ensure_dir "$(dirname "$1")"
    mkdir "$1"
    CREATED_DIRS="$CREATED_DIRS$1
"
}

remove_created_dirs() {
    printf '%s' "$CREATED_DIRS" | sort -r | while IFS= read -r folder; do
        [ -z "$folder" ] || rmdir "$folder" 2>/dev/null || true
    done
}

# Staging is on the same disk as the install. /tmp is often RAM, can be noexec, and a move from it
# wouldn't be one step. A killed run leaves a folder of this name, and only this script makes those.
make_staging() {
    ensure_dir "$LIB_ROOT"
    for stale in "$LIB_ROOT"/.incoming.*; do
        [ ! -e "$stale" ] || rm -rf -- "${stale:?}"
    done
    STAGING=$(mktemp -d "$LIB_ROOT/.incoming.XXXXXX")
}

check_room() {
    free_kib=$(df -Pk "$LIB_ROOT" | awk 'NR == 2 { print $4 }')
    [ "$free_kib" -ge $((ROOM_NEEDED_MIB * 1024)) ] ||
        die "this needs ${ROOM_NEEDED_MIB} MiB free in your home folder, and there's $((free_kib / 1024)) MiB."
}

download_verified() {
    name=$1
    wanted=$2
    url=$RELEASE_URL/v$RELEASE_VERSION/$name
    say "Downloading $url"
    fetch -# -o "$STAGING/$name" "$url" || die "couldn't download $url"
    found=$(sha256sum "$STAGING/$name" | cut -d ' ' -f 1)
    [ "$found" = "$wanted" ] || die "$name isn't the file this script expects, so I'm stopping.
  expected $wanted
  got      $found"
    say "Verified $name"
}

# The toolchain on this PC matches this PC, which is the point of a source build. A container is the
# next best thing, but its glibc is the release's, so it can't help a system older than that.
build_from_source() {
    source_name=guesthouse-$RELEASE_VERSION-source
    download_verified "$source_name.tar.xz" "$SOURCE_SHA256"
    tar -xJf "$STAGING/$source_name.tar.xz" -C "$STAGING"
    bundle=$STAGING/$source_name
    if command -v cargo >/dev/null 2>&1 && command -v ninja >/dev/null 2>&1 && command -v meson >/dev/null 2>&1; then
        sh "$bundle/packaging/build-release.sh" --out "$STAGING/built" qemu app payload
    else
        command -v podman >/dev/null 2>&1 || command -v docker >/dev/null 2>&1 ||
            die "building needs cargo, ninja and meson, or podman or docker, and this PC has neither."
        sh "$bundle/packaging/build-in-images.sh" --out "$STAGING/built" qemu app payload
    fi
    mv "$STAGING/built/guesthouse-$RELEASE_VERSION-$ARCH.tar.xz" "$STAGING/"
}

get_payload() {
    payload=guesthouse-$RELEASE_VERSION-$ARCH.tar.xz
    if [ "$FROM_SOURCE" = yes ]; then
        build_from_source
    else
        download_verified "$payload" "$PAYLOAD_SHA256"
    fi
    tar -xJf "$STAGING/$payload" -C "$STAGING" --no-same-owner || die "$payload wouldn't unpack."
    TREE=$STAGING/guesthouse-$RELEASE_VERSION-$ARCH
    [ -f "$TREE/RELEASE" ] || die "$payload has no RELEASE file, so it's not a Guesthouse release."
    [ "$(release_field version)" = "$RELEASE_VERSION" ] ||
        die "$payload says it's version $(release_field version), not $RELEASE_VERSION."
}

release_field() {
    sed -n "s/^$1=//p" "$TREE/RELEASE" | head -n 1
}

# Without this, the failure would be a loader error the first time someone opens the app.
check_glibc() {
    needed=$(release_field glibc)
    [ -n "$needed" ] || return 0
    have=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{ print $2 }')
    if newer_than "$needed" "${have:-0}"; then
        die "this release needs glibc $needed, and this system has ${have:-an unknown one}. A newer distro release would do, or --from-source if you have the tools to build it."
    fi
}

# The release's own program does the looking, so what a GPU needs is a backend's business and not this
# script's. It needs nothing but glibc, and it's only ever asked questions.
ask_the_release() {
    HOST_REPORT=$STAGING/host-report
    GPU_ANSWER=no
    AMD_ANSWER=no
    if [ -x "$TREE/guesthouse-check" ] &&
        GUESTHOUSE_PROGRAM=$TREE "$TREE/guesthouse-check" >"$HOST_REPORT" 2>/dev/null; then
        GPU_ANSWER=$(awk -F "$TAB" '$1 == "gpu" { print $2 }' "$HOST_REPORT")
        AMD_ANSWER=$(awk -F "$TAB" '$1 == "amd" { print $2 }' "$HOST_REPORT")
    else
        : >"$HOST_REPORT"
    fi
}

print_host_report() {
    awk -F "$TAB" '$1 == "check" {
        print "  " $3 ": " $4
        if ($2 != "ok" && $5 != "") print "    " $5
    }' "$HOST_REPORT"
}

release_has_gpu_files() {
    [ -d "$TREE/guest" ] || [ -d "$TREE/linux" ]
}

# GPU_NOTE is what the plan says about it. An install that had the GPU keeps its files whatever the
# host says today, because machines depend on them, and a driver is something that gets fixed. The
# daemon's own check still judges every Start.
decide_mode() {
    previous=$(manifest_value mode)
    GPU_NOTE=""
    if ! release_has_gpu_files; then
        MODE=plain
        GPU_NOTE="This release has no GPU part in it, so it's plain VMs only."
    elif [ "$GPU_ANSWER" = yes ]; then
        MODE=gpu
    elif [ "$AMD_ANSWER" = yes ]; then
        MODE=gpu
        GPU_NOTE="The checks above say this PC can't run NVIDIA's GPU machines, but its AMD card can be a Linux guest's GPU."
    elif [ "$previous" = gpu ]; then
        MODE=gpu
        GPU_NOTE="The checks above say this PC can't run GPU machines right now. Often that's a driver update that moved to another branch: host and guest have to match. This update doesn't change that, and doesn't touch your driver. GPU machines won't start until it's fixed, your machines and disks stay as they are, and plain VMs still run."
    else
        MODE=plain
        GPU_NOTE="This PC can't run GPU machines, for the reasons above, so this installs plain VMs only. They need nothing from your GPU. If you fix that, run this again."
    fi
}

library_path() {
    PATH=$PATH:/sbin:/usr/sbin ldconfig -p 2>/dev/null |
        awk -v name="$1" '$1 == name && /x86-64/ { print $NF; exit }'
}

firmware_present() {
    grep -qs '"x86_64"' "$FIRMWARE_DESCRIPTORS"/*.json
}

need_present() {
    case $1 in
        gtk) [ -n "$(library_path libgtk-4.so.1)" ] ;;
        adwaita) [ -n "$(library_path libadwaita-1.so.0)" ] ;;
        pipewire) [ -n "$(library_path libpipewire-0.3.so.0)" ] ;;
        opengl) for library in $OPENGL_LIBRARIES; do [ -n "$(library_path "$library")" ] || return 1; done ;;
        cap-ng) [ -n "$(library_path libcap-ng.so.0)" ] ;;
        firmware) firmware_present ;;
        *) command -v "$1" >/dev/null 2>&1 ;;
    esac
}

# Debian's and Arch's come from their package indexes, and Fedora's from a real system. openSUSE's are
# from memory: it's best effort until someone tries them.
package_for() {
    case $FAMILY:$1 in
        fedora:gtk) say gtk4 ;;
        fedora:adwaita) say libadwaita ;;
        fedora:pipewire) say pipewire-libs ;;
        fedora:opengl) say libepoxy mesa-libgbm mesa-libEGL libdrm ;;
        fedora:qemu-img) say qemu-img ;;
        fedora:firmware) say edk2-ovmf ;;
        fedora:7z) say 7zip ;;
        fedora:genisoimage | fedora:isoinfo) say genisoimage ;;
        fedora:swtpm) say swtpm swtpm-tools ;;
        fedora:openssl) say openssl ;;
        fedora:ssh-keygen) say openssh ;;
        fedora:bwrap) say bubblewrap ;;
        fedora:gcc) say gcc ;;
        fedora:cap-ng) say libcap-ng ;;
        debian:gtk) say libgtk-4-1 ;;
        debian:adwaita) say libadwaita-1-0 ;;
        debian:pipewire) say libpipewire-0.3-0t64 ;;
        debian:opengl) say libepoxy0 libgbm1 libegl1 libdrm2 libdrm-amdgpu1 ;;
        debian:qemu-img) say qemu-utils ;;
        debian:firmware) say ovmf ;;
        debian:7z) say 7zip ;;
        debian:genisoimage | debian:isoinfo) say genisoimage ;;
        debian:swtpm) say swtpm swtpm-tools ;;
        debian:openssl) say openssl ;;
        debian:ssh-keygen) say openssh-client ;;
        debian:bwrap) say bubblewrap ;;
        debian:gcc) say gcc ;;
        debian:cap-ng) say libcap-ng0 ;;
        arch:gtk) say gtk4 ;;
        arch:adwaita) say libadwaita ;;
        arch:pipewire) say libpipewire ;;
        arch:opengl) say libepoxy mesa libglvnd libdrm ;;
        arch:qemu-img) say qemu-img ;;
        arch:firmware) say edk2-ovmf ;;
        arch:7z) say 7zip ;;
        arch:genisoimage | arch:isoinfo) say cdrtools ;;
        arch:swtpm) say swtpm ;;
        arch:openssl) say openssl ;;
        arch:ssh-keygen) say openssh ;;
        arch:bwrap) say bubblewrap ;;
        arch:gcc) say gcc ;;
        arch:cap-ng) say libcap-ng ;;
        suse:gtk) say libgtk-4-1 ;;
        suse:adwaita) say libadwaita-1-0 ;;
        suse:pipewire) say libpipewire-0_3-0 ;;
        suse:opengl) say libepoxy0 libgbm1 libEGL1 libdrm2 libdrm_amdgpu1 ;;
        suse:qemu-img) say qemu-img ;;
        suse:firmware) say qemu-ovmf-x86_64 ;;
        suse:7z) say 7zip ;;
        suse:genisoimage | suse:isoinfo) say genisoimage ;;
        suse:swtpm) say swtpm ;;
        suse:openssl) say openssl ;;
        suse:ssh-keygen) say openssh-clients ;;
        suse:bwrap) say bubblewrap ;;
        suse:gcc) say gcc ;;
        suse:cap-ng) say libcap-ng0 ;;
    esac
}

# Every machine needs the first six: QEMU loads OpenGL's libraries for an AMD card's GPU, whether or
# not a machine uses it. The bridge helper needs libcap-ng. The rest are what a GPU machine's install
# and its jail use, and setarch comes with util-linux, which every one of these systems has.
wanted_needs() {
    say gtk adwaita pipewire opengl qemu-img firmware
    [ ! -f "$TREE/$BRIDGE_HELPER_SOURCE" ] || say cap-ng
    [ "$MODE" = plain ] || say 7z genisoimage swtpm
    if [ "$MODE" = gpu ] && [ -d "$TREE/linux" ]; then
        say isoinfo openssl ssh-keygen bwrap gcc
    fi
}

find_missing() {
    MISSING_NEEDS=""
    PACKAGES=""
    for need in $(wanted_needs); do
        need_present "$need" && continue
        MISSING_NEEDS="$MISSING_NEEDS $need"
        for package in $(package_for "$need"); do
            case " $PACKAGES " in
                *" $package "*) ;;
                *) PACKAGES="$PACKAGES $package" ;;
            esac
        done
    done
    PACKAGES=${PACKAGES# }
    MISSING_NEEDS=${MISSING_NEEDS# }
}

install_command() {
    case $FAMILY in
        fedora) say "dnf install -y $1" ;;
        # --no-remove: yes to installing isn't yes to whatever apt decides to take away for it.
        debian) say "apt-get update && apt-get install -y --no-remove $1" ;;
        # Never -Sy: that's the partial upgrade Arch warns about.
        arch) say "pacman -S --needed --noconfirm $1" ;;
        suse) say "zypper --non-interactive install $1" ;;
    esac
}

# Whether root has to put the helper in place: it isn't there, isn't setuid, or isn't this release's.
helper_needs_work() {
    [ -f "$1" ] || return 1
    [ -u "$BRIDGE_HELPER" ] || return 0
    [ "$(hash_of "$1")" != "$(hash_of "$BRIDGE_HELPER")" ]
}

# Root checks the copy against the hash taken here, before it makes it setuid, as the source is in a
# folder you can write. A copy that fails goes.
helper_command() {
    source=$TREE/$BRIDGE_HELPER_SOURCE
    # shellcheck disable=SC2016 # $f and $n are for root's shell
    printf '{ f="%s"; n="$f/%s.new"; { mkdir -p "$f" && chmod 755 "$f" && cp "%s" "$n" && chown 0:0 "$n" && echo "%s  $n" | sha256sum -c --quiet && chmod 4755 "$n" && mv -f "$n" "$f/%s"; } || { rm -f "$n"; false; }; }' \
        "$BRIDGE_HELPER_FOLDER" "${BRIDGE_HELPER##*/}" "$source" "$(hash_of "$source")" "${BRIDGE_HELPER##*/}"
}

# What root does, as one string, so what's shown is exactly what runs. An immutable system gets no
# packages, but its /usr/local takes the helper all the same.
plan_root_work() {
    ROOT_COMMAND=""
    ADD_TO_KVM=no
    if [ "$IMMUTABLE" = no ]; then
        [ -z "$PACKAGES" ] || ROOT_COMMAND=$(install_command "$PACKAGES")
        if kvm_group_would_help; then
            ADD_TO_KVM=yes
            ROOT_COMMAND="${ROOT_COMMAND:+$ROOT_COMMAND && }usermod -aG kvm $(id -un)"
        fi
    fi
    if helper_needs_work "$TREE/$BRIDGE_HELPER_SOURCE"; then
        HELPER_PLACED=yes
        ROOT_COMMAND="${ROOT_COMMAND:+$ROOT_COMMAND && }$(helper_command)"
    fi
}

# A too-old library stops this. The app couldn't start, and the failure would be a loader error. The
# release lists what it calls, so this needs no table of versions.
symbols_missing_from() {
    library=$1
    wanted=$STAGING/symbols-wanted
    grep -E "$2" "$TREE/needs-symbols" | sort -u >"$wanted" || return 0
    LC_ALL=C grep -aowFf "$wanted" "$library" | LC_ALL=C sort -u >"$wanted.found" || true
    LC_ALL=C sort -u "$wanted" | LC_ALL=C comm -23 - "$wanted.found"
}

check_toolkit_library() {
    library=$(library_path "$1")
    [ -n "$library" ] || return 0
    lacking=$(symbols_missing_from "$library" "$2" | head -n 3 | tr '\n' ' ')
    [ -z "$lacking" ] ||
        die "the $3 on this system is too old for this release: it has no ${lacking}. It needs a newer distro release, and this script won't replace it."
}

# A release has an app with HDR and one without, as the first won't load on an older GTK. This is
# after packages, when GTK is here to ask about. With no answer, the one that runs anywhere is safest.
choose_app() {
    [ -f "$TREE/guesthouse-no-hdr" ] || return 0
    gtk=$(GUESTHOUSE_PROGRAM=$TREE "$TREE/guesthouse-check" 2>/dev/null | awk -F "$TAB" '$1 == "gtk" { print $2 }')
    if [ -n "$gtk" ] && ! newer_than "$HDR_GTK" "$gtk"; then
        rm -f "$TREE/guesthouse-no-hdr"
    else
        mv -f "$TREE/guesthouse-no-hdr" "$TREE/guesthouse"
    fi
}

check_toolkit() {
    [ -f "$TREE/needs-symbols" ] || return 0
    check_toolkit_library libgtk-4.so.1 '^(gtk|gdk|gsk)_' GTK
    check_toolkit_library libadwaita-1.so.0 '^adw_' libadwaita
}

# Nothing here replaces a file it didn't put there, because then uninstall.sh couldn't tell whose it is.
refuse_strangers() {
    for path in "$LAUNCHER" "$CURRENT_LINK" "$LIB_ROOT/$RELEASE_VERSION"; do
        if [ -e "$path" ] || [ -L "$path" ]; then
            manifest_lists "$path" ||
                die "$path is already there, and this script didn't put it there. Move it away and run this again."
        fi
    done
    (cd "$TREE/share/icons" && find . -type f) | while IFS= read -r icon; do
        path=$ICON_FOLDER/${icon#./}
        if [ -e "$path" ] && ! manifest_lists "$path"; then
            die "$path is already there, and this script didn't put it there. Move it away and run this again."
        fi
    done
}

# How the new version differs from the old, counted from the manifest and the download.
describe_changes() {
    installed=$1
    old_prefix=$LIB_ROOT/$installed/
    (cd "$TREE" && find . -type f -exec sha256sum {} +) | sed 's/^\([0-9a-f]*\)  \.\//\1 /' >"$STAGING/new-files"
    awk -F "$TAB" -v prefix="$old_prefix" '$1 == "file" && index($3, prefix) == 1 {
        print $2, substr($3, length(prefix) + 1)
    }' "$MANIFEST" >"$STAGING/old-files"
    awk 'NR == FNR { old[$2] = $1; next }
        { if (!($2 in old)) added++; else if (old[$2] != $1) changed++; seen[$2] = 1 }
        END {
            for (name in old) if (!(name in seen)) removed++
            printf "%d changed, %d new, %d gone", changed, added, removed
        }' "$STAGING/old-files" "$STAGING/new-files"
}

print_plan() {
    installed=$(manifest_value version)
    say ""
    if [ -z "$installed" ]; then
        say "The plan: install Guesthouse $RELEASE_VERSION."
    elif [ "$installed" = "$RELEASE_VERSION" ]; then
        say "The plan: put Guesthouse $RELEASE_VERSION in again."
    else
        say "The plan: update Guesthouse from $installed to $RELEASE_VERSION."
        say "  Files: $(describe_changes "$installed")"
        say "  What's new: $REPO_URL/releases/tag/v$RELEASE_VERSION"
        say "  The old version stays until the new one is in place. Your machines, disks and settings stay as they are."
    fi
    say "  Program: $LIB_ROOT/$RELEASE_VERSION, reached through $CURRENT_LINK"
    say "  Launcher: $LAUNCHER"
    say "  Icons: $ICON_FOLDER/hicolor"
    say "  List of what's installed: $MANIFEST"
    if [ "$MODE" = gpu ]; then
        say "  VMs: GPU machines and plain VMs"
    else
        say "  VMs: plain VMs only"
    fi
    [ -z "$GPU_NOTE" ] || say "  $GPU_NOTE"
    print_root_plan
}

print_root_plan() {
    say ""
    if [ "$IMMUTABLE" = yes ] && [ -n "$PACKAGES" ]; then
        say "This system is immutable, so I won't add packages to it. You'll need these, and a reboot:"
        say "  ${LAYER_COMMAND:-(however your system adds packages)} $PACKAGES"
    elif [ -n "$MISSING_NEEDS" ] && [ "$FAMILY" = unknown ]; then
        say "These are missing, and I don't know your distro's names for them: $MISSING_NEEDS"
    fi
    if [ -n "$ROOT_COMMAND" ]; then
        say "This is everything that runs as root:"
        say "  sudo sh -c '$ROOT_COMMAND'"
    elif [ "$IMMUTABLE" = no ] || [ -z "$PACKAGES" ]; then
        say "Nothing here needs root."
    fi
    [ "$HELPER_PLACED" = no ] ||
        say "That puts QEMU's bridge helper in $BRIDGE_HELPER_FOLDER, setuid root, for bridged networking."
    [ -z "$ROOT_COMMAND" ] || [ "$ADD_TO_KVM" = no ] ||
        say "You'd join the kvm group to open /dev/kvm, and that takes a new login."
}

# Piped into a shell, stdin is this script, so the answer has to come from the terminal itself.
confirm() {
    [ -z "$FROM_APP" ] || return 0
    (: </dev/tty) 2>/dev/null || die "I need a terminal to ask you, so nothing was installed. Download this script and run it in one."
    printf 'Go ahead? Type yes: ' >/dev/tty
    IFS= read -r answer </dev/tty || answer=""
    case $answer in
        y | Y | yes | Yes | YES) ;;
        *) die "stopped. Nothing was changed." ;;
    esac
}

run_root_work() {
    [ -n "$ROOT_COMMAND" ] || return 0
    if [ -n "$FROM_APP" ]; then
        run_root_work_in_a_window
        return
    fi
    command -v sudo >/dev/null 2>&1 ||
        die "this needs sudo, and it's not here. Run this as root yourself, then run me again: $ROOT_COMMAND"
    sudo sh -c "$ROOT_COMMAND" </dev/tty || die "that command failed, so I'm stopping. Nothing of Guesthouse's was installed."
    [ "$IMMUTABLE" = yes ] || PACKAGES_ADDED=$PACKAGES
}

# The app has no terminal for sudo to ask in, so pkexec asks in a window. It shows the command too.
run_root_work_in_a_window() {
    command -v pkexec >/dev/null 2>&1 ||
        die "this update needs root, and there's no pkexec to ask for it in a window. Run install.sh --update in a terminal."
    pkexec sh -c "$ROOT_COMMAND" </dev/null || die "root's part didn't run, so I'm stopping. Nothing of Guesthouse's was changed."
    [ "$IMMUTABLE" = yes ] || PACKAGES_ADDED=$PACKAGES
}

# A libdrm older than virglrenderer needs would only show when QEMU couldn't start. With every symbol
# bound at once, its loader says what's missing now, and so it does for any other library.
check_qemu_loads() {
    qemu=$TREE/$QEMU_PROGRAM
    [ -x "$qemu" ] || return 0
    libdrm=$(library_path libdrm_amdgpu.so.1)
    if [ -n "$libdrm" ] && ! LC_ALL=C grep -aqF "$NEWEST_LIBDRM_CALL" "$libdrm"; then
        die "libdrm on this system is older than 2.4.121, which this release's QEMU needs for AMD cards and loads for every VM. Update your system, or move to a newer distro release, and run this again."
    fi
    said=$(LD_BIND_NOW=1 "$qemu" --version 2>&1) ||
        die "this release's QEMU can't start on this system: $(say "$said" | head -n 2 | tr '\n' ' ')"
}

# What's still missing can't be fixed here, and on a system with no packages to install it's up to you.
check_after_root() {
    choose_app
    check_toolkit
    [ "$IMMUTABLE" = no ] || return 0
    find_missing
    [ -z "$MISSING_NEEDS" ] || [ "$FAMILY" = unknown ] ||
        die "these are still missing after installing packages: $MISSING_NEEDS"
    [ -z "$MISSING_NEEDS" ] || warn "these are missing, so the app may not work until you add them: $MISSING_NEEDS"
}

# The app is found through `current` from every launch, so a running VM and a new app agree on QEMU's
# path. That also tells the app where its QEMU is when the guest files that mark the folder aren't there.
make_launcher() {
    exec_line="Exec=env \"GUESTHOUSE_PROGRAM=$CURRENT_LINK\" \"$CURRENT_LINK/guesthouse\""
    awk -v line="$exec_line" '/^Exec=/ { print line; next } { print }' \
        "$TREE/share/$APP_ID.desktop" >"$STAGING/launcher"
}

# A plain install leaves these out, as the app takes them to mean GPU machines can run.
drop_gpu_files() {
    [ "$MODE" = plain ] || return 0
    for part in guest linux; do
        [ ! -d "$TREE/$part" ] || rm -rf -- "${TREE:?}/${part:?}"
    done
}

manifest_line() {
    printf '%s\t%s\t%s\n' "$1" "$2" "$3"
}

hash_of() {
    sha256sum "$1" | cut -d ' ' -f 1
}

# Lines for the files of the new version, as they'll be once they're in place.
list_version_files() {
    target=$LIB_ROOT/$RELEASE_VERSION
    manifest_line dir - "$target"
    (cd "$TREE" && find . -mindepth 1 -type d) | while IFS= read -r name; do
        manifest_line dir - "$target/${name#./}"
    done
    (cd "$TREE" && find . -type f -exec sha256sum {} +) | while read -r hash name; do
        manifest_line file "$hash" "$target/${name#./}"
    done
    (cd "$TREE" && find . -type l) | while IFS= read -r name; do
        manifest_line link "$(readlink "$TREE/$name")" "$target/${name#./}"
    done
}

list_desktop_files() {
    manifest_line file "$(hash_of "$STAGING/launcher")" "$LAUNCHER"
    (cd "$TREE/share/icons" && find . -type f) | while IFS= read -r icon; do
        manifest_line file "$(hash_of "$TREE/share/icons/$icon")" "$ICON_FOLDER/${icon#./}"
    done
}

list_created_dirs() {
    printf '%s' "$CREATED_DIRS" | while IFS= read -r folder; do
        [ -z "$folder" ] || manifest_line dir - "$folder"
    done
}

list_root_work() {
    for package in $PACKAGES_ADDED; do
        manifest_line package - "$package"
    done
    [ "$ADD_TO_KVM" = no ] || manifest_line group - kvm
    [ "$HELPER_PLACED" = no ] || manifest_line root-file "$(hash_of "$TREE/$BRIDGE_HELPER_SOURCE")" "$BRIDGE_HELPER"
}

write_manifest_header() {
    manifest_line format "$MANIFEST_FORMAT" -
    manifest_line version - "$1"
    manifest_line mode - "$2"
    manifest_line data-home - "$DATA_HOME"
}

write_file_atomically() {
    cat >"$1.new"
    mv -f "$1.new" "$1"
}

# Everything old and new, before anything moves, so that uninstall.sh can find it all whatever happens
# from here on. A path can be listed twice for now, with both hashes.
write_manifest_ahead() {
    MANIFEST_BEFORE=$STAGING/manifest.before
    [ ! -f "$MANIFEST" ] || cp "$MANIFEST" "$MANIFEST_BEFORE"
    NEW_ENTRIES=$STAGING/new-entries
    {
        list_version_files
        list_desktop_files
        manifest_line link "$RELEASE_VERSION" "$CURRENT_LINK"
        list_root_work
    } >"$NEW_ENTRIES"
    {
        if [ -f "$MANIFEST" ]; then
            cat "$MANIFEST"
        else
            write_manifest_header "" ""
        fi
        cat "$NEW_ENTRIES"
        list_created_dirs
    } | write_file_atomically "$MANIFEST"
}

# The last line for a path wins. The old version's own files stay listed until they're gone.
write_final_manifest() {
    {
        write_manifest_header "$RELEASE_VERSION" "$MODE"
        awk -F "$TAB" '$1 != "format" && $1 != "version" && $1 != "mode" && $1 != "data-home" {
            key = $1 FS $3
            if (!(key in line)) order[++count] = key
            line[key] = $0
        }
        END { for (i = 1; i <= count; i++) print line[order[i]] }' "$MANIFEST"
    } | write_file_atomically "$MANIFEST"
}

place_version() {
    target=$LIB_ROOT/$RELEASE_VERSION
    if [ -d "$target" ]; then
        MOVED_ASIDE=$STAGING/replaced
        mv "$target" "$MOVED_ASIDE"
    fi
    mv "$TREE" "$target"
    VERSION_PLACED=yes
}

# A copy of what's there goes in staging first, so a failure can put it back.
place_file() {
    source=$1
    path=$2
    backup=
    if [ -f "$path" ]; then
        [ "$(hash_of "$source")" != "$(hash_of "$path")" ] || return 0
        backup=$STAGING/backup.$(hash_of "$path")
        cp "$path" "$backup"
    fi
    printf '%s\t%s\n' "$path" "$backup" >>"$STAGING/placed"
    cp "$source" "$path.new"
    mv -f "$path.new" "$path"
}

place_desktop_files() {
    target=$LIB_ROOT/$RELEASE_VERSION
    place_file "$STAGING/launcher" "$LAUNCHER"
    (cd "$target/share/icons" && find . -type f) | while IFS= read -r icon; do
        place_file "$target/share/icons/$icon" "$ICON_FOLDER/${icon#./}" || exit 1
    done
}

# GNOME looks again only when the theme's folder changes, and an icon cache that's there already would
# hide the new icons. A cache that isn't there is left that way, as it's not this script's to make.
refresh_icons() {
    theme=$ICON_FOLDER/hicolor
    [ -d "$theme" ] || return 0
    touch "$theme" 2>/dev/null || true
    [ -f "$theme/icon-theme.cache" ] || return 0
    for tool in gtk-update-icon-cache gtk4-update-icon-cache; do
        if command -v "$tool" >/dev/null 2>&1; then
            "$tool" -q -t -f "$theme" >/dev/null 2>&1 || true
            return 0
        fi
    done
}

# Until this one step, the old version is what runs.
flip_current() {
    OLD_LINK=$(readlink "$CURRENT_LINK" 2>/dev/null || true)
    ln -s "$RELEASE_VERSION" "$CURRENT_LINK.new"
    mv -Tf "$CURRENT_LINK.new" "$CURRENT_LINK"
    LINK_FLIPPED=yes
}

restore_placed_files() {
    [ -f "$STAGING/placed" ] || return 0
    while IFS="$TAB" read -r path backup; do
        rm -f -- "$path.new"
        if [ -n "$backup" ]; then
            mv -f "$backup" "$path"
        else
            rm -f -- "$path"
        fi
    done <"$STAGING/placed"
}

restore_link() {
    rm -f -- "$CURRENT_LINK.new"
    [ "$LINK_FLIPPED" = yes ] || return 0
    if [ -n "$OLD_LINK" ]; then
        ln -s "$OLD_LINK" "$CURRENT_LINK.new"
        mv -Tf "$CURRENT_LINK.new" "$CURRENT_LINK"
    else
        rm -f -- "$CURRENT_LINK"
    fi
}

restore_version() {
    target=$LIB_ROOT/$RELEASE_VERSION
    [ "$VERSION_PLACED" = no ] || rm -rf -- "${target:?}"
    [ -z "$MOVED_ASIDE" ] || [ ! -d "$MOVED_ASIDE" ] || mv "$MOVED_ASIDE" "$target"
}

restore_manifest() {
    rm -f -- "$MANIFEST.new"
    if [ -n "$MANIFEST_BEFORE" ] && [ -f "$MANIFEST_BEFORE" ]; then
        mv -f "$MANIFEST_BEFORE" "$MANIFEST"
    else
        rm -f -- "$MANIFEST"
    fi
}

# Everything up to the new manifest can be undone, and this leaves what was there before.
roll_back() {
    warn "that failed part-way, so I'm putting everything back as it was."
    # One step failing here is no reason to give up on the others.
    set +e
    restore_link
    restore_placed_files
    refresh_icons
    restore_version
    restore_manifest
}

on_exit() {
    status=$?
    trap - EXIT
    [ "$ROLLBACK" = no ] || [ "$status" -eq 0 ] || roll_back
    [ -z "$STAGING" ] || rm -rf -- "${STAGING:?}"
    [ "$KEEP_CREATED_DIRS" = yes ] || remove_created_dirs
    exit "$status"
}

# Every folder exists first, so that the manifest can list the ones made for this.
make_desktop_dirs() {
    ensure_dir "$(dirname "$LAUNCHER")"
    for icon in $(cd "$TREE/share/icons" && find . -type f); do
        ensure_dir "$(dirname "$ICON_FOLDER/${icon#./}")"
    done
}

install_files() {
    make_launcher
    make_desktop_dirs
    ROLLBACK=yes
    write_manifest_ahead
    place_version
    place_desktop_files
    refresh_icons
    flip_current
    write_final_manifest
    ROLLBACK=no
    KEEP_CREATED_DIRS=yes
}

# Whether the manifest lists anything of a version other than this one. A first install has none.
old_versions_listed() {
    awk -F "$TAB" -v root="$LIB_ROOT/" -v keep="$LIB_ROOT/$RELEASE_VERSION/" '
        ($1 == "file" || $1 == "dir" || $1 == "link") && index($3, root) == 1 &&
        index($3 "/", keep) != 1 && $3 != root "current" { found = 1 }
        END { exit !found }' "$MANIFEST"
}

# It can only fail to tidy up here, and then the new version still works and the old one is still listed.
remove_old_versions() {
    old_versions_listed || return 0
    sh "$CURRENT_LINK/uninstall.sh" --old-versions ${FROM_APP:+"--from-app=$FROM_APP"} ||
        warn "the old version couldn't be removed. It's harmless, and uninstall.sh knows about it."
}

say_done() {
    say ""
    say "Guesthouse $RELEASE_VERSION is installed. It's in your apps, or run it with:"
    say "  env \"GUESTHOUSE_PROGRAM=$CURRENT_LINK\" \"$CURRENT_LINK/guesthouse\""
    say "To remove it: sh \"$CURRENT_LINK/uninstall.sh\""
    [ "$ADD_TO_KVM" = no ] || say "Log out and in again, so that you're in the kvm group."
    [ "$IMMUTABLE" = no ] || [ -z "$PACKAGES" ] || say "Add the packages listed above before you open it."
}

# Which release this run is about, and whether there's anything to do.
choose_release() {
    installed=$(manifest_value version)
    if [ "$UPDATE" = yes ]; then
        [ -n "$installed" ] || die "there's nothing to update. Run this without --update to install."
        read_latest_release || die "couldn't find out the latest release from $LATEST_SCRIPT_URL"
    fi
    [ -n "$RELEASE_VERSION" ] || die "there's no release yet, so there's nothing to install."
    if [ -n "$installed" ] && newer_than "$installed" "$RELEASE_VERSION"; then
        die "you have $installed, which is newer than $RELEASE_VERSION. I won't go backwards."
    fi
    # A plain install of this version goes on, as the GPU may be able to run now, and so does one whose
    # bridge helper has gone wrong.
    if [ "$installed" = "$RELEASE_VERSION" ] && { [ "$UPDATE" = yes ] || [ "$(manifest_value mode)" = gpu ]; } &&
        ! helper_needs_work "$CURRENT_LINK/$BRIDGE_HELPER_SOURCE"; then
        say "Guesthouse $installed is the latest release."
        exit 0
    fi
}

stop_if_nothing_changes() {
    if [ "$(manifest_value version)" = "$RELEASE_VERSION" ] && [ "$(manifest_value mode)" = "$MODE" ] &&
        ! helper_needs_work "$TREE/$BRIDGE_HELPER_SOURCE"; then
        say "Guesthouse $RELEASE_VERSION is the latest release, and it's installed the way this PC can run it."
        exit 0
    fi
}

main() {
    parse_arguments "$@"
    refuse_root
    check_home
    set_paths
    find_family
    find_immutable
    refuse_unsupported
    if [ "$CHECK_UPDATE" = yes ]; then
        check_for_update
    fi
    report_host
    require_tools
    choose_release
    [ -z "$(manifest_value version)" ] || refuse_while_running

    trap on_exit EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM

    make_staging
    check_room
    get_payload
    check_glibc
    ask_the_release
    print_host_report
    decide_mode
    stop_if_nothing_changes
    drop_gpu_files
    find_missing
    plan_root_work
    check_toolkit
    refuse_strangers
    print_plan
    if [ "$DRY_RUN" = yes ]; then
        say ""
        say "That was a dry run, so nothing was changed."
        return
    fi
    confirm
    run_root_work
    check_after_root
    check_qemu_loads
    install_files
    remove_old_versions
    say_done
}

# In braces, so that a download cut off anywhere in this line, even right after `main`, is a syntax error.
{ main "$@"; }
