#!/bin/sh
# Builds Guesthouse's deb, rpm and pacman packages from a release's payload, the same tarball install.sh
# downloads, so a package holds exactly the release's files. Each is built in a container of the
# system it's for, with that system's own tools. They come out unsigned: publish-repo.sh signs them as
# it puts them in a repository.
#
# Usage: packaging/packages/build-packages.sh [--out FOLDER] PAYLOAD [FORMAT...]
#
# PAYLOAD is guesthouse-VERSION-x86_64.tar.xz. FORMAT is deb, rpm or pacman, all three if none is
# named. The packages end up in FOLDER, build/packages if it isn't given. It needs podman or docker and
# no root, and publishes nothing.
#
# What a package puts where, and how that differs from install.sh, is in docs/install.md. In short:
# the release goes in /usr/libexec/guesthouse, where the app looks when it isn't in your home folder,
# with a setuid copy of QEMU's bridge helper beside it, the launcher and icons in /usr/share, and the
# app, guesthouse-vm and guesthouse-check in /usr/bin. install.sh and uninstall.sh are left out, and
# the file installed-by names the package manager, which is how the app knows how to update.
set -eu

# The images come from packaging/images/Dockerfile, unless one of these names another.
# The package's own release number, for a package that changes while the payload doesn't.
PACKAGE_RELEASE=${PACKAGE_RELEASE:-1}
PACKAGER=${PACKAGER:-ruxn labs <packages@ruxn.com>}
HOMEPAGE=https://github.com/ruxnlabs/guesthouse
SUMMARY='Windows and Linux machines on KVM, with the real GPU driver'
LICENSE_NAME=GPL-2.0-or-later
PROGRAM=/usr/libexec/guesthouse
APP_ID=io.github.xnixy.Guesthouse
BRIDGE_HELPER_SOURCE=qemu/build/qemu-bridge-helper
# Beside the program, where only root can write. install.sh puts its copy in /usr/local, which is the
# system administrator's and never a package's, so a package's copy is here, and the app knows both.
BRIDGE_HELPER=$PROGRAM/qemu-bridge-helper
# What a user runs from a terminal, besides the app.
COMMANDS='guesthouse guesthouse-vm guesthouse-check'

# What each system's packages are called for what install.sh's package_for lists, for a GPU install
# with Linux guests, which is what a package always has. Plus curl, for the app's update check, and
# pkexec, for the app's update.
DEB_DEPENDS='libc6 (>= 2.39), libglib2.0-0t64 | libglib2.0-0, libgtk-4-1 (>= 4.14), libadwaita-1-0 (>= 1.5),
 libpipewire-0.3-0t64 | libpipewire-0.3-0, libepoxy0, libgbm1, libegl1, libdrm2,
 libdrm-amdgpu1 (>= 2.4.121), libcap-ng0, qemu-utils, ovmf, 7zip, genisoimage, swtpm, swtpm-tools,
 openssl, openssh-client, bubblewrap, gcc, curl'
DEB_RECOMMENDS='pkexec | policykit-1'
RPM_REQUIRES='glibc >= 2.39, gtk4 >= 4.16, libadwaita, pipewire-libs, libepoxy, mesa-libgbm, mesa-libEGL,
 libdrm >= 2.4.121, libcap-ng, qemu-img, edk2-ovmf, 7zip, genisoimage, swtpm, swtpm-tools, openssl,
 openssh, bubblewrap, gcc, curl'
RPM_RECOMMENDS='polkit'
PACMAN_DEPENDS="'glibc>=2.39' 'gtk4>=4.16' libadwaita libpipewire libepoxy mesa libglvnd 'libdrm>=2.4.121'
 libcap-ng qemu-img edk2-ovmf 7zip cdrtools swtpm openssl openssh bubblewrap gcc curl"
PACMAN_OPTIONAL="'polkit: for the app to update itself'"

REPO=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
OUT=$REPO/build/packages

say() {
    printf '%s\n' "$*"
}

die() {
    printf 'build-packages: %s\n' "$*" >&2
    exit 1
}

find_engine() {
    ENGINE=$(command -v podman || command -v docker) || die "this needs podman or docker."
    # Podman without root maps the container's root to the caller already, and Docker's is root here.
    case ${ENGINE##*/} in
        docker) OWNER=$(id -u):$(id -g) ;;
        *) OWNER= ;;
    esac
}

image_for() {
    case $1 in
        deb) say "${DEB_IMAGE:-$(sh "$REPO/packaging/images/image-of.sh" debian)}" ;;
        rpm) say "${RPM_IMAGE:-$(sh "$REPO/packaging/images/image-of.sh" fedora)}" ;;
        pacman) say "${PACMAN_IMAGE:-$(sh "$REPO/packaging/images/image-of.sh" archlinux)}" ;;
    esac
}

# The rest runs in the container, as its root.

hand_back() {
    [ -z "${OWNER:-}" ] || chown -R "$OWNER" /out
}

