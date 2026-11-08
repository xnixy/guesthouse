# Sourced by the AMD scripts here, after common.sh. A guest on an AMD card has a virtio GPU whose 3D
# runs on the host's card. With native context, the guest's own radeonsi and RADV drive it, which
# needs a Mesa built with amdgpu-virtio. Venus runs its Vulkan on the host's RADV instead, and needs
# nothing built. The host says which in fw_cfg.
# shellcheck shell=bash disable=SC2034,SC2154  # sourced after common.sh, which sets root
amd_mode_file=$root/sys/firmware/qemu_fw_cfg/by_name/opt/guesthouse/amd-mode/raw
amd_state=$root/var/lib/nvfwd/amd
mesa_log=$log_folder/mesa-build.log
# The package that holds RADV, by the same name in every family here. Its version says whose Mesa it is.
mesa_package=mesa-vulkan-drivers
# What the built packages' versions end in, so a Mesa from here is told from the distro's.
build_mark=guesthouse1

# native or venus, or nothing on a boot that isn't on an AMD card.
amd_mode() {
    modprobe qemu_fw_cfg 2>/dev/null
    cat "$amd_mode_file" 2>/dev/null
}

say_amd() {
    tell_host "guesthouse-amd $*"
}

# The family's own name for the distro's Mesa as it's installed now, or nothing.
mesa_version() {
    case ${own_family:-debian} in
    debian) dpkg-query -W -f '${Version}' "$mesa_package" 2>/dev/null ;;
    fedora | suse) rpm -q --qf '%{VERSION}-%{RELEASE}' "$mesa_package" 2>/dev/null ;;
    arch) pacman -Q mesa 2>/dev/null | cut -d ' ' -f 2 ;;
    esac
}

# Arch's Mesa is built with amdgpu-virtio already, so it's never built here.
mesa_has_native_context() {
    [ "${own_family:-}" = arch ]
}

mesa_is_ours() {
    case $(mesa_version) in
    *"$build_mark"*) return 0 ;;
    *) return 1 ;;
    esac
}

# The GL renderer, as a new program sees it with no display.
gl_renderer() {
    eglinfo -B -p surfaceless 2>/dev/null | sed -n 's/^OpenGL core profile renderer: //p' | head -n 1
}

# Whether this renderer is radeonsi on native context. On virgl, the renderer names the host's too.
is_native_gl() {
    case $1 in
    virgl* | zink* | llvmpipe*) return 1 ;;
    *radeonsi*) return 0 ;;
    *) return 1 ;;
    esac
}

# The first Vulkan device's name and driver, with no display.
vulkan_device() {
    vulkaninfo --summary 2>/dev/null | awk -F '= ' '
        /deviceName/ && !name { name = $2 }
        /driverName/ && !driver { driver = $2 }
        END { if (name != "") print name " (" driver ")" }'
}

# Says on the serial port how this boot's 3D went, as the mode says it should. Fails if it didn't.
report_renderer() {
    local mode=$1 gl vulkan
    gl=$(gl_renderer)
    vulkan=$(vulkan_device)
    if [ "$mode" = native ]; then
        is_native_gl "$gl" || { say_amd "failed GL is on ${gl:-nothing} and not radeonsi"; return 1; }
        case $vulkan in
        *"(radv)"*) ;;
        *) say_amd "failed Vulkan is on ${vulkan:-nothing} and not RADV"; return 1 ;;
        esac
        say_amd "native $gl, $vulkan"
    else
        case $vulkan in
        *venus*) say_amd "venus $vulkan, GL on $gl" ;;
        *) say_amd "failed Vulkan is on ${vulkan:-nothing} and not Venus"; return 1 ;;
        esac
    fi
}
