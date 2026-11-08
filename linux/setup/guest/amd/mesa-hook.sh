#!/bin/bash
# Run by the package manager after it changes packages. A new Mesa from the distro has no native
# context, so on a guest on an AMD card it's built again, in the background, once the package manager
# is done. It never fails, as that would fail the package manager's own work.
here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
. "$here/../common.sh"
. "$here/amd-common.sh"
[ -n "$(amd_mode)" ] || exit 0
mesa_is_ours && exit 0
# The service checks it first, as a distro's new Mesa may need no build.
systemctl start --no-block nvfwd-mesa-build.service 2>/dev/null
exit 0