# The payload's files where a package puts them. installed-by says which package manager this is.
stage() {
    format=$1
    manager=$2
    STAGE=/work/stage
    rm -rf /work
    mkdir -p "$STAGE$PROGRAM" "$STAGE/usr/bin" "$STAGE/usr/share/applications" "$STAGE/usr/share/doc/guesthouse"
    tar -xJf /payload.tar.xz -C /work --no-same-owner
    tree=/work/guesthouse-$VERSION-x86_64
    [ -f "$tree/RELEASE" ] || die "the payload has no guesthouse-$VERSION-x86_64/RELEASE."
    [ "$(sed -n 's/^version=//p' "$tree/RELEASE")" = "$VERSION" ] || die "the payload isn't version $VERSION."

    # The package manager installs and removes it, and the app's update goes through it.
    rm -f "$tree/install.sh" "$tree/uninstall.sh" "$tree/needs-symbols"
    if [ "$format" = deb ]; then
        mv "$tree/guesthouse" "$tree/guesthouse-hdr"
    else
        rm -f "$tree/guesthouse-no-hdr"
    fi
    mv "$tree/share/$APP_ID.desktop" "$STAGE/usr/share/applications/"
    mv "$tree/share/icons" "$STAGE/usr/share/icons"
    rmdir "$tree/share"
    cp -a "$tree/." "$STAGE$PROGRAM/"
    install -m 0755 /script/package-setup "$STAGE$PROGRAM/package-setup"
    say "$manager" >"$STAGE$PROGRAM/installed-by"
    chmod 0644 "$STAGE$PROGRAM/installed-by"
    install -m 4755 "$tree/$BRIDGE_HELPER_SOURCE" "$STAGE$BRIDGE_HELPER"
    for command in $COMMANDS; do
        ln -s "../libexec/guesthouse/$command" "$STAGE/usr/bin/$command"
    done
    cp "$tree/LICENSE" "$STAGE/usr/share/doc/guesthouse/copyright"
    # Nothing in the payload is writable by anyone but its owner, whatever umask unpacked it.
    chmod -R go-w "$STAGE"
}

build_deb() {
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends xz-utils dpkg-dev >/dev/null
    stage deb apt
    mkdir "$STAGE/DEBIAN"
    size=$(du -sk --exclude=DEBIAN "$STAGE" | cut -f 1)
    cat >"$STAGE/DEBIAN/control" <<EOF
Package: guesthouse
Version: $VERSION-$PACKAGE_RELEASE
Architecture: amd64
Maintainer: $PACKAGER
Installed-Size: $size
Depends: $DEB_DEPENDS
Recommends: $DEB_RECOMMENDS
Section: admin
Priority: optional
Homepage: $HOMEPAGE
Description: $SUMMARY
 Guesthouse runs Windows and Linux virtual machines on KVM, with the host's
 GPU shared to the guest through its real driver.
EOF
    # dpkg calls the script again when GTK's library changes, so the app with HDR follows an upgrade.
    say "interest-noawait /usr/lib/x86_64-linux-gnu/libgtk-4.so.1" >"$STAGE/DEBIAN/triggers"
    cat >"$STAGE/DEBIAN/postinst" <<EOF
#!/bin/sh
set -e
case \$1 in
    configure | triggered) $PROGRAM/package-setup configure ;;
esac
EOF
    cat >"$STAGE/DEBIAN/prerm" <<EOF
#!/bin/sh
set -e
[ "\$1" != remove ] || $PROGRAM/package-setup remove
EOF
    chmod 0755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/prerm"
    (cd "$STAGE" && find . -path ./DEBIAN -prune -o -type f -print0 | sort -z | xargs -0 md5sum) |
        sed 's| \./| |' >"$STAGE/DEBIAN/md5sums"
    dpkg-deb --root-owner-group -Zxz --build "$STAGE" "/out/guesthouse_${VERSION}-${PACKAGE_RELEASE}_amd64.deb"
}

build_rpm() {
    dnf -y -q install rpm-build xz findutils >/dev/null
    stage rpm dnf
    mkdir -p /work/rpmbuild/SPECS
    # The payload is built already, and its programs, libraries and guest scripts go in as they are:
    # nothing stripped, no debug package, no shebangs changed and no requirements read from the files,
    # as the bundled QEMU's libraries are its own and not the system's to provide.
    {
        cat <<EOF
Name: guesthouse
Version: $VERSION
Release: $PACKAGE_RELEASE
Summary: $SUMMARY
License: $LICENSE_NAME
URL: $HOMEPAGE
ExclusiveArch: x86_64
AutoReqProv: no
Requires: $(say "$RPM_REQUIRES" | tr -d '\n')
Recommends: $RPM_RECOMMENDS
%global debug_package %{nil}
%global __os_install_post %{nil}
%global _build_id_links none

%description
Guesthouse runs Windows and Linux virtual machines on KVM, with the host's GPU shared to the guest
through its real driver.

%install
cp -a /work/stage/. %{buildroot}/

%post
$PROGRAM/package-setup configure

%preun
[ "\$1" -ne 0 ] || $PROGRAM/package-setup remove

%files
%dir $PROGRAM
EOF
        # One line for each file, so the setuid helper can have its own.
        (cd "$STAGE" && find "${PROGRAM#/}" -mindepth 1 -type d -printf '%%dir "/%p"\n')
        (cd "$STAGE" && find "${PROGRAM#/}" ! -type d ! -path "${BRIDGE_HELPER#/}" -printf '"/%p"\n')
        say "%attr(4755, root, root) $BRIDGE_HELPER"
        for command in $COMMANDS; do say "/usr/bin/$command"; done
        say "/usr/share/applications/$APP_ID.desktop"
        (cd "$STAGE" && find usr/share/icons -type f -printf '/%p\n')
        say "%doc /usr/share/doc/guesthouse/copyright"
    } >/work/rpmbuild/SPECS/guesthouse.spec
    rpmbuild --define '_topdir /work/rpmbuild' -bb /work/rpmbuild/SPECS/guesthouse.spec >/work/rpmbuild.log 2>&1 || {
        tail -n 40 /work/rpmbuild.log
        die "rpmbuild failed."
    }
    cp /work/rpmbuild/RPMS/x86_64/guesthouse-"$VERSION"-"$PACKAGE_RELEASE".x86_64.rpm /out/
}

