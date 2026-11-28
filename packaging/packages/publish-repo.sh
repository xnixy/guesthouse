#!/bin/sh
# Puts packages in a local copy of one of the signed repositories pkg.ruxn.com serves, and signs it.
# The workflows copy the repository down from the bucket first and up again after, the packages before
# the signed index, so a client never gets an index naming a package that isn't there yet.
#
# Usage: packaging/packages/publish-repo.sh KIND TREE [SUITE] [PACKAGE...]
#
# KIND is apt, rpm or pacman for Guesthouse's own, or proxmox for the GPU-PV pve-qemu-kvm. TREE is
# the repository's folder. SUITE is apt's and proxmox's only: stable for Guesthouse's apt repository,
# testing or stable for Proxmox's. With no packages it signs the indexes again, which is how a promotion
# that copied a package into a suite's pool finishes.
#
# SIGNING_KEY_FILE names a file with the armored secret key, which is imported into a keyring of the
# container's own and never leaves it. SIGNING_KEY is its fingerprint. The tools are installed first, in
# an image of its own made with no key. The key is only in the second container, which has no network
# and installs nothing. A package already in the pool
# under the same name stays as it is, as what's published never changes. The newest KEEP versions stay
# in each pool, 3 if it isn't set, so a version can still be gone back to.
set -eu

KEEP=${KEEP:-3}
# Where the repository is in the signing container.
TREE=${TREE:-/tree}
ORIGIN='ruxn labs'
PACMAN_DB=guesthouse
RPM_REPO_FILE=guesthouse.repo
RPM_BASE=https://pkg.ruxn.com/guesthouse/rpm
KEY_URL=https://pkg.ruxn.com/ruxn-pkg.asc
# How many days a Proxmox suite's index stays valid. proxmox-refresh.yml signs it again each day.
VALID_DAYS=${VALID_DAYS:-14}

REPO=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)

say() {
    printf '%s\n' "$*"
}

die() {
    printf 'publish-repo: %s\n' "$*" >&2
    exit 1
}

find_engine() {
    ENGINE=$(command -v podman || command -v docker) || die "this needs podman or docker."
    case ${ENGINE##*/} in
        docker) OWNER=$(id -u):$(id -g) ;;
        *) OWNER= ;;
    esac
}

image_for() {
    case $1 in
        apt | proxmox) say "${DEB_IMAGE:-$(sh "$REPO/packaging/images/image-of.sh" debian)}" ;;
        rpm) say "${RPM_IMAGE:-$(sh "$REPO/packaging/images/image-of.sh" fedora)}" ;;
        pacman) say "${PACMAN_IMAGE:-$(sh "$REPO/packaging/images/image-of.sh" archlinux)}" ;;
    esac
}

# The tools, installed into the image the signing container is made from. No key is here.
prepare() {
    case $1 in
        apt | proxmox)
            apt-get update -qq
            apt-get install -y -qq --no-install-recommends apt-utils gnupg xz-utils >/dev/null
            ;;
        rpm) dnf -y -q install rpm-sign createrepo_c gnupg2 >/dev/null 2>&1 ;;
        pacman)
            for tool in gpg repo-add; do
                command -v "$tool" >/dev/null || die "the image has no $tool."
            done
            ;;
    esac
}

# The rest runs in the signing container, as its root.

hand_back() {
    [ -z "${OWNER:-}" ] || chown -R "$OWNER" "$TREE"
}

import_key() {
    export GNUPGHOME=/tmp/gnupg
    rm -rf "$GNUPGHOME"
    mkdir -m 700 "$GNUPGHOME"
    gpg --batch --quiet --import /signing-key.asc
    gpg --batch --list-secret-keys --with-colons "$SIGNING_KEY" | grep -q '^sec' ||
        die "the key in SIGNING_KEY_FILE isn't $SIGNING_KEY."
}

sign_detached() {
    gpg --batch --yes --local-user "$SIGNING_KEY" "$@"
}

