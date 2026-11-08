# Linux guests

A Linux guest doesn't use the Windows route. It runs NVIDIA's own Linux user-mode driver, the same files as the host, and a QEMU device forwards its kernel calls to the host's `/dev/nvidia*`. Ubuntu 26.04 installs by itself, and a Linux you install by hand can have it too.

The app does the host's side of this: the install, the driver image, the QEMU arguments and the viewer. This folder is what goes into a guest, and the device itself is in `qemu/qemu-gpupv.patch`.

## How a call gets through

- **`guest/`** is a kernel module named `nvidia`, with real `/dev/nvidiactl`, `/dev/nvidia0` and `/dev/nvidia-uvm` nodes. It copies a call and everything the call points to into a slot, and says where the pointers and files are.
- **The device, `nvfwd`,** takes the call on the vCPU that made it. It copies the call out of the slot once and works only on that copy. It checks the call against `allow.txt`, finds the pointer and file fields for itself, fills in host values, clears any the guest left alone, and makes the ioctl.
- **A mapping** goes over part of a 64 GiB window in guest physical space. KVM's notifier does the rest, so no memory slot changes per mapping. UVM has to keep the guest's own address, so a UVM mapping gets a memory slot of its own.
- **`kapi/`** stands in for `nvidia-modeset`, under NVIDIA's own `nvidia-drm`. There's one virtual monitor, and a flip goes to the viewer. No display class reaches the host.
- **`layer/`** has two Vulkan layers. `wsi_*.c` answers surface calls, as NVIDIA's own WSI wants its X driver: a Vulkan app on Wayland turns it on with `ENABLE_NVFWD_WSI=1`. `zink_on_nvidia.c` corrects a few things for a compositor on Zink.
- **`cast/`** carries the viewer's keys and pointer into the session, and has a GLX vendor and the Xwayland wrapper for X11 clients, and for a Linux installed by hand an EGL vendor that does the wrapper's job. `session_manager.c` answers for gnome-session, which Ubuntu's desktop here doesn't run, so the shell's menu has Power Off and Restart.
- **`setup/`** is the guest's side of the install: the seed, first boot, and `gpu-up.sh`, which brings the GPU up on each boot or turns it off and says why.

The card's sensor readings go through `nvfwd-sensord`, a separate program. It keeps the driver's sensor page and copies the parts a guest may see into a page of its own, so `nvidia-smi` shows real readings and QEMU holds no host client while the guest runs. If the daemon can't start, or doesn't know the driver, or dies, the guest keeps the GPU and `nvidia-smi` shows N/A. The VM's `calls.log` says why.

## Which card

The device is told the card setup chose: `card=` is its PCI address and `card-minor=` its device file. Before the guest runs, it asks the RM which GPU and device instance are at that address, as the daemon does for Windows, and keeps the card's file open. Whichever GPU file the guest opens, it gets that card's. The answers that list GPUs and device numbers are cut down to that card, which the guest sees as its device 0, and a call naming any other GPU is refused. The sensor daemon is started for the same card. The jail lets QEMU open that card's file and render node, and no other card's. On a PC with one card that changes nothing. It has only been tried with pretend cards.

## Trying it

You need an NVIDIA card with the open kernel modules loaded, and the driver version in `TESTED_VERSIONS` in `crates/app-core/src/linux/driver_versions.rs`. The host's side doesn't ask a package manager: it finds the driver through the loader. I've only run it on Fedora. Debian, Ubuntu and Arch layouts are tested with fixtures and not on real machines. You also need `bwrap`, `genisoimage`, `qemu-img` and `swtpm`. It has only been tried on an AMD CPU.

1. Build QEMU as `qemu/README.md` says, and copy `hw/misc/nvfwd/nvfwd-sensord` next to `qemu-system-x86_64`.
2. `cargo build --release`, and `cargo build --release -p guesthouse-app` for the viewer.
3. Get NVIDIA's source at the loaded driver's version, into `src/open-gpu-kernel-modules` in the data store, which is `~/.local/share/guesthouse`, or say where it is with `NVIDIA_SOURCE`. The guest's modules build against its headers, which aren't in this repository.
4. Set `GUESTHOUSE_PROGRAM` to this repository and `QEMU` to your build, and make an Ubuntu machine from Ubuntu's server ISO.

