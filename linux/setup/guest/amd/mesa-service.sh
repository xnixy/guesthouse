#!/bin/bash
# What nvfwd-mesa-build.service runs: the Mesa build, once the package manager that started it has
# finished, and then the check of what it gave. It's started at boot when this Mesa has no native
# context, and by the package manager's hook when the distro's Mesa is updated.
set -uo pipefail
here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
. "$here/../common.sh"
. "$here/amd-common.sh"
# A package manager that runs this hook is still running, and holds its lock until it's done.
package_managers='apt apt-get dpkg dnf dnf5 rpm pacman zypper unattended-upgr'
lock_wait_seconds=3600

package_manager_busy() {
    local name
    for name in $package_managers; do
        pgrep -x "$name" > /dev/null && return 0
    done
    return 1
}

wait_for_package_manager() {
    local waited=0
    while package_manager_busy && [ "$waited" -lt "$lock_wait_seconds" ]; do
        sleep 5
        waited=$((waited + 5))
    done
}

mode=$(amd_mode)
[ -n "$mode" ] || exit 0
wait_for_package_manager
# The distro's Mesa may need nothing: a native context that works now has its say, and so does one
# whose Mesa was built here or needs no build.
if mesa_is_ours || mesa_has_native_context || { [ "$mode" = native ] && is_native_gl "$(gl_renderer)"; }; then
    [ "$mode" = native ] && report_renderer native
    exit 0
fi
if ! "$here/mesa-build.sh"; then
    reason=$(cat "$amd_state/build-failure" 2>/dev/null)
    say_amd "failed ${reason:-the Mesa build failed}, see $mesa_log"
    exit 0
fi
# On Venus, the new Mesa waits for a start on native context, which the host gives it next.
[ "$mode" = native ] || exit 0
report_renderer native || exit 0
# Ubuntu's own desktop starts again on the new Mesa. A desktop someone installed is theirs to restart.
if [ -z "$own_family" ] && systemctl -q is-active nvfwd-desktop.service; then
    systemctl restart nvfwd-desktop.service
fi