build_pacman() {
    pacman -Sy --noconfirm --needed base-devel >/dev/null
    stage pacman pacman
    useradd -m builder 2>/dev/null || true
    mkdir -p /work/pkg
    # makepkg won't run as root. fakeroot gives the package's files to root, and the helper its bit. Its
    # options keep every file and folder of the payload as it is: nothing stripped, compressed or purged.
    cat >/work/pkg/PKGBUILD <<EOF
pkgname=guesthouse
pkgver=$VERSION
pkgrel=$PACKAGE_RELEASE
pkgdesc='$SUMMARY'
arch=(x86_64)
url='$HOMEPAGE'
license=('$LICENSE_NAME')
depends=($PACMAN_DEPENDS)
optdepends=($PACMAN_OPTIONAL)
options=(!strip !debug !lto !zipman !purge libtool staticlibs emptydirs docs)
install=guesthouse.install

package() {
    cp -r --preserve=mode,links /work/stage/. "\$pkgdir/"
    chmod 4755 "\$pkgdir$BRIDGE_HELPER"
}
EOF
    cat >/work/pkg/guesthouse.install <<EOF
post_install() {
    $PROGRAM/package-setup configure
}

post_upgrade() {
    $PROGRAM/package-setup configure
}

pre_remove() {
    $PROGRAM/package-setup remove
}
EOF
    chown -R builder /work
    runuser -u builder -- env PACKAGER="$PACKAGER" PKGDEST=/work/out sh -c 'cd /work/pkg && makepkg --nodeps --noconfirm' \
        >/work/makepkg.log 2>&1 || {
        tail -n 40 /work/makepkg.log
        die "makepkg failed."
    }
    cp /work/out/guesthouse-"$VERSION"-"$PACKAGE_RELEASE"-x86_64.pkg.tar.zst /out/
}

inside() {
    format=$1
    VERSION=$2
    trap hand_back EXIT
    "build_$format"
    say "built the $format package"
}

main() {
    while [ $# -gt 0 ]; do
        case $1 in
            --out) OUT=$2 && shift 2 ;;
            --inside)
                shift
                inside "$@"
                return
                ;;
            -*) die "there's no option $1." ;;
            *) break ;;
        esac
    done
    [ $# -ge 1 ] || die "name the payload, guesthouse-VERSION-x86_64.tar.xz."
    payload=$(CDPATH='' cd -- "$(dirname -- "$1")" && pwd)/${1##*/}
    [ -f "$payload" ] || die "there's no file called $1."
    version=${payload##*/guesthouse-}
    version=${version%-x86_64.tar.xz}
    printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' ||
        die "$1 isn't called guesthouse-VERSION-x86_64.tar.xz with a version of three numbers."
    printf '%s' "$PACKAGE_RELEASE" | grep -Eqx '[0-9]+(\.[0-9]+)?' ||
        die "PACKAGE_RELEASE is a number, or two with a dot, as all three formats take."
    shift
    [ $# -gt 0 ] || set -- deb rpm pacman
    for format in "$@"; do
        case $format in
            deb | rpm | pacman) ;;
            *) die "there's no format $format. It's deb, rpm or pacman." ;;
        esac
    done
    find_engine
    mkdir -p "$OUT"
    OUT=$(CDPATH='' cd -- "$OUT" && pwd)
    # A file's time in the packages, when it's set. rpmbuild refuses an empty one.
    epoch=
    [ -z "${SOURCE_DATE_EPOCH:-}" ] || epoch="-e SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH"
    for format in "$@"; do
        # shellcheck disable=SC2086 # empty, or one flag with a number
        "$ENGINE" run --rm --security-opt label=disable -e "OWNER=$OWNER" -e "PACKAGER=$PACKAGER" \
            -e "PACKAGE_RELEASE=$PACKAGE_RELEASE" $epoch \
            -v "$payload:/payload.tar.xz:ro" -v "$REPO/packaging/packages:/script:ro" -v "$OUT:/out" \
            "$(image_for "$format")" sh /script/build-packages.sh --inside "$format" "$version"
    done
    say "the packages are in $OUT"
}

{ main "$@"; }
