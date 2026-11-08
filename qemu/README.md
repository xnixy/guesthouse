# The QEMU patch

Guesthouse needs some changes to QEMU. They're in `qemu-gpupv.patch`, on top of QEMU 11.1.3 (commit `005c61c0ef6f`).

For a Windows guest, QEMU knows nothing about any GPU. It offers the guest the devices and VMBus channels it expects on Hyper-V, and passes every packet to a separate daemon over a socket.

For a Linux guest the patch adds a device, `nvfwd`, in `hw/misc/nvfwd`. It forwards the calls of NVIDIA's Linux driver to the host's. `linux/README.md` says how that works.

Two more changes are for a Linux guest on an AMD card. QEMU's D-Bus display reads the guest's GL frames back into shared memory for a viewer that takes no dmabufs, which the app's viewer doesn't, and `-vga virtio` makes the GL device on a GL display, in the VGA's place on the bus.

For an AMD card, QEMU shows the guest a virtio GPU on the host's card, through virglrenderer. The distros' virglrenderer has no AMD native context, so it's built from upstream's source first, with native context for AMD and Venus. It needs libdrm 2.4.121 or newer, libepoxy and Mesa's GBM.

```sh
git clone --depth 1 --branch virglrenderer-1.3.0 https://gitlab.freedesktop.org/virgl/virglrenderer.git
meson setup virgl-build virglrenderer --prefix="$PWD/virgl" --libdir=lib -Dplatforms=egl -Dvenus=true \
  -Ddrm-renderers=amdgpu-experimental -Dvideo=false -Dtests=false -Dbuildtype=release
ninja -C virgl-build install

git clone --depth 1 --branch v11.1.3 https://gitlab.com/qemu-project/qemu.git
cd qemu
git am /path/to/qemu-gpupv.patch
mkdir build && cd build
PKG_CONFIG_PATH="$PWD/../../virgl/lib/pkgconfig" ../configure --target-list=x86_64-softmmu --enable-kvm \
  --enable-slirp --enable-dbus-display --enable-pipewire --audio-drv-list=pipewire --enable-libusb \
  --disable-docs --disable-guest-agent --disable-tools --disable-werror --disable-gtk \
  --disable-sdl --enable-opengl --enable-virglrenderer --disable-spice --disable-curses \
  --disable-modules
ninja qemu-system-x86_64 hw/misc/nvfwd/nvfwd-sensord
cp hw/misc/nvfwd/nvfwd-sensord ../../virgl/libexec/virgl_render_server .
```

You'll need QEMU's usual build dependencies. A release comes with QEMU built, so this is only for building it yourself.

The patch is GPL-2.0-or-later, apart from the tables in `hw/misc/nvfwd`, which come from NVIDIA's open modules and keep NVIDIA's MIT notice.
