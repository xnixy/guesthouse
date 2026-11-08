#!/bin/bash
# Says plainly which parts of this guest work.
# Usage, as root in the guest: nvfwd-check [--gpu-off-expected]
# Exit 0: everything works. 1: something failed. 2: nothing failed, but this boot has no GPU, which
# is the right result after a mismatch, and --gpu-off-expected then gives 0.
# Applications are opened through the shell, as a click in Activities does, and their windows counted.
# Whether they draw properly is for a person, through the viewer.
here=$(dirname "$(readlink -f "$0")")
. "$here/common.sh"
failed=0
apps='org.gnome.Nautilus org.gnome.TextEditor org.gnome.Settings org.gnome.Terminal firefox'
app_wait_seconds=30
gpu_test_seconds=60

pass() { echo "PASS $1"; }
fail() { echo "FAIL $1"; failed=$((failed + 1)); }
off() { echo "OFF  $1"; }
warn() { echo "WARN $1"; }
check() {  # A description, then a command that must succeed.
    local text=$1
    shift
    if "$@" > /dev/null 2>&1; then pass "$text"; else fail "$text"; fi
}

check_install() {
    local package missing=() mask
    check 'first boot finished' test -e "$done_marker"
    for package in $(grep -hv '^#' "$here"/files/packages*.txt); do
        dpkg-query -W -f '${Status}' "$package" 2>/dev/null | grep -q 'ok installed' || missing+=("$package")
    done
    if [ ${#missing[@]} = 0 ]; then pass 'all packages installed'; else fail "packages missing: ${missing[*]}"; fi
    check 'user desk exists' id desk
    check 'GPU step enabled' systemctl is-enabled nvfwd-gpu.service
    check 'desktop enabled' systemctl is-enabled nvfwd-desktop.service
    check 'starts to the graphical target' test "$(systemctl get-default)" = graphical.target
    check 'udev keeps its hands off nvidia.ko' grep -q '^blacklist nvidia$' $root/etc/modprobe.d/nvfwd.conf
    check 'nvidia-drm options set' grep -q 'fbdev=0' $root/etc/modprobe.d/nvfwd.conf
    check 'the power button powers off' grep -q '^PowerKeyIgnoreInhibited=yes$' "$root/etc/systemd/logind.conf.d/90-nvfwd.conf"
    check 'Xwayland wrapped' test -x "$root/usr/bin/Xwayland.real"
    check 'headers for this kernel' test -e "$root/lib/modules/$(uname -r)/build/Makefile"
    for mask in pipewire.socket pipewire.service wireplumber.service; do
        check "$mask masked for logins" test "$(readlink "$root/etc/systemd/user/$mask")" = /dev/null
    done
    check 'Firefox installed' test -x "$root/opt/ff/firefox/firefox"
}

# A guest installed by hand has no first boot of ours and no package list in dpkg. Its desktop is its own.
check_own_install() {
    local label
    check 'setup finished' test -e "$own_done_marker"
    check 'GPU step enabled' systemctl is-enabled nvfwd-gpu.service
    check 'desktop watch enabled' systemctl is-enabled nvfwd-desktop-watch.service
    check 'udev keeps its hands off nvidia.ko' grep -q '^blacklist nvidia$' "$root/etc/modprobe.d/nvfwd.conf"
    check 'the power button powers off' grep -q '^PowerKeyIgnoreInhibited=yes$' "$root/etc/systemd/logind.conf.d/90-nvfwd.conf"
    check 'headers for this kernel' test -e "$root/lib/modules/$(uname -r)/build/Makefile"
    if [ -e "$root/sys/fs/selinux/enforce" ]; then
        # user:role:type:level, and the type is what counts.
        label=$(stat -c %C "$root/usr/lib/nvfwd/gpu-up.sh" 2>/dev/null | cut -d: -f3)
        if [ "$label" = bin_t ]; then
            pass 'SELinux labels the GPU step a program'
        else
            fail "SELinux labels the GPU step ${label:-nothing}, not a program"
        fi
    fi
}

# Its own login screen and desktop, on the GPU, or on the plain screen and why.
check_own_desktop() {
    local where pid on_gpu=
    where=$(value_of "$state_file" desktop 2>/dev/null)
    if [ "$where" != gpu ]; then
        echo "---- desktop on the plain screen: $(value_of "$state_file" desktop_reason 2>/dev/null)"
        return 0
    fi
    echo '---- desktop on the GPU, through Zink'
    . "$here/own-desktop.sh"
    check 'session settings for the GPU in place' grep -q '^GALLIUM_DRIVER=zink$' "$session_settings"
    check 'the GPU screen is lit' gpu_screen_lit
    for pid in $(pgrep -x gnome-shell) $(pgrep -x kwin_wayland); do
        grep -q /dev/nvidia0 "$root/proc/$pid/maps" 2>/dev/null && on_gpu=1
    done
    if [ -n "$on_gpu" ]; then pass 'the compositor runs on the GPU'; else fail 'the compositor runs on the GPU'; fi
}

check_version() {
    local version=$1 module answer
    for module in $modules; do
        check "module $module loaded" test -d "$root/sys/module/$module"
    done
    check "module nvidia says $version" test "$(cat "$root/sys/module/nvidia/version" 2>/dev/null)" = "$version"
    answer=$("$here/driver-version.py" 2>&1)
    check "the host's driver says $version (it said: $answer)" test "$answer" = "$version"
    check "libcuda $version linked in" sh -c "ldconfig -p | grep -q libcuda.so.1 && test -e $driver/lib/libcuda.so.$version"
    check 'driver libraries resolve' sh -c "! ldd $driver/lib/libGLX_nvidia.so.0 $driver/lib/libnvidia-egl-wayland.so.1 $driver/lib/libnvidia-egl-gbm.so.1 2>&1 | grep -qE 'not found|GLIBC'"
}

# A test program that prints a count of wrong values.
check_compute() {
    local text=$1
    shift
    local output
    output=$(timeout "$gpu_test_seconds" "$@" 2>&1)
    if [ $? = 0 ] && echo "$output" | grep -q ' 0 wrong'; then pass "$text"; else fail "$text: $(echo "$output" | tail -n 1)"; fi
}

check_gpu() {
    local listing vulkan
    listing=$(timeout "$gpu_test_seconds" nvidia-smi -L 2>&1)
    if echo "$listing" | grep -q '^GPU 0:'; then pass "nvidia-smi: $listing"; else fail "nvidia-smi: $listing"; fi
    export __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json:/usr/share/glvnd/egl_vendor.d/10_nvidia.json
    export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/nvidia_icd.x86_64.json
    vulkan=$(timeout "$gpu_test_seconds" vulkaninfo --summary 2>&1)
    if echo "$vulkan" | grep -q 'driverName *= *NVIDIA'; then pass 'Vulkan sees the NVIDIA driver'; else fail 'Vulkan sees the NVIDIA driver'; fi
    check_compute 'Vulkan compute, every value checked' "$built/vkcompute"
    check_compute 'CUDA kernel, every value checked' "$built/cudavec"
    # Not a failure: the rest of the GPU doesn't need them.
    timeout "$gpu_test_seconds" nvidia-smi --query-gpu=temperature.gpu --format=csv,noheader 2>&1 | grep -q '^[0-9]' ||
        warn "no sensor readings, so nvidia-smi shows N/A: the host's sensor daemon isn't running, or doesn't know this driver"
}

in_session() {
    local file
    file=$root/run/user/$(id -u desk)/nvfwd-session.env
    [ -e "$file" ] || return 1
    # shellcheck disable=SC2046  # the file is key=value lines from the session
    runuser -u desk -- env $(cat "$file") "$@"
}

shell_eval() {
    in_session gdbus call --session --dest org.gnome.Shell --object-path /org/gnome/Shell \
        --method org.gnome.Shell.Eval "$1"
}

# With no ambient capabilities left, bwrap runs, so glycin loads images. See the desktop's unit.
check_shell_common() {
    local shell_pid=$1
    check 'desktop service running' systemctl is-active nvfwd-desktop.service
    if [ -z "$shell_pid" ]; then
        fail 'gnome-shell running'
        return 1
    fi
    pass 'gnome-shell running'
    check 'Power Off and Restart offered (nvfwd-session)' pgrep -x nvfwd-session
    check 'session holds no ambient capability, so images load' grep -q '^CapAmb:\s*0*$' "$root/proc/$shell_pid/status"
}

check_desktop_gpu() {
    local shell_pid=$1 maps name connector=
    maps=$(cat "$root/proc/$shell_pid/maps" 2>/dev/null)
    for name in libgallium libGLX_nvidia.so libVkLayer_zink_on_nvidia /dev/nvidia0; do
        if echo "$maps" | grep -q "$name"; then pass "gnome-shell has $name mapped"; else fail "gnome-shell has $name mapped"; fi
    done
    for name in "$root"/sys/class/drm/card[0-9]; do
        [ "$(cat "$name/device/device" 2>/dev/null)" = "$nvfwd_device" ] && connector=$name
    done
    check 'nvidia-drm has a connected screen' sh -c "grep -q '^connected' $connector-*/status"
    # Mutter keeps Xwayland's key in the runtime folder, and any client needs it.
    local authority
    authority=$(ls "$root/run/user/$(id -u desk)"/.mutter-Xwaylandauth.* 2>/dev/null | head -n 1)
    if in_session env DISPLAY=:0 XAUTHORITY="$authority" timeout 20 glxinfo -B 2>&1 | grep -q zink; then
        pass 'X11 GL through Xwayland reports Zink'
    else
        fail 'X11 GL through Xwayland reports Zink'
    fi
}

check_desktop_plain() {
    local shell_pid=$1
    if grep -q 'libGLX_nvidia\|/dev/nvidia' "$root/proc/$shell_pid/maps" 2>/dev/null; then
        fail 'plain desktop stays off the NVIDIA driver'
    else
        pass 'plain desktop stays off the NVIDIA driver'
    fi
}

app_windows() {
    shell_eval "imports.gi.Shell.AppSystem.get_default().lookup_app('$1.desktop').get_n_windows()" 2>/dev/null |
        sed -n "s/.*'\"\?\([0-9]\+\)\"\?'.*/\1/p"
}

check_apps() {
    local app waited
    for app in $apps; do
        if ! shell_eval "imports.gi.Shell.AppSystem.get_default().lookup_app('$app.desktop').activate(); 1" > /dev/null 2>&1; then
            fail "$app: the shell would not open it"
            continue
        fi
        for ((waited = 0; waited < app_wait_seconds; waited++)); do
            [ "$(app_windows "$app")" -gt 0 ] 2>/dev/null && break
            sleep 1
        done
        if [ "$waited" -lt "$app_wait_seconds" ]; then pass "$app opens a window"; else fail "$app opens no window in $app_wait_seconds s"; fi
        shell_eval "imports.gi.Shell.AppSystem.get_default().lookup_app('$app.desktop').request_quit(); 1" > /dev/null 2>&1
    done
}

# Whether the guest's scripts are the ones on the driver image, which a boot puts in place.
check_guest_files() {
    local problem
    problem=$(cat "$guest_files_problem" 2>/dev/null)
    if [ -n "$problem" ]; then
        warn "the guest's scripts weren't refreshed from the driver image: $problem"
    elif [ -e "$driver/setup/refresh.sh" ] && [ "$(guest_files_id "$driver/setup")" != "$(cat "$guest_files_marker" 2>/dev/null)" ]; then
        warn "the guest's scripts are older than the driver image's: as root, bash /opt/nv/driver/setup/refresh.sh, then reboot"
    fi
}

finish() {
    check_guest_files
    echo '----'
    if [ "$failed" != 0 ]; then
        echo "FAIL $failed parts do not work"
        exit 1
    fi
    if [ "$gpu" != on ] && [ -z "$gpu_off_expected" ]; then
        echo 'OFF  everything else works, but this boot has no GPU'
        exit 2
    fi
    echo 'PASS everything'
    exit 0
}

gpu_off_expected=
[ "${1:-}" != --gpu-off-expected ] || gpu_off_expected=1
modules='nvidia nvidia_modeset nvidia_drm'
if [ -n "$own_family" ]; then
    modules=nvidia
    check_own_install
    gpu=$(value_of "$state_file" gpu 2>/dev/null)
    if [ "$gpu" = on ]; then
        version=$(value_of "$state_file" version)
        echo "---- GPU is on, driver $version"
        [ "$(value_of "$state_file" tested)" = yes ] || warn "driver $version has never been tested with this project"
        [ "$(value_of "$state_file" desktop)" != gpu ] || modules='nvidia nvidia_modeset nvidia_drm'
        check_version "$version"
        check_gpu
        check_own_desktop
    else
        echo "---- GPU is off: $(value_of "$state_file" reason 2>/dev/null)"
        off 'nvidia-smi, CUDA and Vulkan are not checked'
    fi
    finish
fi
check_install

gpu=$(value_of "$state_file" gpu 2>/dev/null)
shell_pid=$(pgrep -o -x gnome-shell)
if [ "$gpu" = on ]; then
    version=$(value_of "$state_file" version)
    echo "---- GPU is on, driver $version"
    [ "$(value_of "$state_file" tested)" = yes ] || warn "driver $version has never been tested with this project"
    check_version "$version"
    check_gpu
    echo '---- desktop on KMS through Zink'
    check_shell_common "$shell_pid" && check_desktop_gpu "$shell_pid"
else
    echo "---- GPU is off: $(value_of "$state_file" reason 2>/dev/null)"
    off 'nvidia-smi, CUDA, Vulkan and KMS are not checked'
    echo '---- plain desktop'
    check_shell_common "$shell_pid" && check_desktop_plain "$shell_pid"
fi
echo '---- applications, as Activities opens them'
if [ -n "$shell_pid" ]; then check_apps; else fail 'no shell to open applications in'; fi

finish
