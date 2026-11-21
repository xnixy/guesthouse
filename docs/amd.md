# Windows on an AMD card

This is experimental. It works on the card I have and on a few testers' cards, and there's a lot it hasn't met yet.

## What's run

My card is the integrated Radeon in a Ryzen 9 9950X3D: RDNA2, 1002:13C0, on Fedora 45's amdgpu, with AMD's Windows driver 32.0.21045.20006 in the guest.

| | |
|---|---|
| Desktop | works, on my display driver at the size and refresh you pick, and the guest sees AMD's vendor ID |
| HDR | works, where the screen has it and the machine's card draws the desktop |
| Direct3D 11 | works, the test triangle matches WARP pixel for pixel |
| Direct3D 12 | works |
| Vulkan in a window | works, 0 wrong pixels, 144 fps at 144 Hz in my present test |
| OpenGL in a window | works, 4.6 compatibility and 3.3 core, 0 wrong pixels, about 100 to 110 fps |
| Hardware video decode | not yet, it falls back to the CPU; on two newer chips it can be turned on, see [Hardware video](#hardware-video) |

Testers have run it on these too:

| Card | How it went |
|---|---|
| RX 9070 XT, RDNA4 | works: Windows, 3ds Max and Superposition |
| RX 7900 XTX, RDNA3 | installs, display driver at 1920x1080 144 Hz, all four graphics APIs start |
| RX 7800 XT, RDNA3 | works |
| RX 6900 XT, RDNA2 | works |
| RX 6700 XT, RDNA2 | works |
| RX 6700, RDNA2 | works |
| RX 6600 XT, RDNA2 | works: OpenGL games and Edge on the GPU |

On an RX 9070 XT, Superposition at 720p Low went from 21,920 to 36,903 with the speed work in 0.5.2, and 4K Optimized reaches 72% of the same card on the host. [#118](https://github.com/xnixy/guesthouse/issues/118) has the rest.

## Setting it up

1. Download AMD's Windows driver installer for your card from AMD's site. AMD ships each release in packages for different generations, so get the one that lists your card.
2. In the app, make a new Windows machine and pick the AMD card. When there's no AMD driver yet, the dialog asks for the installer and unpacks it for you. On a PC with no desktop, `guesthouse-vm new` does the same when you give it the installer with `--amd-driver`. [Without a window](headless.md) has the rest.
3. Install as usual. Windows comes up with AMD's driver.

For full speed your user has to be able to open `/dev/udmabuf`. The daemon gives the guest's memory to the card through it. On most distros it belongs to the `kvm` group, so `sudo usermod -aG kvm $USER` and a new login do it. The app's checks and the installer tell you when it can't be opened. Without it Windows still runs, only slower.

A machine made for AMD stays on AMD. It can move to another AMD card while it's off, but an NVIDIA card needs a new machine, and the other way round.

## Other AMD cards

Other RX 6000, RX 7000 and RX 9000 cards should work like the ones above. RX 5000 cards use the same driver package as the RX 6000s, but nobody has tried one. Older cards like Polaris aren't handled.

If you have an AMD card, open an issue to say how it went.

## Hardware video

Off by default, and an experiment. With `GUESTHOUSE_AMD_VIDEO=1` in the environment the app or `guesthouse-vm` starts the machine from, the daemon offers Windows the card's video engine. AMD's driver then lists its hardware decoders, and AMF finds its encoder. Without it, video is decoded and encoded on the CPU as before.

It's only offered on Phoenix and Strix chips, which are the Radeon 780M, the 890M and their siblings, and only on a chip with one video instance. The guest's driver has to be told how much video work the chip takes, and that's known for those two. With a second instance amdgpu reads more of the guest's video work than the daemon has copied, so the daemon asks amdgpu how many there are. On any other card the daemon's log says why, and the machine starts without it.

Know this before you turn it on. Video work that the engine's firmware doesn't take makes amdgpu reset the card's video ring, and a reset hits everything on the host that's using the video engine just then: a browser playing a video, a recording, another machine. A guest has done that, with two encoders opened one after the other in one process. So turn it on only on a host where that's all right.

Two more switches are a stopgap for that reset. Each is off unless set, and does nothing without the first. `GUESTHOUSE_AMD_VIDEO_CLOSE=1` has the daemon close an encode session in the card's firmware when the guest's driver says it's over, which the guest's driver doesn't do itself. `GUESTHOUSE_AMD_VIDEO_REST=1500` leaves the video engine idle for that many milliseconds before a new encode session starts, long enough for amdgpu to power it down. Turn on both, as they were run: with both, RustDesk's codec probe opened six encoders one after the other in one process without a fault, and a RustDesk session encoded 3440x1440 with `hevc_amf`. While the engine rests, nothing else of the guest's reaches the GPU.

A tester ran it on a Radeon 780M with driver 32.0.32015.12012 in the guest:

- HEVC decode through Direct3D 11, and the frames match the software decoder's. H.264 through Direct3D 11 crashes in AMD's driver inside the guest.
- `hevc_amf`, `h264_amf` and `av1_amf` encode 1080p, about 2 ms a frame.
- Each frame holds up the guest's other GPU work while the card decodes or encodes it.
- A program that looks for AMF in System32 finds it only once `amfrt64.dll` is copied there from the driver's folder.

Earlier test builds of the same work also ran AV1 decode, and sessions of a minute and more in 1440p and 4K.

## Linux guests on AMD

That's a different route, through virtio-gpu's native context, and it's in [`linux/`](../linux/README.md#an-amd-card).
