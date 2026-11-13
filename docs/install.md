# What the installer does

## What a machine needs

A Windows machine on NVIDIA needs NVIDIA's kernel module loaded, with `/dev/nvidiactl` and the card's own `/dev/nvidiaN`, and `nvidia-smi` on your PATH. It has run on my RTX 5090, with NVIDIA 615.78.08 on the host and 617.42 in Windows 11 Pro, build 26300, and on testers' RTX 5080, RTX 4080, RTX 3090 and GTX 1660 SUPER, and laptops with an RTX A1000, RTX 3050 and RTX A5500. Host driver 620 works for Windows guests too, and Linux guests still need 615. The 20 series has a profile, and nobody has run it yet. A laptop's card uses the same driver installer, which lists it under the laptop maker.

A Windows machine on AMD needs the card on amdgpu, and AMD's Windows installer for that card, which the app unpacks. It has run on an integrated RDNA2 Radeon with driver 32.0.21045.20006, and on testers' RX 9070 XT, RX 7900 XTX, RX 6900 XT, RX 6700 XT, RX 6700 and RX 6600 XT. See [AMD](amd.md).

A Linux machine's needs are in [`linux/`](../linux/README.md).

A plain VM needs QEMU with its D-Bus display, which Fedora calls `qemu-ui-dbus`, plus `qemu-img`, `swtpm`, `genisoimage` and `7z` for installs. A QEMU built as `qemu/README.md` says has no `qemu-img` of its own, so install the system's.

## The steps

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
