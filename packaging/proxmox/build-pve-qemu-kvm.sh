#!/bin/sh
# Builds Proxmox VE's QEMU package, pve-qemu-kvm, from Proxmox's own source with the Windows guest's part
# of qemu/qemu-gpupv.patch on top. Proxmox starts every VM with the /usr/bin/qemu-system-x86_64 of that
# package, so a Proxmox VM only gets the GPU-PV devices if this package has them. docs/proxmox.md says
# how the package is used, and what has run with it.
#
# Usage: packaging/proxmox/build-pve-qemu-kvm.sh [--out FOLDER] [--suffix SUFFIX] REF
#        packaging/proxmox/build-pve-qemu-kvm.sh [--out FOLDER] --patches-only
#
# REF is what to build of Proxmox's pve-qemu.git: a version such as 11.1.3-4, which is the commit called
# "bump version to 11.1.3-4", or a commit or a branch, or the word newest, which is the version of
# pve-qemu-kvm that Proxmox's pve-no-subscription repository has now. SUFFIX goes after Proxmox's
# version, +gpupv1 if it isn't given, so 11.1.3-4 is built as 11.1.3-4+gpupv1. That sorts above Proxmox's
# 11.1.3-4 and below its next release, which is why the package has to be held.
#
# It needs podman or docker and no root on this PC. The build runs in a Debian 13 container, as Proxmox
# VE 9 is Debian 13, with the build's dependencies from Proxmox's pve-no-subscription repository. The
# packages and SHA256SUMS end up in FOLDER, build/proxmox if it isn't given. It publishes nothing.
# --patches-only writes the patches to FOLDER/patches and stops, with no container.
set -eu

PVE_QEMU_URL=${PVE_QEMU_URL:-https://git.proxmox.com/git/pve-qemu.git}
PVE_REPOSITORY='http://download.proxmox.com/debian/pve trixie pve-no-subscription'
# The key Proxmox signs trixie's repositories with. The hash is the one Proxmox's install guide gives.
KEYRING_URL=https://enterprise.proxmox.com/debian/proxmox-archive-keyring-trixie.gpg
KEYRING_SHA256=136673be77aba35dcce385b28737689ad64fd785a797e57897589aed08db6e45
KEYRING=/usr/share/keyrings/proxmox-archive-keyring.gpg
SUFFIX=+gpupv1
# The devices a GPU-PV VM's args: name. The package's QEMU must know each, or the VM won't start on it.
DEVICES='vmbus-bridge hv-timesync gpupv-global gpupv-vgpu gpupv-render'

REPO=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
IMAGE=${IMAGE:-$(sh "$REPO/packaging/images/image-of.sh" debian)}
OUT=$REPO/build/proxmox

say() {
    printf '%s\n' "$*"
}

die() {
    printf 'build-pve-qemu-kvm: %s\n' "$*" >&2
    exit 1
}

# A Windows guest needs only the commits that touch hw/hyperv: the GPU-PV channels, the render device,
# the clock's integration service, and the bridge's MMIO window, which needs one ACPI change as well.
# The rest of the patch is for Linux guests and the app's viewer, and Proxmox needs none of it. A commit
# that touches hw/hyperv and anything else stops the build, so a new one is looked at before it's taken.
extract_patches() {
    rm -rf "${1:?}"
    mkdir -p "$1"
    awk -v dir="$1" '
        function flush(   i, keep, other, name) {
            if (n == 0)
                return
            keep = 0
            other = ""
            for (i = 1; i <= files; i++) {
                if (file[i] ~ /^(include\/)?hw\/hyperv\//)
                    keep = 1
                else if (file[i] != "hw/i386/acpi-build.c")
                    other = other " " file[i]
            }
            if (keep && other != "") {
                printf "build-pve-qemu-kvm: %s also changes%s\n", subject, other > "/dev/stderr"
                failed = 1
            }
            if (keep && !failed) {
                # git format-patch signs each commit off with its version. It is not part of the diff.
                while (n > 0 && line[n] == "")
                    n--
                if (n > 1 && line[n - 1] == "-- " && line[n] ~ /^[0-9][0-9.]*$/)
                    n -= 2
                name = sprintf("%s/%04d-hyperv.patch", dir, ++kept)
                for (i = 1; i <= n; i++)
                    print line[i] > name
                close(name)
                printf "%04d-hyperv.patch  %s\n", kept, subject
            }
            n = 0
            files = 0
        }
        /^From [0-9a-f]+ Mon Sep 17 00:00:00 2001$/ { flush() }
        { line[++n] = $0 }
        /^Subject: / { subject = $0; sub(/^Subject: (\[PATCH[^]]*\] )?/, "", subject) }
        /^diff --git a\// { file[++files] = substr($3, 3) }
        END {
            flush()
            if (kept == 0 && !failed) {
                print "build-pve-qemu-kvm: no commit in the patch touches hw/hyperv" > "/dev/stderr"
                failed = 1
            }
            exit failed
        }
    ' "$REPO/qemu/qemu-gpupv.patch"
}

find_engine() {
    ENGINE=$(command -v podman || command -v docker) || die "this needs podman or docker."
    # Podman without root maps the container's root to the caller already, and Docker's is root here.
    case ${ENGINE##*/} in
        docker) OWNER=$(id -u):$(id -g) ;;
        *) OWNER= ;;
    esac
}

