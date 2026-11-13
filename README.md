# Guesthouse

**Windows and Linux VMs on KVM that share the GPU my desktop is running on, with the vendor's real driver inside.**

![Godot's Forward+ renderer on Vulkan inside a Windows VM](docs/images/vulkan-in-windows.jpg)

That's Godot on Vulkan, in Windows, in a VM on Fedora. Nothing is passed through. The host keeps the card, and my desktop keeps running on it.

> **Status:** early. 0.5.1 is out. It runs well on my RTX 5090, and testers have run Windows on NVIDIA and AMD cards of their own, [listed below](#cards). AMD is newer and experimental.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/xnixy/guesthouse/main/install.sh | sh
```

You need x86_64 Linux. For a GPU in a VM you need an NVIDIA card with the open kernel modules, or an AMD card on amdgpu. Without one it still installs, with plain VMs, and tells you why. The script is meant to be read first, and [what it does](docs/install.md) is written out step by step.

### Cards

Windows with the GPU has run on these:

| Card | | How it went |
|---|---|---|
| RTX 5090 | NVIDIA, Blackwell | Works. It's the card I build on |
| RTX 4080 | NVIDIA, Ada | Works, on a tester's PC |
| RTX 3090 | NVIDIA, Ampere | Works, with host driver 620 |
| GTX 1660 SUPER | NVIDIA, Turing | Works. Forza Horizon 4 runs |
| RTX A1000 Laptop | NVIDIA, Ampere | Works. The laptop's screen is on Intel graphics, so the machine stays in SDR |
| RTX 3050 Laptop | NVIDIA, Ampere | Works, also in SDR |
| RTX A5500 Laptop | NVIDIA, Ampere | Works |
| RTX 5080 | NVIDIA, Blackwell | Works, on a tester's PC |
| Radeon 610M | AMD, RDNA2, integrated | Works. It's the graphics in my Ryzen |
| RX 9070 XT | AMD, RDNA4 | Works. Windows, 3ds Max and Superposition run |
| RX 7900 XTX | AMD, RDNA3 | Works |
| RX 6900 XT | AMD, RDNA2 | Works |
| RX 6700 XT | AMD, RDNA2 | Works |
| RX 6700 | AMD, RDNA2 | Works |
| RX 6600 XT | AMD, RDNA2 | Works |

Nobody has tried these yet. They should work, most likely first:

- Other RTX 50 and 40 series cards, desktop and laptop
- RTX 20 series
- Other RX 7000 and RX 6000 cards, and RDNA2 or RDNA3 graphics in a Ryzen
- RX 5000 cards, which use the same AMD driver package as the RX 6000s

Not handled: older AMD cards like Polaris, GTX 10 series and older NVIDIA cards, and datacentre cards like the A100.

If something doesn't work on your card, or your card isn't in the table and works, please open an issue.

## Why

A real GPU in a Windows VM on Linux usually means passing the whole card through with VFIO, so the host loses it, or paying for a datacenter card and a vGPU licence.

Hyper-V has a third way, called GPU-PV. The guest runs the vendor's normal driver and shares the host's card. It's how WSL gets its GPU. KVM doesn't have it, and I wanted it, because my one big GPU is also the one running my desktop.

So Guesthouse teaches KVM to speak Hyper-V's GPU protocol. Windows boots, finds what looks like a Hyper-V GPU, and loads the vendor's unmodified driver. The commands that driver writes run straight on the card.

## What works

**Windows on NVIDIA.** Direct3D 12 and 11, Vulkan, OpenGL, OpenCL, CUDA, NVENC and NVDEC all run on the real card, and Edge is hardware accelerated. A fresh install takes about 9 minutes from picking the ISO to a working GPU. Work runs at full speed once it reaches the card. Superposition in Direct3D 11 gets about 89% of what the same card scores on the host, and OpenGL about 54%. The Direct3D 11 gap is close to what running through a VM costs. OpenGL is the one I'm still working on, in [#122](https://github.com/xnixy/guesthouse/issues/122).

**Windows on AMD (experimental).** This uses AMD's own Windows driver, unpacked from AMD's installer. It has run on the integrated RDNA2 Radeon in my Ryzen and on testers' RX 9070 XT, RX 7900 XTX, RX 6900 XT, RX 6700 XT, RX 6700 and RX 6600 XT. The desktop, Direct3D 12 and 11, Vulkan and OpenGL work, and hardware video decode doesn't yet. [More on AMD](docs/amd.md).

**Linux guests.** These take a different route: the guest runs NVIDIA's Linux driver, and its calls are forwarded to the host's. CUDA, Vulkan, NVENC and NVDEC work at about host speed, and GNOME and Plasma run on it. On an AMD card the guest runs Mesa on the real card instead, through virtio-gpu's native context. See [`linux/`](linux/README.md).

The numbers, and how I check the picture is right, are in [performance](docs/performance.md).

**Without a window.** `guesthouse-vm` makes, installs, starts and stops machines from a terminal, so a PC with no desktop can run them. [How it works](docs/headless.md).

**Beside your own QEMU or libvirt.** The daemon can take the guest's memory as two files, so QEMU can be started by something else. [How, and what's untested](docs/standalone.md).

![The app's library, with a Windows machine and an Ubuntu machine](docs/images/app-library.jpg)

## The app

You don't need a terminal. Give the app an ISO and it installs Windows or Ubuntu by itself, with the GPU set up. If your PC has more than one card, you pick one for each machine when you make it. A machine can move to another card from the same maker later, while it's off. Another maker's card needs a new machine, as Windows is set up for one vendor's driver. A machine can also take up to three whole drives of this PC. That's in the app only, and not for Ubuntu yet.

![Windows on the real GPU in the app's viewer](docs/images/app-viewer.jpg)

In the viewer, **Ctrl+Alt+M** captures the mouse, which games need. **Ctrl+Alt+Return** goes full screen, and **Ctrl+Alt+P** shows the performance overlay.

For the best speed, run games full screen and pick Direct3D 12 when a game offers it. In a window, Direct3D 11, OpenGL and Vulkan with vsync off present through a copy that can cost up to half the frame rate.

## How it works

```
Windows guest                              Linux host
┌──────────────────────────┐               ┌──────────────────────────────┐
│ your app                 │               │ a daemon, playing the part   │
│ the vendor's driver      │               │ of Hyper-V                   │
│ dxgkrnl (GPU-PV client)  │ ── VMBus ──▶  │ the host's GPU driver        │
└──────────────────────────┘    (QEMU)     │ the card                     │
                                           └──────────────────────────────┘
```

- **A QEMU patch** gives the guest the devices and VMBus channels it expects on Hyper-V, plus the Linux guest's device.
- **A daemon** answers what Windows asks and runs its work on the card, through a backend for each vendor.
- **The app** installs and runs the machines. It also puts a display driver of mine into Windows, signed with a certificate made on your PC.

The guest's commands aren't translated. Its driver writes them and the card runs them. [ARCHITECTURE.md](ARCHITECTURE.md) has the details.

## Is it safe?

Programs in a VM get their own GPU page tables, and can't read each other's memory. Windows never talks to the host's driver, only to the daemon, which checks everything it's sent. QEMU and the daemon are jailed with Landlock and seccomp. A guest can still slow the card down for everyone, and a bug in the GPU driver is a bug in your kernel, so only run guests you trust. [The full picture](docs/security.md).

## What doesn't work yet

Quite a lot, and I'd rather you hear it from me. The [open issues](https://github.com/xnixy/guesthouse/issues) are the list. The biggest gap is how few cards have run this. If you'd like to help, run it on your card and open an issue with how it went.

Some of the bugs on the way were good ones. [Here are my favourites](docs/bugs.md).

## Working on it

You need Rust 1.88 or newer, or 1.92 for the app, and no GPU.

```sh
cargo test                       # everything but the app
cargo bench -p guesthouse-core   # the message path, in nanoseconds
cargo run -p guesthouse-app      # needs GTK 4.16 and libadwaita 1.5 development files
```

With GTK 4.14, add `--no-default-features` to leave out HDR. A plain build has no GPU backend, so add `--features guesthouse-daemon/nvidia,guesthouse-daemon/amd`. [CONTRIBUTING.md](CONTRIBUTING.md) has the rest.

## Bring your own

There are no NVIDIA, AMD or Microsoft binaries in this repo. You bring your own Windows ISO and GPU driver. A few tables and some sample code come from what they've published as open source, and [THIRD-PARTY.md](THIRD-PARTY.md) lists them. This is a personal project, and it has nothing to do with NVIDIA, AMD or Microsoft.

## Thanks

QEMU; Microsoft's dxgkrnl for Linux, the public source for how GPU-PV talks; NVIDIA's open kernel modules; Mesa, for Zink and the open AMD drivers; gVisor's nvproxy; zako-vdd, which the display driver is a fork of; Microsoft's OpenVMM; and GTK, libadwaita and the Rust crates the app is built on.

## Licence

Copyright (C) 2026 xnixy. GPL-2.0-or-later, the QEMU patch included, apart from what [THIRD-PARTY.md](THIRD-PARTY.md) lists. See [LICENSE](LICENSE).

NVIDIA, GeForce, AMD, Radeon and Windows are trademarks of their owners.
