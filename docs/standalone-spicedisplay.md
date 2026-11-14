# The GPU picture from firmware to desktop, over spice, from libvirt

[standalone](standalone.md) says how to run the daemon and QEMU yourself, and sketches a libvirt domain for a Windows machine. That sketch hasn't been run. This page is a setup that has: the domain runs on a system libvirt, Windows boots with the GPU, and the picture - the firmware's boot output, the Windows boot manager, then the desktop - shows up in a spice client, on one display channel. It has been running daily on an RTX 3090 host, with QEMU 11.1 from a distro package.

It needs one more QEMU patch beside `qemu-gpupv.patch`: [qemu-spicedisplay.patch](../qemu/qemu-spicedisplay.patch). The rest of this page covers the patch, the setup, and what the display costs.

## The patch

`qemu-gpupv.patch` gives QEMU the VMBus devices the guest needs, but the machine still has no display for the GPU's picture. The GPU renders into the daemon's frame socket (`frames.sock`), and QEMU knows nothing of it. `qemu-spicedisplay.patch` adds a device, `gh-frames`, in `hw/display/gh-frames.c`, that connects to that socket and shows the pictures as a QEMU graphic console.

Apply it after the first patch, from the QEMU source:

```sh
git am /path/to/qemu-gpupv.patch
git am /path/to/qemu-spicedisplay.patch
```