# The rest runs in the container, as its root.

resolve_ref() {
    case $1 in
        *.*-*)
            git log --format='%H %s' origin/master |
                awk -v want="bump version to $1" '{ h = $1; sub(/^[^ ]+ /, "") } $0 == want { print h; exit }'
            ;;
        *) git rev-parse --verify --quiet "$1^{commit}" || git rev-parse --verify --quiet "origin/$1^{commit}" ;;
    esac
}

# Docker leaves what root made in the container to root here, a failed build's files too.
hand_back() {
    [ -z "${OWNER:-}" ] || chown -R "$OWNER" /build
}

# Unpacks the package and asks its QEMU for the devices it has, and for the bridge's MMIO window, which
# Proxmox's own vmbus-bridge hasn't. The libraries it needs are the build's, so they're here. QEMU
# ends with 0 when asked for a device it doesn't know, so its list is read instead.
check_devices() {
    rm -rf /tmp/gpupv-check
    dpkg-deb -x "$1" /tmp/gpupv-check
    qemu=/tmp/gpupv-check/usr/bin/qemu-system-x86_64
    "$qemu" --version | head -n 1 | grep . || die "the package's QEMU doesn't start here."
    list=$("$qemu" -device help)
    for device in $DEVICES; do
        say "$list" | grep -F "name \"$device\"" >/dev/null || die "the package's QEMU has no $device device."
    done
    "$qemu" -device vmbus-bridge,help | grep mmio-base >/dev/null ||
        die "the package's vmbus-bridge has no mmio-base."
    say "its QEMU has $DEVICES"
}