# A package that's there already stays, and a new one goes in. Each is checked to be what its name says.
add_to_pool() {
    pool=$1
    shift
    mkdir -p "$pool"
    for package in "$@"; do
        name=${package##*/}
        if [ -e "$pool/$name" ]; then
            say "$name is published already, so the published one stays."
            continue
        fi
        cp "$package" "$pool/$name.new"
        mv "$pool/$name.new" "$pool/$name"
        ADDED="$ADDED $pool/$name"
    done
}

# The newest $KEEP of each package's versions stay, by the version in the name.
prune_pool() {
    pool=$1
    pattern=$2
    # shellcheck disable=SC2010 # names of our own making, with no spaces
    ls "$pool" | grep -E "$pattern" | sort -V | head -n "-$KEEP" | while IFS= read -r old; do
        say "removing $old, as $KEEP newer versions are kept"
        rm -f "$pool/$old" "$pool/$old.sig"
    done
}

apt_release() {
    suite=$1
    label=$2
    cd "$TREE"
    mkdir -p "dists/$suite/main/binary-amd64"
    packages=dists/$suite/main/binary-amd64/Packages
    apt-ftparchive packages "pool/$suite" >"$packages"
    gzip -9nkf "$packages"
    xz -kf "$packages"
    apt-ftparchive \
        -o "APT::FTPArchive::Release::Origin=$ORIGIN" \
        -o "APT::FTPArchive::Release::Label=$label" \
        -o "APT::FTPArchive::Release::Suite=$suite" \
        -o "APT::FTPArchive::Release::Codename=$suite" \
        -o APT::FTPArchive::Release::Architectures=amd64 \
        -o APT::FTPArchive::Release::Components=main \
        release "dists/$suite" >"dists/$suite/Release.new"
    if [ -n "${VALID_UNTIL:-}" ]; then
        sed "/^Date: /a Valid-Until: $VALID_UNTIL" "dists/$suite/Release.new" >"dists/$suite/Release"
        grep -q '^Valid-Until: ' "dists/$suite/Release" || die "the Release file has no Date line to put Valid-Until after."
        rm -f "dists/$suite/Release.new"
    else
        mv "dists/$suite/Release.new" "dists/$suite/Release"
    fi
    sign_detached --clearsign -o "dists/$suite/InRelease" "dists/$suite/Release"
    sign_detached --armor --detach-sign -o "dists/$suite/Release.gpg" "dists/$suite/Release"
}

publish_apt() {
    suite=$1
    label=$2
    package_pattern=$3
    shift 3
    import_key
    for package in "$@"; do
        dpkg-deb -f "$package" Package | grep -Eqx "$package_pattern" || die "${package##*/} isn't a package of this repository."
    done
    add_to_pool "$TREE/pool/$suite" "$@"
    # shellcheck disable=SC2010 # names of our own making, with no spaces
    for name in $(ls "$TREE/pool/$suite" | grep '\.deb$' | sed 's/_.*//' | sort -u); do
        prune_pool "$TREE/pool/$suite" "^${name}_.*\\.deb\$"
    done
    apt_release "$suite" "$label"
}

publish_rpm() {
    import_key
    add_to_pool "$TREE/packages" "$@"
    for package in $ADDED; do
        rpmsign --define "_gpg_name $SIGNING_KEY" --addsign "$package" >/dev/null
    done
    prune_pool "$TREE/packages" '^guesthouse-[0-9].*\.x86_64\.rpm$'
    createrepo_c --quiet --update "$TREE"
    sign_detached --armor --detach-sign -o "$TREE/repodata/repomd.xml.asc" "$TREE/repodata/repomd.xml"
    cat >"$TREE/$RPM_REPO_FILE" <<EOF
[guesthouse]
name=Guesthouse
baseurl=$RPM_BASE
enabled=1
gpgcheck=1
repo_gpgcheck=1
gpgkey=$KEY_URL
EOF
}

# pacman reads one version of each package from the database, so older files stay only for going back
# by hand. The database's links become copies, as the bucket serves files and not links.
publish_pacman() {
    import_key
    folder=$TREE/x86_64
    add_to_pool "$folder" "$@"
    for package in $ADDED; do
        sign_detached --detach-sign --no-armor -o "$package.sig" "$package"
    done
    for package in $ADDED; do
        repo-add --quiet --nocolor "$folder/$PACMAN_DB.db.tar.gz" "$package"
    done
    prune_pool "$folder" '^guesthouse-[0-9].*-x86_64\.pkg\.tar\.zst$'
    for part in db files; do
        archive=$folder/$PACMAN_DB.$part.tar.gz
        [ -f "$archive" ] || continue
        sign_detached --detach-sign --no-armor -o "$archive.sig" "$archive"
        rm -f "$folder/$PACMAN_DB.$part" "$folder/$PACMAN_DB.$part.sig"
        cp "$archive" "$folder/$PACMAN_DB.$part"
        cp "$archive.sig" "$folder/$PACMAN_DB.$part.sig"
    done
    rm -f "$folder"/*.old "$folder"/*.old.sig
}

# Sets IMAGE_NAME to an image of the distribution's, with the tools put in. The script is copied into it, so the signing
# container runs the same file. Nothing of the key is anywhere near.
prepared_image() {
    kind=$1
    context=$(mktemp -d)
    IMAGE_NAME=guesthouse-publish-$kind-$$
    trap 'rm -rf "$context"; "$ENGINE" rmi -f "$IMAGE_NAME" >/dev/null 2>&1 || true' EXIT
    cp "$REPO/packaging/packages/publish-repo.sh" "$context/publish-repo.sh"
    printf 'FROM %s\nCOPY publish-repo.sh /script/publish-repo.sh\nRUN sh /script/publish-repo.sh --prepare %s\n' \
        "$(image_for "$kind")" "$kind" >"$context/Containerfile"
    "$ENGINE" build --quiet -t "$IMAGE_NAME" -f "$context/Containerfile" "$context" >/dev/null || die "couldn't make the image."
}

inside() {
    kind=$1
    shift
    ADDED=
    trap hand_back EXIT
    case $kind in
        apt) publish_apt "$SUITE" Guesthouse 'guesthouse' "$@" ;;
        proxmox) publish_apt "$SUITE" 'Guesthouse GPU-PV pve-qemu-kvm' 'pve-qemu-kvm(-dbgsym)?' "$@" ;;
        rpm) publish_rpm "$@" ;;
        pacman) publish_pacman "$@" ;;
    esac
    say "the $kind repository is signed"
}

main() {
    if [ "${1:-}" = --prepare ]; then
        shift
        prepare "$@"
        return
    fi
    if [ "${1:-}" = --inside ]; then
        shift
        inside "$@"
        return
    fi
    [ $# -ge 2 ] || die "usage: publish-repo.sh KIND TREE [SUITE] [PACKAGE...]"
    kind=$1
    tree=$2
    shift 2
    SUITE=
    case $kind in
        apt)
            SUITE=${1:-}
            [ "$SUITE" = stable ] || die "Guesthouse's apt repository has the one suite, stable."
            shift
            ;;
        proxmox)
            SUITE=${1:-}
            case $SUITE in testing | stable) ;; *) die "Proxmox's suites are testing and stable." ;; esac
            shift
            ;;
        rpm | pacman) ;;
        *) die "there's no kind $kind. It's apt, rpm, pacman or proxmox." ;;
    esac
    [ -n "${SIGNING_KEY:-}" ] && [ -f "${SIGNING_KEY_FILE:-}" ] || die "SIGNING_KEY and SIGNING_KEY_FILE have to name the key."
    printf '%s' "$SIGNING_KEY" | grep -Eqx '[0-9A-F]{40}' || die "SIGNING_KEY has to be the key's fingerprint."
    find_engine
    mkdir -p "$tree"
    tree=$(CDPATH='' cd -- "$tree" && pwd)
    mounts=
    for package in "$@"; do
        [ -f "$package" ] || die "there's no file called $package."
        case ${package##*/} in
            *[!A-Za-z0-9._+~-]*) die "${package##*/} has a character a package's name doesn't." ;;
        esac
        mounts="$mounts -v $(CDPATH='' cd -- "$(dirname -- "$package")" && pwd)/${package##*/}:/new/${package##*/}:ro"
    done
    # In the container, the packages are mounted under /new.
    for package in "$@"; do
        set -- "$@" "/new/${package##*/}"
        shift
    done
    key_file=$(CDPATH='' cd -- "$(dirname -- "$SIGNING_KEY_FILE")" && pwd)/${SIGNING_KEY_FILE##*/}
    valid_until=
    if [ "$kind" = proxmox ]; then
        valid_until=$(date -u -d "+$VALID_DAYS days" '+%a, %d %b %Y %H:%M:%S UTC') || die "can't work out the Valid-Until date."
    fi
    prepared_image "$kind"
    # The key is in this container only. It has no network, and the tools are in the image already.
    # shellcheck disable=SC2086 # the mounts are flags, and their paths have no spaces
    "$ENGINE" run --rm --network none --security-opt label=disable -e "OWNER=$OWNER" -e "SIGNING_KEY=$SIGNING_KEY" \
        -e "SUITE=$SUITE" -e "KEEP=$KEEP" -e "VALID_UNTIL=$valid_until" -v "$key_file:/signing-key.asc:ro" \
        -v "$tree:/tree" $mounts "$IMAGE_NAME" sh /script/publish-repo.sh --inside "$kind" "$@"
}

{ main "$@"; }