The install needs no hands, and the first boot builds everything. If anything about the driver is in doubt the GPU stays off and the VM still boots. The guest gets one plain sentence and the app has the details, as those can hold your paths. As root in the guest, `nvfwd-check` says which parts work.

## A Linux you installed yourself

Any ISO the app doesn't install by itself makes a plain machine, which you install by hand in the viewer. If it's a Linux, its settings have a Linux GPU switch. With it on, the machine starts with the same device, driver image and jail as Ubuntu's, on its own firmware and disk, and with the GPU off it boots as it always did.

Once, as root in the guest:

    mount -o ro LABEL=NVFWD_DRIVER /mnt
    bash /mnt/setup/setup-own.sh

then reboot. `setup-own.sh` reads `/etc/os-release` and uses dnf, apt, pacman or zypper, so Fedora, Debian, Ubuntu, Arch, openSUSE and the distributions built on them. It refuses anything else, an image-based system like Silverblue, and Secure Boot. It installs the running kernel's headers, a compiler and what our programs need, builds our module and the stand-in for nvidia-modeset, and installs `nvfwd-gpu.service`, which brings the GPU up at each boot as in Ubuntu. With SELinux on, the scripts are labelled as programs and the image is mounted as libraries. It never turns SELinux off. Run it again after updating the app.

A new kernel needs a new build. Each boot builds for its kernel if it has to, and a hook builds as the kernel arrives, so the first boot doesn't wait: kernel-install on Fedora, the kernel's postinst and header_postinst hooks on Debian and Ubuntu, a pacman hook on Arch. openSUSE has no hook yet, so its first boot on a new kernel takes a minute or two longer. There's no DKMS: what's built depends on the host's driver as much as on the kernel, and the boot already rebuilds for either.

With GDM, the login screen and your desktop go on the GPU too, the way Ubuntu's does: nvidia-drm on the stand-in, and GNOME on Zink over NVIDIA's Vulkan driver. That covers Fedora Workstation, Debian and Ubuntu with GNOME. Plasma goes on the GPU the same way, with KWin told which card is the GPU's, behind Plasma Login (Fedora's KDE) or SDDM's Wayland login screen. The settings live in `/run/environment.d` for that boot only, so a boot without the GPU gets plain Mesa on the plain screen. If the login screen doesn't light the GPU's screen within 90 seconds, the guest boots again on the plain screen, and later boots with the same driver and kernel start there. Delete `/var/lib/nvfwd/desktop-failed` to try again, or put `desktop=plain` in `/etc/nvfwd/own-install.conf` to keep it plain. Any other display manager, and SDDM with its X11 login screen, stays on the plain screen for now, and there the GPU is for programs: CUDA, Vulkan, NVENC and NVDEC, `nvidia-smi`. On the GPU's desktop, X11 programs get the GPU too, through Xwayland on Zink as in Ubuntu, and a Vulkan program on Wayland needs `ENABLE_NVFWD_WSI=1` to show a window.

The viewer shows the GPU's frames once there are some, and the plain screen until then or when the guest says its desktop is there. The keyboard and mouse are QEMU's either way, and a Linux machine has the relative mouse that capturing it needs. The app's Shut Down is the VM's power button, so the setup makes that button shut down: logind acts on it, GNOME does by itself in a VM, and Plasma is told to in `/etc/xdg/powerdevilrc` unless you have one.

`nvfwd-check` says which parts work, and while the VM runs its row in the app says whether the guest brought its GPU up, and where its desktop is. Every network mode and a shared folder work with the GPU, in the same jail as Ubuntu's. A disc you put in the drive while it runs can't be read in the jail.

