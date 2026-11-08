#!/bin/bash
# Builds the distro's own Mesa again from its source package, at the version that's installed, with
# the distro's own options and amdgpu-virtio added, and installs it as the distro's packages with a
# version of their own. So the next Mesa update from the distro replaces it, and the package
# manager's hook builds that one too. The whole log goes to mesa-build.log, and the build's progress
# to the host and the terminal.
set -euo pipefail
here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
. "$here/../common.sh"
. "$here/amd-common.sh"
work=$root/var/cache/nvfwd/mesa
# What a build of Mesa takes on disk, with its source and packages.
room_needed_kib=$((8 * 1024 * 1024))
# The host hears of the build's progress this often at most.
progress_seconds=20
option=-Damdgpu-virtio=true

# The terminal, as everything else goes to the log.
exec 3>&1

# The reason goes where the service that runs this reads it.
refuse() {
    printf '%s\n' "$1" > "$amd_state/build-failure"
    echo "The Mesa build failed: $1" | tee -a "$mesa_log" >&3
    exit 1
}

# Ninja's [DONE/TOTAL] lines, said to the host now and then, and shown here as a share.
show_progress() {
    local line done total last=0 now
    while IFS= read -r line; do
        printf '%s\n' "$line" >> "$mesa_log"
        [[ $line =~ ^\[([0-9]+)/([0-9]+)\] ]] || continue
        done=${BASH_REMATCH[1]}
        total=${BASH_REMATCH[2]}
        now=$(date +%s)
        if [ $((now - last)) -ge "$progress_seconds" ] || [ "$done" = "$total" ]; then
            say_amd "building $done $total"
            echo "Building Mesa: $done of $total" >&3
            last=$now
        fi
    done
}

check_room() {
    local free
    mkdir -p "$work"
    free=$(df -Pk "$work" | awk 'NR == 2 { print $4 }')
    [ "${free:-0}" -ge "$room_needed_kib" ] ||
        refuse "it needs 8 GiB free under $work, and there's $((${free:-0} / 1024 / 1024)) GiB."
}

# Only the 64-bit Mesa is built. A 32-bit one, as Steam brings, would be left on the old version,
# which its packages refuse.
check_no_32_bit() {
    case $family in
    debian) ! dpkg-query -W -f '${Package}:${Architecture}\n' 'mesa-*' 'libgl1-mesa-dri' 2>/dev/null | grep -q ':i386$' ;;
    fedora) ! rpm -qa --qf '%{NAME}.%{ARCH}\n' 'mesa-*' | grep -q '\.i686$' ;;
    esac || refuse "32-bit Mesa packages are installed, and this only builds the 64-bit ones."
}

# Ubuntu's and Debian's source lists, deb822 or one line a list, with their source packages as well.
debian_sources() {
    local list
    for list in "$root"/etc/apt/sources.list.d/*.sources; do
        [ -e "$list" ] || continue
        sed -i 's/^Types: deb$/Types: deb deb-src/' "$list"
    done
    list=$root/etc/apt/sources.list
    if [ -e "$list" ] && ! grep -q '^deb-src ' "$list"; then
        sed -n 's/^deb /deb-src /p' "$list" > "$root/etc/apt/sources.list.d/nvfwd-mesa-src.list"
    fi
    apt-get update
}

# A changelog entry gives the packages their version, the distro's with the mark on the end.
debian_mark_version() {
    local version=$1 distribution
    distribution=$(head -n 1 debian/changelog | sed -E 's/^[^)]*\) ([^;]*);.*/\1/')
    {
        printf 'mesa (%s+%s) %s; urgency=medium\n\n' "$version" "$build_mark" "$distribution"
        printf '  * Built again in this VM with %s, for native context on an AMD card.\n\n' "$option"
        printf ' -- Guesthouse <root@localhost>  %s\n\n' "$(date -R)"
        cat debian/changelog
    } > debian/changelog.new
    mv debian/changelog.new debian/changelog
}

build_debian() {
    local version=$1 tree package installed=()
    export DEBIAN_FRONTEND=noninteractive
    debian_sources
    apt-get build-dep -y "mesa=$version" || refuse "apt-get could not install what Mesa $version builds with."
    rm -rf "${work:?}"/*
    cd "$work"
    apt-get -o APT::Sandbox::User=root source "mesa=$version" ||
        refuse "apt-get could not download the source of Mesa $version."
    tree=$(find . -maxdepth 1 -type d -name 'mesa-*' | head -n 1)
    [ -n "$tree" ] || refuse "apt-get source gave no tree for mesa $version."
    cd "$tree"
    grep -q '^override_dh_auto_configure:' debian/rules || refuse "debian/rules has no configure step to add $option to."
    sed -i "s/^override_dh_auto_configure:/confflags += $option\n\noverride_dh_auto_configure:/" debian/rules
    debian_mark_version "$version"
    DEB_BUILD_OPTIONS="nocheck parallel=$(nproc)" dpkg-buildpackage -b -uc -us 2>&1 | show_progress ||
        refuse "dpkg-buildpackage failed."
    cd "$work"
    # The packages that are installed now, and no others.
    for package in *.deb; do
        dpkg-query -W -f '${Status}' "$(dpkg-deb -f "$package" Package)" 2>/dev/null | grep -q 'ok installed' &&
            installed+=("./$package")
    done
    [ ${#installed[@]} -gt 0 ] || refuse "none of the built packages is one that's installed."
    apt-get install -y --allow-downgrades "${installed[@]}"
}

build_fedora() {
    local version=$1 spec rpms=() package name
    dnf install -y rpm-build dnf5-plugins
    rm -rf "${work:?}"/*
    cd "$work"
    dnf download --source "mesa-$version" || refuse "dnf could not download the source of Mesa $version."
    rpm -i --define "_topdir $work" mesa-*.src.rpm
    spec=$work/SPECS/mesa.spec
    grep -q '^%meson \\$' "$spec" || refuse "mesa.spec has no %meson step to add $option to."
    sed -i "s/^%meson \\\\$/%meson \\\\\n  $option \\\\/" "$spec"
    sed -i -E "s/^(Release:[[:space:]]*.*)$/\1.$build_mark/" "$spec"
    dnf builddep -y "$spec" || refuse "dnf could not install what Mesa $version builds with."
    rpmbuild --define "_topdir $work" --nocheck -bb "$spec" 2>&1 | show_progress || refuse "rpmbuild failed."
    for package in "$work"/RPMS/*/*.rpm; do
        name=$(rpm -qp --qf '%{NAME}' "$package")
        rpm -q "$name" > /dev/null 2>&1 && rpms+=("$package")
    done
    [ ${#rpms[@]} -gt 0 ] || refuse "none of the built packages is one that's installed."
    dnf install -y "${rpms[@]}"
}

family=${own_family:-debian}
mkdir -p "$log_folder" "$amd_state"
: > "$mesa_log"
rm -f "$amd_state/build-failure"
version=$(mesa_version)
[ -n "$version" ] || refuse "$mesa_package isn't installed, so there's no Mesa to build again."
echo "Building Mesa $version again with native context for the AMD GPU. This takes a while, and the log is $mesa_log."
check_room
check_no_32_bit
case $family in
debian) build_debian "$version" >> "$mesa_log" 2>&1 ;;
fedora) build_fedora "$version" >> "$mesa_log" 2>&1 ;;
*) refuse "building Mesa isn't set up for this distro, so it goes on with Venus." ;;
esac
mesa_is_ours || refuse "the built packages didn't install."
rm -rf "${work:?}"
echo "Built and installed Mesa $(mesa_version)."
say_amd "built $(mesa_version)"
