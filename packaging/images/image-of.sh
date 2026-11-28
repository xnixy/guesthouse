#!/bin/sh
# Prints the image the Dockerfile beside this script names as NAME: debian, fedora or archlinux.
#
# Usage: packaging/images/image-of.sh NAME
set -eu

name=${1:?usage: image-of.sh NAME}
here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
image=$(awk -v name="$name" '$1 == "FROM" && $3 == "AS" && $4 == name { print $2 }' "$here/Dockerfile")
[ -n "$image" ] || {
    printf 'image-of: no image is called %s.\n' "$name" >&2
    exit 1
}
printf '%s\n' "$image"
