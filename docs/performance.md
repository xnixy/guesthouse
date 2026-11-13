# Performance

The Windows numbers here are from the Python prototype unless I say otherwise. The Linux numbers are from the device in this repo.

The NVIDIA numbers were all measured on one machine: Ryzen 9 9950X3D, RTX 5090, Fedora 45, NVIDIA 615.78.08 on the host and 617.42 in Windows. Host driver 620 works for Windows guests too, and Linux guests still need 615.

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

**How fast is a real program?** Superposition at 1080p Extreme in Direct3D 11 scores about **21,850** in Windows on 0.4.0, over five runs with no viewer open. The Windows build through Proton on the same PC's Linux host gets 24,606, so that's about 89%. The card is 98% busy, and I don't know yet where the rest goes ([#124](https://github.com/xnixy/guesthouse/issues/124)). OpenGL is further behind, about 12,400 against 22,810 on the host, so about 54% ([#122](https://github.com/xnixy/guesthouse/issues/122)).

I used to compare with the lowest 5090 on Unigine's leaderboard, around 31,000, and call it two thirds. That was other people's cards on native Windows, so it wasn't a fair comparison.

![Superposition's result in the VM: 20,912 at 1080p Extreme](images/superposition-result.jpg)

![Superposition running in the VM](images/superposition-running.jpg)

*Dumps of the guest's real screen from an earlier version, shrunk for this page. The running shot is from an earlier run at the same settings. The result screen says "GPU: NVIDIA" because the launcher can't read the card's name yet, and its RAM line is wrong: the VM had 16 GB.*

A heavy Godot test scene at 1280x720 does about 690 fps natively. In the VM the prototype does about 196 on Vulkan and 158–186 on Direct3D 12, and the Rust daemon about 240 and 333. GPU time per frame is the same as native, about 1.2 ms, so that gap isn't the GPU either.

**For the best speed, run games fullscreen, and pick Direct3D 12 where a game offers it.** In a window, Direct3D 11 and OpenGL games, and Vulkan with vsync off, present through a copy the CPU has to wait on. That can cost up to half the frame rate. Fullscreen Direct3D 11 and Direct3D 12 don't, and neither does Vulkan with vsync on. [#123](https://github.com/xnixy/guesthouse/issues/123) tracks a proper fix.

**Is the picture right?** I don't judge that by eye. These tests compare exact values, and the Vulkan and OpenGL ones check the final screen pixel by pixel:

| Test | Wrong |
|---|---|
| Direct3D 12 frames | 0 of 300 |
| Vulkan through a real swapchain | 0 of 256,000 pixels |
| OpenGL through a real window | 0 of 256,000 pixels |
| CUDA vector add | 0 of 4,194,304 values |
| H.264 decoded through ffmpeg, against a software decode | 0 of 120 frames |

Encoding through ffmpeg comes out clean too: 300 frames each of H.264, HEVC and AV1, with 0 errors, and the worst frame around 43 dB against the source. NVENC stops at the card's session limit, the same as it does natively.

### Windows on AMD

This is experimental. These numbers are from the integrated RDNA2 Radeon in the same Ryzen, with AMD's driver 32.0.21045.20006 in Windows. The same pixel tests pass there: the Direct3D 11 triangle matches WARP exactly, and Vulkan and OpenGL windows come out with 0 wrong pixels. My present test does about 63 fps on Vulkan and 35 on OpenGL, where each frame still waits on its copy to the screen. There's no hardware video decode yet. On a tester's RX 9070 XT, Superposition at 720p Low reaches 65% of the same card on the host, and 4K Optimized 72%. That's on main, in the next release. More in [AMD](amd.md).

### Linux guests

This route is in this repo, in [`linux/`](../linux/README.md), with how to try it.

Linux takes a different route. The guest runs NVIDIA's real Linux driver and its calls get forwarded to the host's driver. Ubuntu 26.04.1 installs by itself, and a Linux you install yourself can have the GPU too. So far I've tried that on Fedora 45 Workstation and Fedora 45 KDE Plasma.

`nvidia-smi`, CUDA and Vulkan work, and speed is about the same as the host. With the guest's desktop off, my compute test gets about 120 TFLOPS on Vulkan in both, and an empty CUDA launch takes about 5 µs in both. The one real gap is an empty Vulkan submit, about 54 µs against 28. NVENC and NVDEC work too, and NVIDIA's session limit holds across the host and its guests. GNOME runs on it through Zink, with Firefox, Files and Settings all opening and a terminal doing real work. On a Linux you installed yourself, Plasma does too. Godot's Windows build even runs in there through Wine on Wayland.

![nvidia-smi and a small CUDA test in a terminal on the Ubuntu guest's desktop](images/nvidia-smi-in-ubuntu.jpg)

That's `nvidia-smi` and a small CUDA test of mine, on the guest's own desktop. The load and memory it shows are for the whole card, because other VMs were using it at the time.

In theory this route isn't tied to the 5090 the way the Windows one is. It forwards the real driver's calls, so it should work on anything NVIDIA's open driver supports, from the 20 series up, as long as its tables get rebuilt for that driver version. That's only theory for now: it has run on my 5090, on driver 615.78.08, with an AMD CPU, and nothing else.

Delay is where it's behind. A simple app takes about 20 to 30 ms from key press to picture, against about 11.5 on the host, and Godot takes 40 to 100 ms, more on Vulkan than OpenGL.

An AMD card, built in or not, can give a Linux guest its 3D too, through virtio-gpu's native context: the guest runs Mesa's radeonsi and RADV on the real card. Ubuntu sets itself up for it, and a Linux you install yourself has a setup script. This is experimental: it has only run on one integrated Radeon here, so a discrete or older card may not work yet, and if it doesn't the guest falls back or boots without the GPU. Reports welcome. See [`linux/`](../linux/README.md#an-amd-card).
