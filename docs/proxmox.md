# Proxmox VE

A Windows guest with GPU-PV can be an ordinary Proxmox VM. `qm` starts and stops it, Proxmox Backup Server backs it up, and its disks, network and USB devices are Proxmox's. It takes three things: a `pve-qemu-kvm` built with the GPU-PV devices, the daemon running beside each VM, and the GPU's words in the VM's config.

A tester has run this on one host since 20 November 2026. This page says what ran there, with the versions. Anything it doesn't name hasn't been tried.

## What it ran on

| | |
|---|---|
| Host | Proxmox VE 9.2, Debian 13, kernel `7.0.14-29-pve` |
| Card | Radeon 780M, `1002:15bf`, an integrated RDNA3 GPU |
| QEMU | `pve-qemu-kvm 11.1.3-2+gpupv1`, and since 22 November `11.1.3-4+gpupv1` |
| Guesthouse | 0.7.0, and the guest was made with 0.6.2. From 21 November one VM's daemon was a build of #271 on 0.7.1. Since 22 November both run 0.7.5's own daemon |
| Guest | Windows 11 Pro, build 26300, with AMD's driver from Adrenalin 26.10.2 (32.0.32015.12012) |

The six patches in those packages are the `hw/hyperv` commits of `qemu/qemu-gpupv.patch` from 0.7.0. They're the same in 0.7.5. The first day ran on an earlier build with the same six patches, `11.1.3-2+virgl1+gpupv1`, which differed only in being built against virglrenderer 1.3.0 instead of Debian's 1.1.0. The table under [what ran](#what-ran) says which build each part ran on, where "both" means the two 11.1.3-2 builds. On `11.1.3-4+gpupv1` so far: both VMs started and stopped with `qm`, side by side, and its last row.

## Checking a host

```
packaging/proxmox/guesthouse-pve.sh check
```

It runs as root and only reads. It says what the host has and lacks for a VM with the GPU, and then what to put right, in that order: Proxmox's enterprise repository left on without a subscription, a card that `vfio-pci` has or that its driver is kept off, Landlock with the `lsm=` line for the host's boot loader, the `pve-qemu-kvm` and what keeps it, a newer one from Proxmox, the user's groups, lingering and Guesthouse's GPU files, the packages for making a Windows guest and swtpm's AppArmor rules, and each VM's `args:`, `memory:` and file in `/etc/guesthouse/proxmox` against each other, as the hookscript checks them. For a VM on a bridge it notes that Remote Desktop is shut from the LAN, below. `guesthouse-check`'s own lines come after the user's. It ends with 1 if something stops a VM or the making of its guest. `--report` prints the same without the names of users, the host and VMs, and without users' paths, for an issue.

It has run on the host here, where it found nothing to put right, and on made-up hosts in `tests/proxmox/run.sh`. `tests/proxmox/setup-vm.sh` feeds `setup-vm.sh` made-up printed scripts.

## Why its own pve-qemu-kvm

Proxmox starts every VM with `/usr/bin/qemu-system-x86_64` from its package `pve-qemu-kvm`, and a VM can't name another one. Proxmox's QEMU knows nothing of GPU-PV. Guesthouse's own QEMU has the devices, but none of Proxmox's patches: its backup, its machine types and the rest that `qemu-server` expects. And the devices can't be added from outside, as a VMBus device has to be inside QEMU's process, and Proxmox builds QEMU without modules.

So the package is Proxmox's, built from Proxmox's source, with the Windows guest's part of Guesthouse's patch at the end of its patch series. On 11.1.3-2, Proxmox's 74 patches and all of Guesthouse's touch no file in common, and they apply without a conflict. The six `hw/hyperv` commits apply on their own too. The rest of Guesthouse's patch is for Linux guests and the app's viewer, and isn't in the package.

For a VM without `vmbus-bridge` the six commits change nothing that runs. In `vmbus.c` the bridge gets two properties that default to 0 and a channel's offer gets a field, and `acpi-build.c` gets a branch that's only taken when the bridge has an MMIO window.

## The repository

The package is also in a signed apt repository, so there's nothing to build. It has two suites. `testing` gets a new build the night Proxmox puts out a new `pve-qemu-kvm`, so Proxmox's fixes aren't held back for long. A build reaches `stable` only after it has run with a GPU-PV VM on a real card. Each package has a build attestation, so `gh attestation verify FILE --repo ruxnlabs/guesthouse` checks it came from this repo's workflow. The key is the one Guesthouse's own packages use, with the fingerprint `80BE19F31995110352B875E22425E7C41C3ACA45`, and its public half is in this repo as [`packaging/packages/ruxn-pkg.asc`](../packaging/packages/ruxn-pkg.asc).

As root on the host, for `stable`:

```sh
install -d /etc/apt/keyrings
key=$(mktemp)
curl -fsSL https://pkg.ruxn.com/ruxn-pkg.gpg -o "$key"
gpg --show-keys --with-colons "$key" | grep -q '^fpr:::::::::80BE19F31995110352B875E22425E7C41C3ACA45:$' &&
  install -m 644 "$key" /etc/apt/keyrings/ruxn-pkg.gpg
rm -f "$key"
printf '%s\n' 'Types: deb' 'URIs: https://pkg.ruxn.com/proxmox' 'Suites: stable' 'Components: main' \
  'Signed-By: /etc/apt/keyrings/ruxn-pkg.gpg' >/etc/apt/sources.list.d/guesthouse-gpupv.sources
printf '%s\n' 'Package: pve-qemu-kvm' 'Pin: release o=ruxn labs,l=Guesthouse GPU-PV pve-qemu-kvm' 'Pin-Priority: 1001' \
  >/etc/apt/preferences.d/guesthouse-gpupv
apt update && apt install pve-qemu-kvm
```

For `testing`, write `testing` in place of `stable`. The pin takes the place of the hold. It keeps `pve-qemu-kvm` to this repository's builds, even when Proxmox's is newer, and `apt upgrade` still takes each new build here. A package that's held already needs `apt-mark unhold pve-qemu-kvm` first. To go back to Proxmox's own, remove the two files, then `apt update && apt install --allow-downgrades pve-qemu-kvm=VERSION` with Proxmox's version.

Between a new Proxmox release and its build here, `apt list --upgradable` shows nothing for `pve-qemu-kvm`, as the pin keeps Proxmox's out. A `qemu-server` that asks for a newer `pve-qemu-kvm` waits for that build too.

Both suites' indexes are signed again each day, and say how long they're good for.

The debug symbols aren't in the repository. They stay with the workflow's run for 30 days.

## Building the package

To build it yourself instead, or a version the repository doesn't have:

```
packaging/proxmox/build-pve-qemu-kvm.sh 11.1.3-4
```

The argument is what to build of `git.proxmox.com/git/pve-qemu.git`: a version, which is the commit Proxmox calls "bump version to 11.1.3-4", or a commit or a branch. `newest` builds the version of `pve-qemu-kvm` that Proxmox's `pve-no-subscription` repository has now. The package gets Proxmox's version with `+gpupv1` after it, and `--suffix` names another. The script needs podman or docker and no root. It builds in a Debian 13 container, with the build's dependencies from Proxmox's `pve-no-subscription` repository, and checks Proxmox's archive key against the hash in the script. It takes about ten minutes, and leaves the package, its debug symbols and `SHA256SUMS` in `build/proxmox`. On a PC with little RAM, `DEB_BUILD_OPTIONS=parallel=3` before the command limits the build to three jobs. With 7 GiB and eight jobs, the kernel killed a compiler for want of memory.

It takes the patches from `qemu/qemu-gpupv.patch` in the same checkout: every commit that touches `hw/hyperv/` or `include/hw/hyperv/`. If one of those also touches anything but `hw/i386/acpi-build.c`, it stops, so a new commit is looked at before it goes into the package. `--patches-only` writes the patches to `build/proxmox/patches` and stops there.

At the end it unpacks the package and looks in its QEMU's list for each device the VM's `args:` name, and for the bridge's `mmio-base`, and fails if one is missing.

The workflow `.github/workflows/proxmox.yml` runs the script with `newest` each night, and for a pull request that changes the patch or the script. So a patch that stops applying to a new `pve-qemu-kvm` fails there, and names the patch and the file. It keeps no package. A host's package is built with the script.

`11.1.3-2+gpupv1` and the `11.1.3-4+gpupv1` that runs here were built with the same steps, by hand, before the script. The script has built `11.1.3-4+gpupv1` too, in 11 minutes with three jobs, and its package has the same files, version and dependencies as the one that runs.

## Installing a build of your own

```
apt install ./pve-qemu-kvm_11.1.3-4+gpupv1_amd64.deb
apt-mark hold pve-qemu-kvm
```

It replaced one package, and added and removed nothing. VMs that are running keep the QEMU they started with until they're next started.

The hold matters. `11.1.3-4+gpupv1` sorts above Proxmox's `11.1.3-4` and below its next release, so without the hold the next update puts Proxmox's package back, and every VM with GPU-PV words in its `args:` stops starting, as QEMU doesn't know the devices. Other VMs don't mind either package.

With the hold, Proxmox's QEMU fixes, security fixes among them, don't reach the host until a build of the new release is installed. `apt list --upgradable` shows when one is waiting. Each new `pve-qemu-kvm` from Proxmox needs a new build before it's installed, and `build-pve-qemu-kvm.sh newest` builds the one the repository has. A `qemu-server` that asks for a newer `pve-qemu-kvm` would be held back too.

To install a build over a held one, `apt install --allow-change-held-packages ./FILE`, then `apt-mark hold pve-qemu-kvm` again. A build that sorts lower also needs `--allow-downgrades`.

## The guest

The Windows here was made and installed with Guesthouse first, with `guesthouse-vm` on the host itself, as [without a window](headless.md) says. Then its disk went into a new Proxmox VM with `qm disk import`, with a new EFI disk and a new TPM state. With the render device at the PCI address Guesthouse had given it, Windows found the same GPU and needed nothing new.

A second VM was a Windows that already existed, with Guesthouse's files put into it by hand and no agent. That's a change of its own, and isn't on this page.

## Setting a VM up

```
packaging/proxmox/setup-vm.sh --user USER 101 MACHINE
```

It runs as root on the host, with the VM stopped, for a VM whose disk is a Guesthouse machine's, as above. It asks `guesthouse-vm command MACHINE`, as USER, for the GPU's words, and puts them at the start of the VM's `args:`, with the run folder of the VM's slot and with memory files named for the VM. It sets `memory:` to the machine's RAM, and `balloon: 0`. It puts `hookscript.sh` in the `local` storage's snippets as `guesthouse.sh`, and names it as the VM's hookscript. And it writes `/etc/guesthouse/proxmox/101.conf`, which the hookscript reads: the user, the slot, the RAM, and the rest of the daemon's environment that `guesthouse-vm command` printed, such as the card. A switch of your own, such as `GUESTHOUSE_AMD_GIVE_WAY=1` for [sharing an AMD card](amd.md#sharing-the-card), goes on its `daemon_env=` line, after the words already there, and is read at the VM's next start. Running the script again writes the file anew, so add it again after that.

- The slot is the lowest of 1 to 3 that no other VM's file names. Guesthouse's own machines take 0 first. `--slot` names one.
- The render device goes at `bus=pcie.0,addr=0x6`, which is where Guesthouse had it for the machine here. `--render-address` names another slot, and `none` leaves it to QEMU.
- `--ram` gives the guest other RAM than the machine's, and `--storage` another storage for the snippet.
- The rest of `args:` stays as it was. Run it again after an update of Guesthouse, or for another slot or RAM, and it only replaces the GPU's words.
- It stops if the VM hasn't `bios: ovmf` and a q35 machine type, and notes an `ostype` other than `win10` or `win11` and a `cpu` other than `host`. The disks, the TPM, the network and the rest of the config below are yours.
- Root runs what it writes, as QEMU's words and the daemon's environment, and those come from a program in the user's home. So it takes only words of the shapes `guesthouse-vm command` prints today, with this VM's paths, and stops at anything else. A newer Guesthouse that prints other words needs a newer script.
- If `qm set` fails, the VM and its file stay as they were.
- `--dry-run` prints the file and the `qm set` it would run, and changes nothing.

The hookscript does this, at each start and stop.

- In `pre-start`, it checks that `memory:` matches the RAM, that `args:` names the VM's memory files and the slot's sockets, that no other daemon of the user's is in the slot, that 2 GiB of RAM is left once the guest's is taken, and that `/dev/shm` has room for it. If any of that fails, the VM doesn't start. Then it starts the daemon as the user, as the systemd unit `guesthouse-daemon-VMID`, and waits for both sockets. The daemon's output goes to `daemon.out` in the run folder.
- In `post-stop`, it stops the unit and removes the sockets and the two memory files.
- The unit is ordered before `pve-guests.service`, so when the host shuts down, Proxmox shuts the guest down before the daemon stops. At boot, with `onboot: 1`, it waits up to 90 seconds for the user's `/run/user/UID`.

## The VM's config

These are the lines that matter, as they ran, written out for VM 101 with the daemon in slot 1 of the user with uid 1000. `setup-vm.sh` writes the `args:`, `memory:` and `balloon:` lines:

```
bios: ovmf
machine: pc-q35-11.1
ostype: win11
cpu: host,flags=-nested-virt
memory: 8192
balloon: 0
vga: std,memory=8
efidisk0: ...,efitype=4m
tpmstate0: ...,version=v2.0
args: -device vmbus-bridge,mmio-base=0x10000000000,mmio-size=0x1000000000 -device hv-timesync -chardev socket,id=gpg,path=/run/user/1000/guesthouse/run-1/global.sock,reconnect-ms=500 -object memory-backend-file,id=gpio,size=64G,mem-path=/dev/shm/gpupv-window.101,share=on -device gpupv-global,chardev=gpg,instanceid=711dad3a-73ce-468b-90a9-ede6906841b2,iomem=gpio,iomem-base=0x10000000000 -chardev socket,id=gpv,path=/run/user/1000/guesthouse/run-1/vgpu.sock,reconnect-ms=500 -device gpupv-vgpu,chardev=gpv,instanceid=7a1e2f30-4c55-4d6e-9f10-0123456789ab -object memory-backend-file,id=ram0,size=8G,mem-path=/dev/shm/gpupv-ram.101,share=on -machine memory-backend=ram0 -device gpupv-render,channel=7a1e2f30-4c55-4d6e-9f10-0123456789ab,hostcaps=1,bus=pcie.0,addr=0x6
```

- `args:` holds the GPU's words from `guesthouse-vm command NAME`, in its order: the bridge, the clock's service, the two sockets, the two memory files and the three GPU devices. By hand, copy `mmio-base` and `iomem-base` from what it prints for your PC, and `vendor-id` and `device-id` on `gpupv-render` if it prints them. `bus=pcie.0,addr=` puts the render device where Guesthouse had it. Without it, Windows found the device at a new address and installed its driver again by itself.
- `memory:` is in MiB, and has to match the RAM backend's `size=` and the daemon's `GUESTHOUSE_RAM`. Proxmox passes `-m` from it, and `-machine memory-backend=ram0` makes the file the guest's RAM.
- `balloon: 0`, as the RAM file is taken whole at the start. A balloon wasn't tried.
- `vga: std` shows the firmware and the boot. After that, Windows draws its desktop on Guesthouse's display driver, and Proxmox's console stays black. Use Remote Desktop or another remote desktop program.
- `cpu: host,flags=-nested-virt` is Proxmox's word with Hyper-V's flags for `ostype: win11`, with `svm` off, as Guesthouse has it. Guesthouse's three more flags, `hv-stimer-direct`, `hv-frequencies` and `hv-tlbflush`, weren't needed.
- The rest is Proxmox's as usual. A SATA disk and a VirtIO SCSI disk both ran, and so did a VirtIO network card on a bridge, three USB devices from the host with `usbN: host=`, and both `localtime: 0` and Proxmox's default for Windows.

## The daemon beside it

Each VM has its own daemon, as [standalone](standalone.md) describes: started before QEMU, with `GUESTHOUSE_RAM_FILE` and `GUESTHOUSE_WINDOW_FILE` on `/dev/shm`, `GUESTHOUSE_SLOT`, `GUESTHOUSE_RAM` and `GUESTHOUSE_AMD_CARD`, and stopped after QEMU has gone, with the two files removed. It runs as the ordinary user that installed Guesthouse and ran its setup, and reads that user's store.

- Proxmox runs QEMU as root. It opened the user's memory files, which are mode 600, and both sockets.
- The user needs lingering on, with `loginctl enable-linger`, so that `/run/user/UID` is there without a login.
- A backup of a VM that's off starts QEMU paused, so the daemon has to come up for it as well.
- `hookscript.sh` does this, as [setting a VM up](#setting-a-vm-up) says.

## What ran

| | | Build |
|---|---|---|
| `qm start` and `qm shutdown` | the GPU is working after each | both |
| A timer that runs `qm start` each morning and `qm shutdown` each evening | | both |
| A restart inside Windows | the GPU is working after it | `+virgl1+gpupv1` |
| `qm stop`, the power cut | the next start is fine, and Windows notes the unclean end | `+virgl1+gpupv1` |
| Backup to Proxmox Backup Server, VM running | snapshot mode, 64 GiB in 8 minutes, the guest kept running, and the GPU was fine after | `+virgl1+gpupv1` |
| Backup, VM off | 1 minute. QEMU and the daemon start for it and stop after | `+virgl1+gpupv1` |
| Backup of a 980 GiB VM, running | 1 hour 39 minutes | `+virgl1+gpupv1` |
| `qmrestore` as a new VM | the config comes with `args:` and the hookscript. The paths in `args:` name the old VM and needed changing, then it started with the GPU working | `+virgl1+gpupv1` |
| Two GPU-PV VMs at once | both GPUs working | both |
| A USB licence dongle passed through | works in Windows | both |
| #271's video, `GUESTHOUSE_AMD_VIDEO=1` with `_CLOSE=1` and `_REST=1500` | a RustDesk session encoded 3440x1440 with `hevc_amf` on the earlier build. On both, RustDesk's codec probe opened its six encoders at each start of the service | both |
| Four guests on the card, below | | `+gpupv1` |
| 0.7.5's own daemon, in both VMs | the GPU is working. With the video switches, RustDesk's probe opened its six encoders, and each was closed | `11.1.3-4+gpupv1` |
| A reboot of the host, with one VM on `onboot: 1` | Proxmox shut the guest down, and its daemon stopped 3 seconds later. After the boot the VM started by itself, with the GPU working | `11.1.3-4+gpupv1` |
| `setup-vm.sh` and `hookscript.sh`, on the imported VM | the `args:` it wrote was the same, word for word, as the one that had run, and run again it left them so. `qm start` started the daemon and the GPU was working, and `qm shutdown` stopped it and removed the files. With `memory:` changed, the start was refused | `11.1.3-4+gpupv1` |

All but the last row ran with a config written by hand and with the hookscript that `hookscript.sh` was written from, which takes the same steps in the same order. It also ran a watcher beside each VM, which isn't part of this.

GravityMark 1.89, Direct3D 12, 1920x1080, was 6599 in the imported VM and 6531 in the other one on the earlier build, 6447 in the imported one on `11.1.3-2+gpupv1`, beside the other VM, and 6468 there on `11.1.3-4+gpupv1` with 0.7.5's daemon. The same Windows under Guesthouse's own start was 6287 to 6553.

Four guests shared the card at once, by three routes: the two Windows VMs on GPU-PV, a Debian VM on Proxmox's VirGL (`vga: virtio-gl`), and a container given `/dev/dri/renderD128`. Nothing failed and nothing reset.

| | Windows A, GravityMark | Windows B, GravityMark | Debian VM, glmark2 | Container, vkmark |
|---|---|---|---|---|
| each alone | 6243 | 6169 | 75 | 4624 |
| both Windows | 2950 | 2961 | | |
| all four | 2945 | 2973 | 8 | 163 |

From amdgpu's counts in fdinfo, the two Windows guests had 49 % and 47 % of the card's 3D time. Beside them, the two small Linux loads got 5 % and 1 %.

## Things to know

- **Don't install `qemu-utils`.** On Proxmox it removes `proxmox-ve`, `pve-manager`, `pve-qemu-kvm` and `qemu-server`. Proxmox's own packages have `qemu-img` already. Guesthouse's setup check named it on every Debian host up to 0.7.0. Since 0.7.1 it names a package only for a tool that's missing, so on Proxmox it doesn't.
- **Landlock is off in Proxmox's kernel.** Without it Guesthouse won't start a GPU machine of its own unless the machine is allowed to run unjailed, and the guest has to be made and started with Guesthouse first. The host here runs with `lsm=landlock,lockdown,yama,integrity,apparmor` on the kernel's line, which is Proxmox's list with Landlock first. A daemon on a host without it wasn't tried.
- **Proxmox doesn't count the guest's RAM.** It's a file in `/dev/shm`, so Proxmox shows a VM using under 100 MiB, while the whole of its RAM is taken from the start. Leave room for it yourself, and in `/dev/shm`, which is half the RAM by default.
- **The three RAM sizes go together.** `memory:`, the `size=` of the RAM backend and `GUESTHOUSE_RAM`. `setup-vm.sh` sets all three, and the hookscript refuses to start a VM when they differ, so what QEMU does then wasn't seen.
- **A restore to another VM ID** keeps the old ID wherever `args:` has it, as in the memory files' names above, and has no file in `/etc/guesthouse/proxmox`. `setup-vm.sh` for the new ID should put both right, which hasn't been tried.
- **A start that QEMU refuses** ends with an error in Proxmox's task, after the hookscript has started the daemon. When that happened here, on a wrong device word in `args:`, the daemon saw QEMU go and ended by itself 5 seconds later, and the memory files were gone. A daemon that's left is stopped by the VM's next start, or by `systemctl stop guesthouse-daemon-VMID`.
- **Remote Desktop on a bridge.** Guesthouse sets Windows' firewall up for its own network, and lets port 3389 in only from 10.0.2.2, the host there. A VM on a Proxmox bridge has no such host, so Remote Desktop from the LAN stays shut until a rule in Windows lets it in. A tester on Proxmox ran into this after `--remote-desktop on`. Proxmox's console is black after the boot, and Windows+P with Duplicate put the desktop there for him.
- **Removing a VM** leaves its file in `/etc/guesthouse/proxmox`, and `setup-vm.sh` counts its slot as taken until the file goes.
- **Four daemons at most** for one user at a time, one in each slot, and Guesthouse's own machines use the same slots.
- **QEMU runs as root, without Guesthouse's jail.** That's how Proxmox runs every VM. The daemon is still jailed.

## Not tried

- `hookscript.sh` itself at a reboot of the host, and for a backup. Both ran with the hookscript it was written from.
- `setup-vm.sh` for more than one VM, for a VM that isn't a copy of a Guesthouse machine, and for a machine that Guesthouse put on NVMe, whose render device may have been at another address.
- NVIDIA and Intel cards, and AMD cards other than the 780M.
- Live migration, HA and clusters, snapshots with or without RAM, suspend to disk, memory hotplug and the balloon.
- Linux guests this way.
- A release's daemon between 0.7.1 and 0.7.4, and any `pve-qemu-kvm` other than the ones above.
- A remote desktop session on 0.7.5's daemon with the video switches. Its codec probe ran, a session hasn't yet.
- More than a few days' running.
