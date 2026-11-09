#!/bin/sh
# Tests how the packaging gets the bundled libraries' source packages, on a copy of what's committed. run.sh
# runs it. apt-get and apt-cache are stand-ins, so nothing is downloaded: the archive is a folder here.
#
# Usage: tests/install/library-sources.sh
set -u

HERE=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH='' cd -- "$HERE/../.." && pwd)
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
failed=0

check() {
    if eval "$2"; then
        printf 'ok: %s\n' "$1"
    else
        printf 'FAILED: %s\n' "$1"
        failed=1
    fi
}

mkdir -p "$T/tree" "$T/bin" "$T/archive" "$T/out/stage"
if [ -e "$REPO/.git" ]; then
    git -C "$REPO" archive HEAD | tar -xC "$T/tree"
else
    cp -R "$REPO/." "$T/tree/"
fi

# One source package in the archive, with a version that has an epoch, as zlib's does.
printf 'the original\n' >"$T/archive/zlib_1.3.orig.tar.xz"
printf 'the changes\n' >"$T/archive/zlib_1.3-1.debian.tar.xz"
sum() { sha256sum "$1" | cut -d ' ' -f 1; }
cat >"$T/archive/zlib_1.3-1.dsc" <<DSC
Format: 3.0 (quilt)
Source: zlib
Version: 1:1.3-1
Checksums-Sha256:
 $(sum "$T/archive/zlib_1.3.orig.tar.xz") 14 zlib_1.3.orig.tar.xz
 $(sum "$T/archive/zlib_1.3-1.debian.tar.xz") 12 zlib_1.3-1.debian.tar.xz
Files:
 0 14 zlib_1.3.orig.tar.xz
DSC
cat >"$T/out/stage/LIBRARIES.txt" <<'LIST'
# A comment, as the real one starts with.
libz.so.1 zlib1g 1:1.3-1 source zlib 1:1.3-1
libvirglrenderer.so.1 virglrenderer virglrenderer-1.3.0, built here from upstream
LIST

# apt-cache answers from the archive's .dsc. apt-get copies the package in, or fails as FAIL says.
cat >"$T/bin/apt-cache" <<STAND_IN
#!/bin/sh
printf 'Package: zlib\nVersion: 1:1.3-1\nChecksums-Sha256:\n %s 300 zlib_1.3-1.dsc\n\n' "\$(sha256sum "$T/archive/zlib_1.3-1.dsc" | cut -d ' ' -f 1)"
STAND_IN
cat >"$T/bin/apt-get" <<STAND_IN
#!/bin/sh
printf '%s\n' "\$*" >>"$T/apt-get-calls"
case \${FAIL:-} in
    connect) echo "E: Failed to fetch http://archive.ubuntu.com/x  Could not connect to archive.ubuntu.com:80, connection timed out"; exit 100 ;;
    gone) echo "E: Can not find version '1:1.3-1' of package 'zlib'"; exit 100 ;;
esac
cp "$T"/archive/* .
STAND_IN
chmod +x "$T/bin/apt-cache" "$T/bin/apt-get"
libraries=$T/out/bundled-source/libraries

fetch() {
    : >"$T/apt-get-calls"
    PATH=$T/bin:$PATH sh "$T/tree/packaging/build-release.sh" --out "$T/out" library-sources >"$T/said" 2>&1
}

fetch
check 'a package that isn'"'"'t here is downloaded' '[ $? -eq 0 ] && grep -q "source.*zlib=1:1.3-1" "$T/apt-get-calls"'
check 'and arrives whole' 'cmp -s "$T/archive/zlib_1.3-1.dsc" "$libraries/zlib_1.3-1.dsc" && [ -f "$libraries/zlib_1.3.orig.tar.xz" ]'

: >"$libraries/stray.tar.xz"
fetch
check 'one that'"'"'s here with the right hashes is reused' '[ $? -eq 0 ] && [ ! -s "$T/apt-get-calls" ]'
check 'and it says so' 'grep -q "reusing zlib=1:1.3-1" "$T/said"'
check 'a file no package lists is removed' '[ ! -e "$libraries/stray.tar.xz" ]'

printf 'changed\n' >"$libraries/zlib_1.3.orig.tar.xz"
fetch
check 'a listed file with the wrong hash means a download' '[ $? -eq 0 ] && [ -s "$T/apt-get-calls" ]'
check 'which puts the right one back' 'cmp -s "$T/archive/zlib_1.3.orig.tar.xz" "$libraries/zlib_1.3.orig.tar.xz"'

printf 'changed\n' >>"$libraries/zlib_1.3-1.dsc"
FAIL=connect fetch
check 'a .dsc that isn'"'"'t the archive'"'"'s isn'"'"'t trusted' '[ $? -ne 0 ] && [ -s "$T/apt-get-calls" ]'
check 'and an archive that can'"'"'t be reached is called that' 'grep -q "couldn'"'"'t reach the archive for zlib=1:1.3-1" "$T/said"'
check 'not a version that'"'"'s gone' '! grep -q "any more" "$T/said"'

rm -rf "$libraries"
FAIL=gone fetch
check 'a version the archive dropped is called that' '[ $? -ne 0 ] && grep -q "the archive has no zlib=1:1.3-1 any more" "$T/said"'

exit "$failed"