Like Ubuntu's, it uses the card setup chose on a PC with more than one, and gets only that card's device files: see [Which card](#which-card).

What's tested: Fedora 45 Workstation and Fedora 45 KDE Plasma, installed by hand, with SELinux enforcing. The setup, `nvfwd-check`, CUDA, Vulkan and `nvidia-smi` all passed. GDM and GNOME, and Plasma Login and Plasma, came up on the GPU in the viewer, with automatic login and without, a Vulkan window and an X11 GL window drew, and Shut Down worked. After `dnf upgrade kernel` the next boot fetched the new kernel's headers and built for it. The build also passed in Debian 13 and Arch containers against their kernel headers. Debian, Ubuntu, Arch and openSUSE haven't run in a real guest yet, and openSUSE not even in a container.

## An AMD card

This is experimental. It has only run on one integrated Radeon so far, so a discrete or older card may not work yet; if it doesn't, the guest falls back to Venus or boots without the GPU, and never the NVIDIA path. Reports from real cards are welcome.

A PC with an AMD card, built in or not, can give its 3D to a Linux guest a different way: virtio-gpu's native context. QEMU's virtio GPU passes the guest's amdgpu commands to the host's card through virglrenderer, and the guest runs Mesa's own radeonsi and RADV on it. Nothing of the host's driver goes into the guest. The settings' Card row picks the card on a PC with more than one, and a machine gets either the NVIDIA card or an AMD one, never both. A PC with no NVIDIA card gives new Linux machines its first AMD card.

The host needs nothing but the install script. The release's QEMU is built with OpenGL and carries its own virglrenderer 1.3.0, with Venus and the amdgpu renderer on. It needs libdrm 2.4.121 or newer, which Ubuntu 24.04 and the distros built on it have, and the install script says so if it's older. QEMU is told to use RADV and Mesa's EGL and nothing else, and the jail lets it open the card's render node, list `/dev/dri` and nothing more there. Its frames are read back from the card for the viewer, the same way as the plain screen's.

The guest's side is a Mesa built with `-Damdgpu-virtio=true`, and few distros ship one. Arch does, so it needs nothing. Fedora 44 and 45, Debian sid, and Ubuntu 24.04, 26.04 and 26.10 don't. So the guest builds its own distro's Mesa again, at the version it has, with the distro's own options and that one added, and installs it as the distro's packages with `guesthouse1` on the version. That takes 5 to 10 minutes with 16 cores. A hook in apt or dnf builds it again whenever the distro updates Mesa, and until it's done GL goes through virgl and Vulkan runs on the CPU. The build's progress shows in the machine's row.

- **Ubuntu** does all of this by itself on its first boot.
- **A Linux you installed yourself** runs the same `setup-own.sh` as above, from the image the machine gets, and the setup builds Mesa there and then. Fedora and Debian-based distros are covered. Arch needs no build. openSUSE has no build yet, so it goes on Venus.

If the build fails, or Mesa still isn't on the card after it, the guest says why on its serial port, the row shows it, and the next start is on Venus: Vulkan on the host's RADV through virglrenderer's render server, and GL on virgl. A guest on Venus that later builds a working Mesa goes back to native context on the start after that.

It should work on any card the host's amdgpu driver runs: GCN, Polaris and RDNA 1 to 4. Neither Mesa's virtio path nor virglrenderer looks at the generation. Older GCN cards need the host's `amdgpu.si_support` or `amdgpu.cik_support`, as radeon takes them by default. Only a Radeon 610M, the iGPU of a Ryzen 9000, has been tried.

What's tested, on that iGPU: Ubuntu 26.04 installed by the app and Fedora 45 Workstation installed by hand, SELinux enforcing. In both, radeonsi and RADV were on native context, GNOME ran on the card, and a GL test and a Vulkan render and compute test all passed, with compute as fast as on the host. Both kept it after a reboot, and a Mesa update from the distro was built again by the hook. A build made to fail moved Fedora to Venus, where the same tests passed, and a new build moved it back.

## Video

NVENC and NVDEC work in a guest. With ffmpeg, H.264, HEVC and AV1 all encode on the card and decode on the card, and the decoded frames match a software decode exactly.

NVENC needs one more control, and it won't open a session without the driver's own answer. So that one call goes to the host, and the device only forwards it at its one size. `allow.txt` has a note on each entry video added.

NVIDIA's limit on encode sessions still holds, because that answer is the driver's. This card stops at 12 on the host, and a guest stops at 12 too. They share the 12: with 8 starting on each side, 12 ran between them. `nvidia-smi` on the host doesn't count a guest's sessions, though.

Sunshine builds from its own source in a guest. Its startup check finds H.264, HEVC and AV1 through Vulkan, capturing with KMS. I haven't streamed to a client yet. Its direct NVENC path doesn't start, as CUDA gives the host's PCI address and the guest has no such card. A client also needs Sunshine's ports, and only SSH is forwarded in NAT mode.

## On a driver I haven't tried

Another point release of the same branch gets a warning and a try. In order, you'd see:

1. The app wants NVIDIA's source at your exact version, and says how to get it.
2. The tables made from that source have to match the ones in the device. If a driver moved a field the device checks, the GPU stays off, as QEMU would need a new build. They're the same at 615.71.09, 615.71.10 and 615.78.08.
3. The guest builds its modules against your driver. A change there fails the build, which turns the GPU off and names the log.
4. The guest runs CUDA and Vulkan before it calls the GPU on. A call the allow list doesn't have shows up here: the GPU goes off, the plain desktop comes up, and the VM's `calls.log` has a `REFUSED` line for it.
5. The sensor daemon only knows one exact version, so on any other `nvidia-smi` shows N/A. Everything else works.

## How far to trust a guest

A Linux guest gets the GPU the way a program of yours does. It can do what your user can do to the card, it sees whole-card sensor and memory readings, and it can slow or crash your GPU session, which takes your desktop with it. It only gets calls on the list, it can only name its own GPU clients, and QEMU runs jailed with no root. As far as I could test, it can't reach QEMU's memory or anyone else's GPU work. A bug in this device only gets QEMU's jail, but a bug in NVIDIA's driver is a bug in your kernel. Treat a guest as being as trusted as you, and don't run one you don't trust.

## Limits

Each is a setting on the device, so `-device nvfwd,...,max-vram=12G`. A value of 0 for the rate turns it off. A machine's video memory cap in the app becomes `max-vram`.

| Setting | Default | Past it |
|---|---|---|
| `max-call-rate` | 20,000 calls a second | the call waits on its own vCPU |
| `max-call-burst` | 200,000 calls | the same |
| `max-vram` | 8 GiB | out of memory, as on a full card |
| `max-sysmem` | 8 GiB | out of memory |
| `max-objects` | 65,536 | out of resources |
| `max-maps` | 7,168 | mmap fails with ENOMEM |
| `max-uvm` | 32 GiB | mmap fails with ENOMEM |

The defaults come from what a session really does. GNOME with Files and Terminal peaked at 2,902 forwarded calls a second, 920 objects and 241 mappings, and was never made to wait. A desktop with more on it will hold more, and I haven't measured one.

Vulkan needs more than a 2 GiB `max-vram`: at that value pipelines fail to create. Only memory the guest asks for is counted. Page tables and channels use video memory too, and those are held down only by the object limit. Nothing limits GPU time.

## The jail

QEMU runs in the same jail as a Windows guest's: `guesthouse-jail`, with the plan from `crates/app-core/src/vm_args/qemu_jail.rs`, and bubblewrap's namespaces around it where the PC allows them. On top of what any QEMU there gets, it can open the card's device nodes, read the driver image and the allow list, and run the sensor daemon (`vm_args/forwarding_jail.rs`). It doesn't get a pid namespace, as in one QEMU's D-Bus display can't let the viewer in, and Landlock keeps it from tracing anything outside anyway. `setarch -L` goes on before the jail, whose filter won't let QEMU change its address layout. The device makes the socket its frames go out on, so it can make sockets of its own, which a Windows guest's QEMU can't with its sound off. A PC with no Landlock starts the guest without the GPU, unless the machine may start without a jail. The device won't run as root at all.

QEMU still opens `/dev/nvidia*` itself, and its filter doesn't look at ioctl numbers. The rest of the plan is a small proxy that owns those files and passes them to QEMU over a socket. That proxy has to answer live, as a guest opens a new file for each mapping.

## Tests

`python3 -m unittest discover -s linux/setup/tests -p 'test_*.py'` needs Python and bash.

`make -C linux/tests/device QEMU=/path/to/patched/qemu BUILD=/somewhere` runs the device's checks with no GPU and no VM, under ASan and UBSan. It needs `libasan`, `libubsan` and glib's headers.

`tests/` has small programs for CUDA, Vulkan and OpenGL, and `sensorrate`, which shows that sensor calls wait for the call rate. The CUDA and Vulkan ones count wrong values, and `glbench` writes an image to compare with a run on the host.
