# Running the GPU beside your own QEMU or libvirt

Guesthouse starts the daemon and QEMU for you. If you'd rather start them yourself, from a script or from libvirt, you can. The daemon can take the guest's memory as two files you name, and `guesthouse-vm command` prints the words to start a machine this way.

I've started the daemon on its memory files, with a pretend GPU behind it, and started the patched QEMU with the GPU's devices beside it. QEMU connected to both sockets, and both had the same two files open. I haven't run a Windows guest with the real GPU this way yet, and I don't test it with each release. Treat this page as a map, and expect to find things I missed.

## The pieces

- **A patched QEMU.** QEMU knows nothing about this GPU. My patch adds the VMBus devices below. A release comes with it built, at `qemu/build/qemu-system-x86_64` in the release folder. To build it yourself, see [the QEMU patch](../qemu/README.md). A distro's QEMU won't do.
- **The daemon, `guesthouse-daemon`.** One per machine. It makes two sockets in `BASE/run`, or `BASE/run-N` for slot N: `vgpu.sock` for the GPU's channel and `global.sock` for the one the guest's graphics kernel looks for first. QEMU connects to both as a client, so the daemon has to be up first. It reads what setup wrote to the store (`setup/setup.json`), so run setup once with Guesthouse.
- **Shared memory.** The guest's RAM and a 64 GiB window that holds the GPU's IO space. QEMU and the daemon map the same two files. The app makes them as memfds and hands them to both. Here you name two files instead.
- **VMBus devices.** `vmbus-bridge` is the bus, and `hv-timesync` keeps Windows' clock on the host's. `gpupv-global` and `gpupv-vgpu` are the two channels, and `gpupv-render` is the render device that shows Windows its GPU. The instance ids are fixed, and the guest looks for them: `711dad3a-73ce-468b-90a9-ede6906841b2` for the global channel, and `7a1e2f30-4c55-4d6e-9f10-0123456789ab` for the GPU's, which `gpupv-render` names in its `channel=`.
- **Hyper-V CPU flags.** Windows takes the machine for a Hyper-V guest, and the VMBus devices need it to. The `-cpu host,hv-...` word has them. With a TPM it also turns off `svm` and `vmx`, so Windows can't put its own hypervisor between the guest and QEMU.

## The memory files

Two variables on the daemon, both or neither:

| Variable | What |
|---|---|
| `GUESTHOUSE_RAM_FILE` | The guest's RAM. As big as `GUESTHOUSE_RAM`, which is `16G` if you don't set it. |
| `GUESTHOUSE_WINDOW_FILE` | The window. Always 64 GiB. |

Give full paths. QEMU opens the same two with `memory-backend-file,share=on`, as the printed command shows.

The daemon makes a file that isn't there, private to you (mode 600). For the RAM file it also takes all of the room at once, as the app does with a memfd. RAM fills as the guest touches it, and by the time a PC that is short would show it, the GPU has pinned what it has and nothing can be swapped to make room. This way a start either has the memory or doesn't start. The window is only sized: it fills as far as the guest's locked allocations go.

A file that is already there is taken as it is, if it's a regular file, yours, closed to other users, and exactly the right size. Anything else is refused, with the reason. A file from an earlier run is fine, and a link is not followed.

What you have to provide:

- Put both files on `/dev/shm`, or another tmpfs. Not hugetlbfs: the daemon maps single pages of the RAM file, and hugetlbfs wants whole huge pages. Not a disk file either. It works, but the guest's memory would go through the page cache and out to the disk.
- Leave room. `/dev/shm` is half your RAM by default, and the RAM file takes its size from it at once. `sudo mount -o remount,size=20G /dev/shm` makes it bigger. The window only takes what the guest uses of it.
- Remove the files when both processes have stopped. They stay until you do, and the RAM file keeps its memory. It also still holds what the guest had in memory, which is one more reason.

The daemon puts its jail on after it has opened them, so the jail needs no rule for either path.

## Starting it

```
guesthouse-vm command NAME
```

prints what to run for a machine, as a script: the run folder, the virtual TPM if it has one, the daemon with its environment, a wait for both sockets, QEMU, and the stop. It starts nothing and changes nothing. If the PC lacks something a start needs, such as the GPU part, it says so in a note and prints anyway. It's for Windows machines and plain ones. For an Ubuntu machine, or a plain one with the GPU for Linux, it refuses, since the forwarded GPU starts differently and I haven't done that.

The order matters:

1. Start the daemon, and wait until `vgpu.sock` and `global.sock` both exist. Remove old ones first, or an old socket passes for a ready daemon.
2. Start QEMU. It connects to both.
3. To stop, shut Windows down, wait until QEMU has gone, then stop the daemon with `kill`, and then remove the memory files. QEMU goes first, since the guest's GPU work goes through the daemon.

The environment of the daemon, besides the two files:

| Variable | What |
|---|---|
| `GUESTHOUSE_SLOT` | 0 to 3. Names the run folder, `run` or `run-N`. |
| `GUESTHOUSE_RAM` | The guest's RAM, like `16G`. It has to match QEMU's `-m` and the RAM backend's size. |
| `GUESTHOUSE_VRAM` | The most video memory the guest may have, like `8G`. Unset is no cap. |
| `GUESTHOUSE_STORE` | The data store, if it isn't the default. |
| `GUESTHOUSE_AMD_CARD` | The PCI address of the AMD card, for a machine on AMD. |

## Which of QEMU's words are yours

The printed list is about 90 words. In groups:

- **The guest needs these.** `q35` with `smm=on` and the secure-boot flash drives, the Hyper-V CPU flags, the clock words (`-rtc`, `kvm-pit`), the VGA and its memory size, which the firmware picks the screen mode from, the disk on AHCI as the first boot device, and every GPU word: the VMBus devices, the two sockets, the two memory backends and `-machine memory-backend=ram0`. Keep the devices in the printed order. Windows was installed with them in those places, and moving them makes it see new hardware.
- **These are yours.** `-name`, `-smp`, the paths of the disk and the firmware, the network (`-nic` and the port forward to the agent), the sound, the serial log, the debug log, the TPM, and the relative USB mouse. `-m` and the RAM backend's size are yours too, if they agree with `GUESTHOUSE_RAM` and with each other.
- **Only the app uses these.** `-display dbus,p2p=yes` and the four `-qmp` sockets are how the app's viewer and its controls reach QEMU. Without the app you can drop them. Keep one `-qmp` of your own to talk to QEMU.

That's my reading of it. I haven't taken words out one at a time to see what breaks.

The printed command runs QEMU unjailed, with its sockets as paths. The app puts QEMU in a jail ([security](security.md)) and hands it its sockets open. In this mode that is yours to do. A QEMU that a guest has taken over can do anything your user can.

## libvirt

This is a sketch, and I haven't run it. It has the shape, and `guesthouse-vm command NAME` has the words.

libvirt starts QEMU as a user. The daemon makes its files and sockets private to its own user, and QEMU has to be the same user to open them. So use a session connection, `qemu:///session`, which runs QEMU as you. A system connection runs it as `qemu` or `libvirt-qemu`, and then it can't open them.

```xml
<domain type="kvm" xmlns:qemu="http://libvirt.org/schemas/domain/qemu/1.0">
  <name>windows-gpu</name>
  <memory unit="GiB">16</memory>
  <vcpu>8</vcpu>
  <os firmware="efi">
    <type arch="x86_64" machine="q35">hvm</type>
    <firmware><feature enabled="yes" name="secure-boot"/></firmware>
  </os>
  <features>
    <smm state="on"/>
    <hyperv mode="custom">
      <relaxed state="on"/>
      <vapic state="on"/>
      <spinlocks state="on" retries="8191"/>
      <vpindex state="on"/>
      <runtime state="on"/>
      <synic state="on"/>
      <stimer state="on"><direct state="on"/></stimer>
      <frequencies state="on"/>
      <reset state="on"/>
      <ipi state="on"/>
      <tlbflush state="on"/>
    </hyperv>
  </features>
  <cpu mode="host-passthrough"/>
  <clock offset="utc">
    <timer name="hypervclock" present="yes"/>
  </clock>
  <devices>
    <emulator>/home/you/.local/lib/guesthouse/current/qemu/build/qemu-system-x86_64</emulator>
    <disk type="file" device="disk">
      <driver name="qemu" type="qcow2"/>
      <source file="/home/you/.local/share/guesthouse/machines/win/disk.qcow2"/>
      <target dev="sda" bus="sata"/>
      <boot order="1"/>
    </disk>
  </devices>
  <qemu:commandline>
    <qemu:arg value="-device"/><qemu:arg value="vmbus-bridge,mmio-base=0x10000000000,mmio-size=0x1000000000"/>
    <qemu:arg value="-device"/><qemu:arg value="hv-timesync"/>
    <qemu:arg value="-chardev"/><qemu:arg value="socket,id=gpg,path=/run/user/1000/guesthouse/run/global.sock,reconnect-ms=500"/>
    <qemu:arg value="-object"/><qemu:arg value="memory-backend-file,id=gpio,size=64G,mem-path=/dev/shm/gpupv-window.standalone,share=on"/>
    <qemu:arg value="-device"/><qemu:arg value="gpupv-global,chardev=gpg,instanceid=711dad3a-73ce-468b-90a9-ede6906841b2,iomem=gpio,iomem-base=0x10000000000"/>
    <qemu:arg value="-chardev"/><qemu:arg value="socket,id=gpv,path=/run/user/1000/guesthouse/run/vgpu.sock,reconnect-ms=500"/>
    <qemu:arg value="-device"/><qemu:arg value="gpupv-vgpu,chardev=gpv,instanceid=7a1e2f30-4c55-4d6e-9f10-0123456789ab"/>
    <qemu:arg value="-object"/><qemu:arg value="memory-backend-file,id=ram0,size=16G,mem-path=/dev/shm/gpupv-ram.standalone,share=on"/>
    <qemu:arg value="-machine"/><qemu:arg value="memory-backend=ram0"/>
    <qemu:arg value="-device"/><qemu:arg value="gpupv-render,channel=7a1e2f30-4c55-4d6e-9f10-0123456789ab,hostcaps=1"/>
  </qemu:commandline>
</domain>
```

