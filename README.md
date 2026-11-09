# Guesthouse

**Windows and Linux VMs on KVM that use the same GPU my desktop is using, with NVIDIA's real driver inside.**

![Godot's Forward+ renderer on Vulkan inside a Windows VM](docs/images/vulkan-in-windows.jpg)

That's Godot's full Forward+ renderer on Vulkan, in Windows, in a VM on Fedora. Nothing is passed through. The host never gives up the card, so my GNOME desktop keeps running on it the whole time.

> **Status:** early. Version 0.2.3 is out, and everything's here, the Windows GPU part included. It's run on my PC, on one RTX 5090, and Windows has now run on a tester's RTX 4080 too. What works, what doesn't and what's next is in the [roadmap](https://github.com/xnixy/guesthouse/issues/2), which I keep up to date.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/xnixy/guesthouse/main/install.sh | sh
```

It needs x86_64 Linux. For a GPU in a VM you need an NVIDIA card with the open kernel modules, and so far only my RTX 5090 and a tester's RTX 4080 have run this. The 40, 20 and 16 series get experimental support. On anything else it installs with plain VMs only, and says why. [What the installer does](#what-the-installer-does) has the details.

## Why I'm building this

If you want a proper GPU in a Windows VM on Linux, you normally get two options. You pass the whole card through with VFIO and the host loses it, or you buy a datacenter card and a vGPU licence.

Hyper-V has a third way, called GPU-PV. The guest runs the vendor's normal driver and shares the host's card. It's the same plumbing WSL uses. KVM doesn't have it, and I wanted it, because my one big GPU is also the one running my desktop.

So this project teaches KVM to speak Hyper-V's GPU protocol. Windows boots, finds what looks like a Hyper-V GPU, and loads NVIDIA's unmodified driver. After that, the commands the driver writes run directly on the card.

## Where this is at

It's all in Rust now, and the Python prototype is gone. On my PC, from a fresh clone, this repo builds QEMU from its patch, takes a Windows ISO through the app, installs Windows with nobody touching it, and boots it with the GPU working.

This repo has:

- the QEMU patch, in `qemu/`
- the NVIDIA backend, in `crates/backend-nvidia`
- the files that go into a Windows guest, in `guest/`
- the code that reads and answers the guest's messages
- the tracking of processes, allocations, fences and GPU addresses
- the line a GPU vendor's backend plugs in at, so AMD and Intel can fit in later
- a tool that replays a recorded session and compares every reply with the prototype's
- the socket that hands frames to a viewer as dmabufs
- the desktop app, in `crates/app`
- the Linux GPU route, in `linux/`, with its QEMU device in the patch
- a display driver for Windows guests, in `drivers/windows-display/`, which the app installs by itself
- an installer, `install.sh`

A Windows GPU machine needs NVIDIA's kernel module loaded, with `/dev/nvidiactl` and the card's own `/dev/nvidiaN`, and `nvidia-smi` on your PATH. It has run on my RTX 5090, with NVIDIA 615.78.08 on the host and 617.42 in Windows 11 Pro, build 26300. A tester has run Windows on an RTX 4080 with the same host driver, with a fix that's on main and not in a release yet. Nothing else has run it. With two NVIDIA cards, setup picks one and writes down its PCI address, and Windows and Linux machines open that card. The tester's PC has a 4080 and a GTX 1660 SUPER, and Windows ran on the 4080. Linux machines on a PC with two cards have only been tried on pretend cards so far.

A plain VM needs QEMU with its D-Bus display, which Fedora calls `qemu-ui-dbus`, plus `qemu-img`, `swtpm`, `genisoimage` and `7z` for installs. A QEMU built as `qemu/README.md` says has no `qemu-img` of its own, so install the system's. There are no recordings in the repo, so the replay tool only runs on a recording you make yourself.

![The app's library, with a Windows machine and an Ubuntu machine](docs/images/app-library.jpg)

![Windows on the real GPU in the app's viewer](docs/images/app-viewer.jpg)

That's the app. Windows there is on the real GPU, on my PC.

## What it does

The Windows numbers here are from the Python prototype unless I say otherwise. The Linux numbers are from the device in this repo.

Everything here was measured on one machine: Ryzen 9 9950X3D, RTX 5090, Fedora 45, NVIDIA 615.78.08 on the host and 617.42 in Windows.

### Windows guests

**The VM shows NVIDIA's vendor ID.** Windows' own GPU-PV driver only binds to a Microsoft device ID, and Edge, Chromium and streaming programs look at the vendor ID before they will use a GPU, so on Microsoft's they stay on the CPU. So Windows makes a small driver package from its own GPU-PV driver, bound to NVIDIA's vendor ID with a device ID NVIDIA has never shipped, and signed like the display driver. A new machine does this during its install. One made by an older version does it at its first start, and that start then restarts the VM once by itself, as only a new VM can show the new ID. The first boot on the new ID restarts Windows once too, as Direct3D 12 only works on it after a restart. Then the machine is ready, and Edge is hardware accelerated with no flags. A guest that won't take the package keeps Microsoft's ID and the row says why.

Direct3D 12, Direct3D 11, Vulkan and OpenGL all run on the real card, along with the desktop and normal apps. So does OpenCL. So do CUDA, NVENC and NVDEC, on the real video engines, which ffmpeg uses with H.264, HEVC and AV1. `nvidia-smi` finds the card. A fresh install takes about 9 minutes from picking the ISO to the GPU working.

**Is it really the GPU, at full speed?** Yes. This is my own benchmark, run on Linux and then inside the VM:

| Test | Linux host | Windows guest |
|---|---|---|
| Compute | 115–119 TFLOPS | 122.6 TFLOPS |
| Video memory copy | 1,064–1,072 GB/s | 1,036 GB/s |
| One dispatch with a barrier | 2.05 µs | 2.05 µs |
| Empty submission round trip | 21 µs | 26 µs, on the Rust daemon |

Once work reaches the card it runs at full speed. Getting it there costs a few µs more than on the host. The prototype took 240 µs for that round trip.

**How fast is a real program?** Superposition at 1080p Extreme in Direct3D 11 scores about **20,000** to **21,000** with no viewer open, but not every time. I've seen 18,400, and on the prototype five runs back to back in one boot fell from 21,354 to 15,332. On the Rust daemon the same test held: 20,186, 19,997, 19,487, a crash inside Superposition, then 20,054. I don't know why the prototype fell, or why it crashed. In OpenGL it's about 11,800. The lowest 5090 I could find on Unigine's leaderboard is around 31,000, so call it two thirds of what the card can do. Something is still eating the rest, and I don't know what yet.

![Superposition's result in the VM: 20,912 at 1080p Extreme](docs/images/superposition-result.jpg)

![Superposition running in the VM](docs/images/superposition-running.jpg)

*Dumps of the guest's real screen, shrunk for this page. The running shot is from an earlier run at the same settings. The result screen says "GPU: NVIDIA" because the launcher can't read the card's name yet, and its RAM line is wrong: the VM had 16 GB.*

A heavy Godot test scene at 1280x720 does about 690 fps natively. In the VM the prototype does about 196 on Vulkan and 158–186 on Direct3D 12, and the Rust daemon about 240 and 333. GPU time per frame is the same as native, about 1.2 ms, so that gap isn't the GPU either.

**For the best speed, run games fullscreen, and pick Direct3D 12 where a game offers it.** In a window, Direct3D 11 games present through a copy the CPU has to wait on, and Vulkan always does. That can cost up to half the frame rate. Fullscreen Direct3D 11 and Direct3D 12 don't take that path. Vulkan stays on it for now. A proper fix is on the [roadmap](https://github.com/xnixy/guesthouse/issues/2).

**Is the picture right?** I don't judge that by eye. These tests compare exact values, and the Vulkan and OpenGL ones check the final screen pixel by pixel:

| Test | Wrong |
|---|---|
| Direct3D 12 frames | 0 of 300 |
| Vulkan through a real swapchain | 0 of 256,000 pixels |
| OpenGL through a real window | 0 of 256,000 pixels |
| CUDA vector add | 0 of 4,194,304 values |
| H.264 decoded through ffmpeg, against a software decode | 0 of 120 frames |

Encoding through ffmpeg comes out clean too: 300 frames each of H.264, HEVC and AV1, with 0 errors, and the worst frame around 43 dB against the source. NVENC stops at the card's session limit, the same as it does natively.

### Linux guests

This route is in this repo, in [`linux/`](linux/README.md), with how to try it.

Linux takes a different route. The guest runs NVIDIA's real Linux driver and its calls get forwarded to the host's driver. Ubuntu 26.04.1 installs by itself, and a Linux you install yourself can have the GPU too. So far I've tried that on Fedora 45 Workstation and Fedora 45 KDE Plasma.

`nvidia-smi`, CUDA and Vulkan work, and speed is about the same as the host. With the guest's desktop off, my compute test gets about 120 TFLOPS on Vulkan in both, and an empty CUDA launch takes about 5 µs in both. The one real gap is an empty Vulkan submit, about 54 µs against 28. NVENC and NVDEC work too, and NVIDIA's session limit holds across the host and its guests. GNOME runs on it through Zink, with Firefox, Files and Settings all opening and a terminal doing real work. On a Linux you installed yourself, Plasma does too. Godot's Windows build even runs in there through Wine on Wayland.

![nvidia-smi and a small CUDA test in a terminal on the Ubuntu guest's desktop](docs/images/nvidia-smi-in-ubuntu.jpg)

That's `nvidia-smi` and a small CUDA test of mine, on the guest's own desktop. The load and memory it shows are for the whole card, because other VMs were using it at the time.

In theory this route isn't tied to the 5090 the way the Windows one is. It forwards the real driver's calls, so it should work on anything NVIDIA's open driver supports, from the 20 series up, as long as its tables get rebuilt for that driver version. That's only theory for now: it has run on my 5090, on driver 615.78.08, with an AMD CPU, and nothing else.

Delay is where it's behind. A simple app takes about 20 to 30 ms from key press to picture, against about 11.5 on the host, and Godot takes 40 to 100 ms, more on Vulkan than OpenGL.

An AMD card, built in or not, can give a Linux guest its 3D too, through virtio-gpu's native context: the guest runs Mesa's radeonsi and RADV on the real card. Ubuntu sets itself up for it, and a Linux you install yourself has a setup script. This is experimental: it has only run on one integrated Radeon here, so a discrete or older card may not work yet, and if it doesn't the guest falls back or boots without the GPU. Reports welcome. See [`linux/`](linux/README.md#an-amd-card).

### Keys in the viewer

- **Ctrl+Alt+M** captures the mouse, and again lets it go.
- **Ctrl+Alt+Return** goes full screen, and again comes back. So does the top edge of the screen.
- **Ctrl+Alt+P** shows or hides the performance overlay.

Capture the mouse for games. The guest then gets the mouse's own motion instead of a position on a tablet, which is what a game's mouse look needs, and the response is much better. Full screen doesn't capture it by itself. The header's mouse button does the same as the key.

## How it works

```
Windows guest                              Linux host
┌──────────────────────────┐               ┌──────────────────────────────┐
│ your app                 │               │ a daemon, playing the part   │
│ NVIDIA's Windows driver  │               │ of Hyper-V                   │
│ dxgkrnl (GPU-PV client)  │ ── VMBus ──▶  │ NVIDIA's Linux driver        │
└──────────────────────────┘    (QEMU)     │ RTX 5090                     │
                                           └──────────────────────────────┘
```

There are three pieces:

- **A QEMU patch.** The Windows part is about 900 lines, and gives the guest the devices and the VMBus channels it expects to find on Hyper-V. The Linux device in the same patch is about 4,800 lines more.
- **A daemon**, which answers everything Windows asks, sets up real GPU channels through the Linux driver, and rings their doorbells when the guest has work.
- **A desktop app**, Guesthouse, so that none of this needs a terminal. It takes a Windows ISO, installs Windows unattended and brings up the GPU. It puts my display driver in Windows too. Microsoft hasn't signed that driver, so the app signs it on each machine with a certificate it makes there, adds the certificate to the ones Windows trusts, and deletes the private key.

Apart from one small workaround, the guest's commands aren't translated. The Windows driver writes them, and the GPU runs them. Finished frames stay in video memory and reach the viewer as dmabufs.

## Keeping guests apart

A VM is only worth having if what's in it stays in it, so here's where that stands.

What's in place for Windows guests:

- **Programs in a VM can't see each other on the GPU.** Each process gets its own GPU page tables, like on real Windows. My test has one process hold a secret and another try to read and overwrite it by its GPU address. On the Rust backend it read none of the 16,384 words, in every try, and its writes left the secret whole. The same read got all 16,384 words of a process's own secret, and of one it was properly given through a shared handle, and the same write overwrote a process's own buffer, so the test does reach memory when it's allowed to. The one thing they still share is a block of fences, which only holds counters.
- **Windows never talks to the host's driver.** It talks to my daemon, and the daemon makes the calls. Sizes and ranges in every message get checked, and the parsers and handlers are fuzz tested.
- **You can cap a VM's video memory.** With a limit set, a program that goes over it loses its device. Without one, the VM is told to leave a share of the card for the host, but that's a budget and not a wall. A second GPU VM won't start until the first one has a cap.
- **QEMU and the daemon are jailed.** Each one only reaches what that VM needs: its disk, its firmware variables, its memory, its run folder and its shared folder, and for the daemon the card. Not the rest of your home folder and not another VM. The daemon has no network at all. Neither can gain privileges, even through a setuid program, or trace your other programs. It's Landlock and seccomp, which need no root, and where bubblewrap can make namespaces, QEMU gets those too. That's every network mode: passt and a bridge's helper start just before QEMU's jail goes on and hand it what it needs, and a USB device you pass to a VM is handed to QEMU open. Turn a machine's sound off in its settings, or have no PipeWire running, and QEMU gets the sockets it needs handed to it and can't open any more, so it can't reach your desktop's either. A PC that can't do the Landlock part won't start a GPU machine unless you allow that machine in its settings, and then its row says it isn't jailed.
- **The daemon's sockets are for its owner alone.** The guest agent's port is on loopback with a token per machine, so other users on the PC can reach the port but not the agent.
- **A failing program only costs itself.** A program that keeps failing on the GPU loses its own device, and the rest of the VM carries on.

What a guest can still do:

- **Get in the way.** A program can't read or change its neighbours, but trying faults the GPU, and in the one try I timed, that froze other work on the card for about four seconds, the host's included. If too many programs fault in a minute, that VM's GPU stops until Windows restarts, and the app says so.
- **See some of your hardware.** Windows sees your card's real UUID and ECID, your CPU and how much RAM you have.
- **Reach the card's driver.** The daemon keeps the card open, so a bug in it that hands a guest the daemon also hands it NVIDIA's driver, and a bug there is a bug in your kernel. Only run a Windows guest you'd trust with that.
- **Reach your audio server.** With sound, QEMU keeps its connection to PipeWire, so a QEMU taken over could in principle record from your microphone, not just play. On Ubuntu 24.04, whose Landlock can't hold back sockets and which doesn't let bubblewrap make namespaces out of the box, it could also reach your desktop's other sockets, like the session bus. The machine's row says when that's the case. Turning the machine's sound off closes both, but not for a Linux guest's QEMU, which makes the socket its picture goes out on.

What's still coming:

- **Jails for the rest.** The virtual TPM and machines with no GPU aren't jailed yet.
- **Trust a Linux guest like you trust yourself.** A Linux guest gets the GPU the way a program of yours does. It can do what your user can do to the card, it sees whole-card sensor and memory readings, and it can slow or crash your GPU session, which takes your desktop with it. It only gets calls on the list, it can only name its own GPU clients, and QEMU is in the same jail as a Windows guest's, plus the chosen card's device files, as that's how the GPU gets to the guest. As far as I could test, it can't reach QEMU's memory or anyone else's GPU work. A bug in this device only gets a guest QEMU's jail, but a bug in NVIDIA's driver is a bug in your kernel. Treat a guest as being as trusted as you, and don't run one you don't trust.

## My favourite bugs

**"The window is occluded."** Every Vulkan present failed with `PRESENT_OCCLUDED` while the window sat there in plain sight. I spent hours in Windows' visibility code. The real cause was my own daemon: Windows was sending it a copy to do, and it was refusing with "not surfaces I know". That line had been in my own log the whole time.

**1,366 allocations.** Vulkan fell over at 4K. The driver wanted a 256 MiB block in 32 MiB pieces, and my daemon kept answering "here's 192 KiB", so as far as I can tell the driver politely asked again, about 1,366 times per block. Answering with what it asked for took one run from 8,129 allocations to 374, and 4K started working.

**The blank Explorer.** WinUI apps opened with empty windows. Their frames were waiting on a CPU event that nobody ever signalled.

**The psychedelic sky.** Geometry perfect, colours from another dimension. A 10-bit image was being copied byte for byte into an 8-bit surface.

![A 10-bit image read as 8-bit](docs/images/bug-10-bit.jpg)

## What doesn't work yet

Quite a lot, and I'd rather you hear it from me. The list is in the [roadmap](https://github.com/xnixy/guesthouse/issues/2), under known problems, so it stays current without this page going stale. The biggest one: it has only run on two PCs, mine with an RTX 5090 and a tester's with an RTX 4080, and the 4080 needs a fix that isn't in a release yet. See [#9](https://github.com/xnixy/guesthouse/issues/9) if you'd like to help with other cards.

## Trying the code

You need Rust 1.88 or newer, or 1.92 for the app, and no GPU or VM.

```sh
cargo test                       # everything but the app
cargo bench -p guesthouse-core   # the message path, in nanoseconds
cargo run -p guesthouse-app       # the app, which needs GTK 4.16 and libadwaita 1.5 development files
```

With GTK 4.14, add `--no-default-features`: that leaves out HDR and nothing else. A plain `cargo build` leaves the NVIDIA backend out too: add `--features guesthouse-daemon/nvidia` to build it in.

[ARCHITECTURE.md](ARCHITECTURE.md) explains how the pieces fit, and [CONTRIBUTING.md](CONTRIBUTING.md) how to work on them.

## What the installer does

`install.sh` and `uninstall.sh` are at the top of this repo. They're plain `sh`, and they're meant to be read before you run them. Nothing runs until the last line, so a download that gets cut off does nothing. To read it first, download it, read it, and run it with `sh install.sh --dry-run`, which checks everything, shows the plan and stops.

What `install.sh` does, in order:

1. Refuses to run as root, on NixOS, on musl, and on anything but x86_64.
2. Looks at the distro, CPU and KVM, with no root.
3. Downloads one release into a folder of its own and checks its SHA-256 against the one in the script.
4. Asks the release's own `guesthouse-check` what this PC can run. That's what looks at the GPU and its driver. If the answer's no it still installs, with plain VMs only, and says why.
5. Shows you the plan and every command that needs root, word for word, and waits for you to type yes. Root is one `sudo`, for missing packages and for QEMU's bridge helper, or nothing at all if those are already done.
6. Has root put QEMU's bridge helper in `/usr/local/libexec/guesthouse`, setuid root, which bridged networking needs. It's built with the release, it's the one thing outside your home folder, and it only goes in again when a release changes it. A setuid program can't live in your home folder, where it could be swapped or given a config you wrote. It reads `/etc/qemu/bridge.conf` and drops to the one right it needs as it starts, the same as Fedora's own.
7. Puts everything else under your own folders: the program in `~/.local/lib/guesthouse/VERSION`, reached through a `current` link, plus a launcher and icons. It writes all of that in `~/.local/lib/guesthouse/manifest`.
8. Tells the desktop the icons changed. It touches `~/.local/share/icons/hicolor`, and if there's an icon cache in it already, it rebuilds that with `gtk-update-icon-cache`. It never makes a cache, and the uninstall does the same once the icons are gone.

It never installs or changes a GPU driver, kernel arguments or services. On an immutable distro it adds no packages and prints what to layer, but the bridge helper still goes in, as `/usr/local` is writable there. If bridged networking says the helper is missing or can't run as root, run the installer again: it puts it back, even when the version is the same.

A word on the checksum. It proves the download is the release the script names, and that's all. It can't protect you from the script, which is why you should read it.

To update, run the script again, or `~/.local/lib/guesthouse/current/install.sh --update`. It says what will change and refuses while one of its VMs is running. VMs of another copy of Guesthouse, like a build of the repo, don't count. The old version stays until the new one is in place, so a failed update leaves you with what you had. Your machines, disks and settings aren't touched. `--check-update` says if there's a newer release and changes nothing. If your driver has moved to another branch, it tells you what that means. GPU machines won't start until the branches match, and plain VMs still run.

To remove it, run `~/.local/lib/guesthouse/current/uninstall.sh`. It refuses while one of its VMs is running, and removes what's in the manifest and nothing else, and a file that has changed since the install stays. The bridge helper goes too, with one `sudo`, unless another install has replaced it since. Your machines stay too, unless you pass `--delete-machines` and then type the folder's name. The driver files the app copied from NVIDIA's installer stay, and it says where. Packages stay, as something else may use them, and it lists them.

I run this on Fedora. The oldest systems it's meant for are Ubuntu 24.04 and Debian 13: it needs glibc 2.39, GTK 4.14 and libadwaita 1.5, and on anything older the script stops and says why. HDR needs GTK 4.16, so Ubuntu 24.04 gets the app without it. I've installed it on those two in containers with no GPU, but not on a real desktop of either. Arch should work and openSUSE is best effort, and I haven't tried those at all.

## Bring your own

This repo has no NVIDIA or Microsoft binaries or ISOs in it. You supply your own Windows ISO and NVIDIA's driver. A few tables and some sample code come from what they've published as open source, and [THIRD-PARTY.md](THIRD-PARTY.md) lists those. This is a personal project, and it has nothing to do with NVIDIA or Microsoft.

## Thanks

None of this would exist without other people's work:

- QEMU, which this is a patch for
- Microsoft's dxgkrnl driver for Linux, the public source for how GPU-PV talks
- NVIDIA's open GPU kernel modules
- gVisor's nvproxy, the closest thing to the Linux route I've seen
- Mesa's Zink, which GNOME and Plasma run on in Linux guests
- zako-vdd, which the display driver is a fork of
- Microsoft's OpenVMM, for how Hyper-V's time sync talks
- GTK, libadwaita and the Rust crates the app is built on

## Licence

Copyright (C) 2026 xnixy

GPL-2.0-or-later, the QEMU patch included, apart from the few things [THIRD-PARTY.md](THIRD-PARTY.md) lists. See [LICENSE](LICENSE).

NVIDIA, GeForce and Windows are trademarks of their owners.
