#!/bin/sh
# Builds Microsoft's dxgkrnl, the guest's half of GPU-PV, for the running kernel, and installs it.
# A distro's kernel doesn't have it, as it's only in Microsoft's WSL kernel. Its source is fetched
# from that kernel on GitHub, at a commit named below, and never kept in Guesthouse, and it's under
# the GPL 2. gpu.sh runs this again whenever the running kernel has no dxgkrnl, as after a kernel
# update.
set -eu
kernel=$(uname -r)
here=$(dirname "$0")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends build-essential git "linux-headers-$kernel"

major=$(echo "$kernel" | cut -d. -f1)
minor=$(echo "$kernel" | cut -d. -f2)
# Microsoft's 6.18 branch builds on 6.18 and newer as it is. Its 6.6 branch needs eventfd_signal's
# newer form from 6.8 on.
if [ "$major" -gt 6 ] || { [ "$major" -eq 6 ] && [ "$minor" -ge 18 ]; }; then
    branch=linux-msft-wsl-6.18.y
    commit=4ee82e228f384c4335f6b2048af72d6956f74c21
elif [ "$major" -eq 6 ] && [ "$minor" -ge 6 ]; then
    branch=linux-msft-wsl-6.6.y
    commit=a07f9ea8a99139913acbcc1c160b132cb2a49c81
else
    echo "Kernel $kernel is older than any dxgkrnl this builds."
    exit 1
fi

# The source is the one commit named above, which a newer commit of the branch doesn't change.
git init --quiet "$work/wsl"
git -C "$work/wsl" remote add origin https://github.com/microsoft/WSL2-Linux-Kernel.git
git -C "$work/wsl" sparse-checkout set --no-cone /drivers/hv/dxgkrnl/ /include/uapi/misc/d3dkmthk.h
git -C "$work/wsl" fetch --quiet --depth 1 --filter=blob:none origin "$commit"
git -C "$work/wsl" checkout --quiet FETCH_HEAD
[ "$(git -C "$work/wsl" rev-parse HEAD)" = "$commit" ] || {
    echo "What was fetched for $branch isn't commit $commit."
    exit 1
}

source=$work/source
mkdir -p "$source/include/uapi/misc"
cp "$work/wsl/drivers/hv/dxgkrnl/"*.c "$work/wsl/drivers/hv/dxgkrnl/"*.h "$source/"
cp "$work/wsl/include/uapi/misc/d3dkmthk.h" "$source/include/uapi/misc/"
cp "$here/Kbuild" "$here/guesthouse-compat.h" "$source/"
if [ "$branch" = linux-msft-wsl-6.6.y ] && [ "$minor" -ge 8 ]; then
    sed -i 's/eventfd_signal(\([^,]*\), 1)/eventfd_signal(\1)/' "$source"/*.c
fi

make -C "/lib/modules/$kernel/build" M="$source" modules
install -D -m 0644 "$source/dxgkrnl.ko" "/lib/modules/$kernel/updates/dxgkrnl.ko"
depmod -a "$kernel"
