#!/bin/bash
# Tests packaging/packages/publish-repo.sh without a container engine: a stand-in podman records how it is
# called, and stand-in apt-ftparchive and gpg stand for the tools in the signing container. Checks that the
# images are named by digest, that the tools go into an image made with no key, that the container that holds
# the key has no network and installs nothing, and that a Proxmox suite's Release file says how long it's valid.
#
# Usage: tests/packages/publish-repo.sh
set -u

HERE=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH='' cd -- "$HERE/../.." && pwd)
SCRIPT=${PUBLISH_REPO:-$REPO/packaging/packages/publish-repo.sh}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
failed=0

fail() {
    printf 'FAILED: %s\n' "$*"
    failed=1
}

mkdir -p "$WORK/bin" "$WORK/tree"
printf 'fake key\n' >"$WORK/key.asc"

# The engine: records each call, and keeps the Containerfile of a build.
cat >"$WORK/bin/podman" <<'STUB'
#!/bin/sh
printf '%s\n' "$*" >>"$CALLS"
case $1 in
    build)
        while [ $# -gt 0 ]; do
            [ "$1" = -f ] && cp "$2" "$CALLS.containerfile.$(wc -l <"$CALLS")"
            shift
        done
        ;;
esac
exit 0
STUB
chmod +x "$WORK/bin/podman"

run_kind() {
    kind=$1
    shift
    CALLS=$WORK/calls.$kind
    : >"$CALLS"
    export CALLS
    PATH=$WORK/bin:$PATH SIGNING_KEY=0123456789ABCDEF0123456789ABCDEF01234567 SIGNING_KEY_FILE=$WORK/key.asc \
        sh "$SCRIPT" "$kind" "$WORK/tree" "$@" >"$WORK/out.$kind" 2>&1 || {
        fail "$kind: the script stopped: $(cat "$WORK/out.$kind")"
        return
    }
}

check_kind() {
    kind=$1
    calls=$WORK/calls.$kind
    build_line=$(grep -n '^build ' "$calls" | head -n 1 | cut -d: -f1)
    run_line=$(grep -n '^run ' "$calls" | head -n 1 | cut -d: -f1)
    [ -n "$build_line" ] || fail "$kind: no image was built for the tools."
    [ -n "$run_line" ] || fail "$kind: nothing was run."
    [ -n "$build_line" ] && [ -n "$run_line" ] && [ "$build_line" -lt "$run_line" ] || fail "$kind: the tools aren't installed before the signing container runs."
    grep -q key.asc <(sed -n "${build_line}p" "$calls") && fail "$kind: the key is near the build of the tools' image."
    grep -Eq '^FROM .*@sha256:[0-9a-f]{64}$' "$calls.containerfile.$build_line" || fail "$kind: the base image isn't named by digest."
    run=$(sed -n "${run_line}p" "$calls")
    case $run in *key.asc*) ;; *) fail "$kind: the signing container has no key." ;; esac
    case $run in *"--network none"*) ;; *) fail "$kind: the container with the key has a network." ;; esac
    grep -q 'apt-get\|dnf\|pacman -S' <(tr ' ' '\n' <<<"$run") && fail "$kind: the container with the key installs packages."
    grep -q -- '--prepare' <<<"$run" && fail "$kind: the tools are installed in the container with the key."
    [ "$(grep -c '^run ' "$calls")" = 1 ] || fail "$kind: more than one container ran."
    image=$(sed -n "${build_line}p" "$calls" | sed 's/.* -t \([^ ]*\) .*/\1/')
    case $run in *" $image "*) ;; *) fail "$kind: the container that signs isn't from the image the tools went into." ;; esac
}

for kind in apt rpm pacman; do
    suite=
    [ "$kind" = apt ] && suite=stable
    run_kind "$kind" $suite
    check_kind "$kind"
done
run_kind proxmox testing
check_kind proxmox

# Valid-Until is for Proxmox's suites, and the Release file carries it after Date.
grep -q 'VALID_UNTIL=[A-Z][a-z][a-z], [0-9][0-9] [A-Z][a-z][a-z] 20[0-9][0-9] [0-9:]* UTC' "$WORK/calls.proxmox" || fail "proxmox: the run isn't given a Valid-Until date."
grep -q 'VALID_UNTIL= ' "$WORK/calls.apt" || fail "apt: the run is given a Valid-Until date."

# Inside the signing container: stand-ins for the tools, and the Release file they make.
mkdir -p "$WORK/inside/bin" "$WORK/inside/tree/pool/testing"
cat >"$WORK/inside/bin/apt-ftparchive" <<'STUB'
#!/bin/sh
case " $* " in
    *" release "*) printf 'Origin: ruxn labs\nDate: Mon, 01 Jan 2029 00:00:00 UTC\nSHA256:\n' ;;
esac
exit 0
STUB
cat >"$WORK/inside/bin/gpg" <<'STUB'
#!/bin/sh
case $* in
    *--list-secret-keys*) echo sec ;;
    *-o*) while [ $# -gt 0 ]; do [ "$1" = -o ] && { shift; cp "$2" "$1"; break; }; shift; done ;;
esac
exit 0
STUB
cat >"$WORK/inside/bin/xz" <<'STUB'
#!/bin/sh
exit 0
STUB
chmod +x "$WORK/inside/bin/"*
inside() {
    PATH=$WORK/inside/bin:$PATH TREE=$WORK/inside/tree SIGNING_KEY=0123456789ABCDEF0123456789ABCDEF01234567 \
        SUITE=testing "$@" sh "$SCRIPT" --inside proxmox
}
inside env VALID_UNTIL='Mon, 15 Jan 2029 00:00:00 UTC' >"$WORK/inside.out" 2>&1 || fail "the signing run stopped: $(cat "$WORK/inside.out")"
release=$WORK/inside/tree/dists/testing/Release
[ "$(sed -n '/^Date: /{n;p}' "$release")" = 'Valid-Until: Mon, 15 Jan 2029 00:00:00 UTC' ] || fail "the Release file doesn't have Valid-Until after Date."
inside env VALID_UNTIL= >"$WORK/inside.out" 2>&1 || fail "the signing run without a date stopped"
! grep -q Valid-Until "$release" || fail "a Release file has Valid-Until when none was asked for."

# The images come from packaging/images/Dockerfile, each by digest.
for name in debian fedora archlinux; do
    image=$(sh "$REPO/packaging/images/image-of.sh" "$name") || fail "image-of.sh has no $name."
    printf '%s' "$image" | grep -Eq '@sha256:[0-9a-f]{64}$' || fail "the $name image isn't named by digest: $image"
    cat "$WORK"/calls.*.containerfile.* | grep -qF "FROM $image" || fail "no image was made from the $name image"
done
sh "$REPO/packaging/images/image-of.sh" nothing >/dev/null 2>&1 && fail "image-of.sh gave an image for a name it hasn't got."

[ "$failed" = 0 ] && echo "publish-repo: all checks passed"
exit "$failed"