inside() {
    ref=$1
    suffix=$2
    trap hand_back EXIT
    export DEBIAN_FRONTEND=noninteractive
    export DEBFULLNAME="${DEBFULLNAME:-Local build}" DEBEMAIL="${DEBEMAIL:-local@build.invalid}"
    # Behind some proxies apt's pipelined downloads come back mixed up, as "Hash Sum mismatch". One at a
    # time costs a few seconds.
    echo 'Acquire::http::Pipeline-Depth "0";' > /etc/apt/apt.conf.d/99no-pipelining
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends git build-essential devscripts equivs ca-certificates \
        curl lintian fakeroot meson ninja-build python3-venv >/dev/null
    curl -fsSLo "$KEYRING" "$KEYRING_URL"
    printf '%s  %s\n' "$KEYRING_SHA256" "$KEYRING" | sha256sum -c --quiet ||
        die "Proxmox's archive key isn't the one expected."
    say "deb [signed-by=$KEYRING] $PVE_REPOSITORY" >/etc/apt/sources.list.d/pve.list
    apt-get update -qq
    if [ "$ref" = newest ]; then
        ref=$(apt-cache policy pve-qemu-kvm | awk '$1 == "Candidate:" { print $2 }')
        case $ref in
            [0-9]*.*-*) ;;
            *) die "Proxmox's repository names no pve-qemu-kvm to build." ;;
        esac
        say "the newest pve-qemu-kvm in Proxmox's repository is $ref"
    fi

    # A fresh clone each time, so nothing of an earlier build, its patches or its submodule, is left in it.
    cd /build
    rm -rf pve-qemu pve-qemu-kvm*.deb SHA256SUMS
    git clone -q "$PVE_QEMU_URL" pve-qemu
    cd pve-qemu
    commit=$(resolve_ref "$ref") || commit=
    [ -n "$commit" ] || die "pve-qemu.git has nothing called $ref."
    git checkout -q "$commit"
    git log -1 --format='pve-qemu %H %s'
    version=$(dpkg-parsechangelog -S Version)$suffix

    mk-build-deps -i -r -t 'apt-get -y -qq --no-install-recommends' debian/control >/dev/null
    say "fetching QEMU's source and its subprojects"
    make submodule </dev/null >/build/submodule.log 2>&1 || {
        tail -n 20 /build/submodule.log
        die "fetching the source failed, and /build/submodule.log has the rest."
    }

    # Proxmox's patches are a quilt series, applied in order, and these go last.
    mkdir debian/patches/gpupv
    for patch in /build/patches/*.patch; do
        cp "$patch" debian/patches/gpupv/
        say "gpupv/${patch##*/}" >>debian/patches/series
    done
    dch -v "$version" "Add the GPU-PV devices for Windows guests, the hw/hyperv commits of Guesthouse's qemu-gpupv.patch."
    head -n 1 debian/changelog

    say "building $version, which takes a while"
    make deb </dev/null >/build/build.log 2>&1 || {
        tail -n 30 /build/build.log
        die "the build failed, and /build/build.log has the rest."
    }
    cp "pve-qemu-kvm_${version}_amd64.deb" "pve-qemu-kvm-dbgsym_${version}_amd64.deb" /build/
    cd /build
    check_devices "pve-qemu-kvm_${version}_amd64.deb"
    sha256sum pve-qemu-kvm*.deb >SHA256SUMS
    cat SHA256SUMS
}

main() {
    patches_only=
    while [ $# -gt 0 ]; do
        case $1 in
            --out) OUT=$2 && shift 2 ;;
            --suffix) SUFFIX=$2 && shift 2 ;;
            --patches-only) patches_only=1 && shift ;;
            --inside)
                # Not in a list with && or ||, where the shell would ignore set -e inside it.
                shift
                inside "$@"
                return
                ;;
            -*) die "there's no option $1." ;;
            *) break ;;
        esac
    done
    case $SUFFIX in
        +[a-z0-9]*) ;;
        *) die "the suffix has to start with + and a letter or a digit, as $SUFFIX doesn't." ;;
    esac
    mkdir -p "$OUT"
    OUT=$(CDPATH='' cd -- "$OUT" && pwd)
    extract_patches "$OUT/patches"
    [ -z "$patches_only" ] || return 0
    [ $# -eq 1 ] || die "name what to build of pve-qemu.git, such as 11.1.3-4, or newest."
    find_engine
    "$ENGINE" run --rm --security-opt label=disable -e "OWNER=$OWNER" -e DEBFULLNAME -e DEBEMAIL -e DEB_BUILD_OPTIONS \
        -v "$OUT:/build" -v "$REPO/packaging/proxmox:/script:ro" "$IMAGE" \
        sh /script/build-pve-qemu-kvm.sh --inside "$1" "$SUFFIX"
    say "the packages are in $OUT"
}

{ main "$@"; }