It builds with the configure flags in [the QEMU patch's README](../qemu/README.md), `--disable-spice` included. The device talks to QEMU's console layer, not to spice, so any display backend carries the picture - spice, vnc, dbus, gtk.

What the device does, in short:

- **One display channel.** The console exists from realize, so display channels attach at startup. There is no video device in the domain (libvirt's `<video><model type="none"/></video>`), so this console is the only one: the client has exactly one display, and what it shows follows the machine.
- **The firmware output too.** The device registers `etc/ramfb`, the same fw_cfg contract as [hw/display/ramfb.c](https://gitlab.com/qemu-project/qemu/-/blob/master/hw/display/ramfb.c). The firmware draws its boot output into guest RAM, and the device paints it on the same console until the first GPU frame arrives. A guest reset brings it back. So the boot screen, the firmware setup, and Windows' boot manager are visible without a second display.
- **A steady 60 Hz.** The console presents on a fixed cadence and always takes the newest frame; frames in between are dropped, the way a monitor drops whatever the GPU flips while it is scanning out.
- **The cursor.** Shape and position arrive on the socket and go to the console's cursor channel, so the cursor moves at wire latency instead of frame rate.

Limits: view-only, no input injection, and no migration - the device carries no vmstate.

## The setup

The pieces beyond [standalone](standalone.md): a dedicated system user, systemd units for the daemon and swtpm, a libvirt hook, and a domain whose QEMU runs as that user.

**Why a system user.** The daemon makes its sockets and memory files private (0600), and QEMU has to open them. A `qemu:///session` connection, as [the sketch](standalone.md) suggests, runs QEMU as you and works. On a machine that runs the VM all the time, a system connection with one dedicated user is steadier: the domain survives your logout, and nothing of your own user is in QEMU's reach. The static `dac` seclabel in the domain runs QEMU as that user without relabelling.

The files in [standalone-spicedisplay/](standalone-spicedisplay/):

| File | Where it goes |
|---|---|
| [guesthouse-daemon@.service](standalone-spicedisplay/guesthouse-daemon@.service) | `/etc/systemd/system/` |
| [guesthouse-swtpm@.service](standalone-spicedisplay/guesthouse-swtpm@.service) | `/etc/systemd/system/` |
| [qemu-hook](standalone-spicedisplay/qemu-hook) | `/etc/libvirt/hooks/qemu`, chmod 755 |
| [domain-windows.xml](standalone-spicedisplay/domain-windows.xml) | your template for the machine |

The steps, with the paths the files assume (adjust them together - store `/var/lib/guesthouse`, run folder `/run/guesthouse`, user `guesthouse`):

1. Make the user, and give it the store and the run folder: `useradd -r -m -d /var/lib/guesthouse guesthouse`, and the machine's folders under it (`machines/windows-gpu`, `machines/windows-gpu/tpm`). The `kvm` and `video` groups go to the user as supplementary groups, for `/dev/kvm` and the card's render nodes.
2. Install the two unit files, `systemctl daemon-reload`. They are not enabled - the hook owns the lifecycle.
3. Install the hook. libvirt runs one `qemu` hook for every domain event, so if something else on the host already uses `/etc/libvirt/hooks/qemu`, merge the `prepare`/`release` cases in.
4. Make the machine: install Windows into a disk at `/var/lib/guesthouse/machines/windows-gpu/disk.qcow2`, from a Windows ISO on the cdrom, with the GPU words from `guesthouse-vm command windows-gpu` in place. This is the fiddly part and is outside spice's scope - [install](install.md) covers how the app does it.
5. `virsh define` the domain and start it. The hook brings the daemon (and swtpm) up, waits for the sockets, and only then does QEMU start.
6. Connect a spice client to the port in the domain, through an ssh tunnel: `ssh -L 5900:localhost:5900 host`, then `remote-viewer spice://localhost:5900`.

The daemon, QEMU and swtpm all run as the one user, and the hook takes them down when the domain stops, removing the memory files.

## What the display costs

Measured on the RTX 3090 host, Unigine Heaven at 60 fps over an ssh tunnel, QEMU 11.1, spice-server 0.16, spice-gtk 0.42 client:

| Settings | Picture | Wire |
|---|---|---|
| `streaming-video=all`, no image setting | stripes | ~15 MB/s |
| `streaming-video=all` + gstreamer vp8 | stripes | ~15 MB/s |
| `image-compression=quic` | clean, jerky | ~60 MB/s |
| **`auto_glz`, `streaming-video=off`** | **clean, smooth** | **~31 MB/s** |

- **Streams tear.** With fast full-screen updates, spice's video streams (native mjpeg, and gstreamer's vp8 alike) paint vertical displaced stripes. The domain keeps `streaming-video=off`.
- **quic stutters.** Clean picture, but the wavelet encode costs the spice worker enough to make playback uneven.
- **glz is the setting.** `image-compression=auto_glz` with streaming off was clean, smooth, and a third of the quic wire cost.
- **`jpeg-wan-compression=always` corrupts the glz path** (gray striping, displaced blocks) - keep it off.
- **h264 needs a client that can decode it.** spice-gtk 0.41 and later dropped h264, so a server that prefers h264 falls back to mjpeg silently. vp8 decodes through gstreamer's libav plugin if you ever turn streams on.
- **The cursor in a scaled client looks big.** spice-gtk draws the cursor bitmap at its pixel size and scales the desktop, so in a window smaller than the screen (virt-manager's zoom), the cursor looks oversized. At 100% zoom it is right.

## Things that bite

- **The daemon replaces a viewer, it doesn't queue.** One client on `frames.sock` at a time; a new one takes over.
- **The domain keeps QEMU's words in order.** Windows was installed with the devices where `guesthouse-vm command` prints them; moving them makes the guest see new hardware. The example domain keeps the printed order and pins the PCI slots.
- **`<video><model type="none"/>` matters.** With a video device in the domain, its console comes back as a second display channel, and the client shows two screens, one of them the firmware's idea of a VGA card.
- **The hook's socket wait is load-bearing.** A socket left over from an earlier run passes for a ready daemon; the hook removes old ones first, and fails the domain start if the daemon doesn't come up.
- **`-machine memory-backend=ram0` wins over libvirt's own RAM object.** The sketch in [standalone](standalone.md) wasn't sure; in practice it does, and the guest's RAM lands in the file the daemon maps.