What's missing from it, and what I'm unsure of:

- `mmio-base` and `iomem-base` are where the window sits in the guest's address space: `0x10000000000` on a CPU that addresses more than 40 bits, and lower on one that addresses 40 or fewer, such as 39 bits, where it is `0x4000000000`. The daemon works it out the same way, so copy the number `guesthouse-vm command` prints for your PC.
- It has no VGA, no TPM, no network and no USB. Take them from the printed list, in order. Windows was installed with them.
- libvirt makes a RAM object of its own for the machine, and I don't know if `-machine memory-backend=ram0` reliably wins over it. If it doesn't, the guest's RAM isn't the file, and the GPU won't work.
- libvirt can add QEMU's own `-sandbox`. I haven't tried the devices under it.
- The sockets and the memory files have to be there before the domain starts, so the daemon is started first, outside libvirt, as above. A hook that starts it is yours to write.

## What you skip

Guesthouse does more than start two processes. After Windows answers, the app:

- waits for the desktop, and checks that Windows has a working GPU;
- decides which vendor ID the render device shows, and restarts Windows once if that changed. The printed command shows the one the app last left for the machine;
- sets the desktop's size and refresh rate on the display driver at each boot. The refresh rate doesn't survive a restart of Windows;
- connects the shared folder, if the machine has one;
- puts NVIDIA's driver files and settings store into Windows when they're missing.

None of that happens on this route. A machine Guesthouse has started before keeps what it put in Windows, so it should come up the same. A change you make to the machine, such as a new size, isn't applied.

The app stops a machine by asking Windows to shut down through its agent. Here use QEMU's monitor: `system_powerdown` is the power button, and Windows shuts down on it.

## The limits

- **Make the machine with Guesthouse first.** Its install puts the GPU driver files, the display driver and the agent into Windows, and I haven't written a way to do that by hand. Make the machine, install it, and let it start once in the app. Then its disk, its firmware file and its TPM folder, which are in its folder in the store, can be run from elsewhere.
- **There's no viewer.** The picture goes to the app's viewer, which this route doesn't have. Use Remote Desktop, or Sunshine on NVIDIA. [Without a window](headless.md) says how to reach the guest.
- **QEMU's jail is yours.** See above.
- **A drive of the host's is open to everyone.** The app opens each drive it gives a machine exclusively, so the kernel refuses to mount it while the machine runs. The printed command opens it by its name under `/dev/disk/by-id` instead, and nothing stops a mount. Don't mount a drive while QEMU has it.
- **The agent's token** is in the machine's folder, `guest.token`. Only the app makes requests of the agent, so nothing here needs it.
- **It isn't tested every release.** The daemon's jail and the memory files are, in the test suite, and `guesthouse-vm command` is tested for the words it prints. A whole GPU machine on this route isn't.

## Linux guests

A Linux guest doesn't use any of this. On an AMD card, it's upstream virtio-gpu with native context: the patched QEMU has a few changes for it, and [the QEMU patch](../qemu/README.md) and [AMD](amd.md) say what. On NVIDIA it's the `nvfwd` device, which [`linux/README.md`](../linux/README.md) describes. Neither is printed by `guesthouse-vm command` yet.

If you're coming from GPU passthrough with a Windows you already have, I haven't tried that. That guest has NVIDIA's own driver on a PCI card, and my install expects to put its own in.
