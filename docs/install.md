# What the installer does

## What a machine needs

A Windows machine on NVIDIA needs NVIDIA's kernel module loaded, with `/dev/nvidiactl` and the card's own `/dev/nvidiaN`, and `nvidia-smi` on your PATH. It has run on my RTX 5090, with NVIDIA 615.78.08 on the host and 617.42 in Windows 11 Pro, build 26300, and on testers' RTX 5080, RTX 5060 Ti, RTX 4090, RTX 4080, RTX 3090, RTX 2080 and GTX 1660 SUPER, and laptops with an RTX A1000, RTX 3050 and RTX A5500. Host driver 620 works too, for Windows and Linux guests. A laptop's card uses the same driver installer, which lists it under the laptop maker.

A Windows machine on AMD needs the card on amdgpu, and AMD's Windows installer for that card, which the app unpacks, or `guesthouse-vm new --amd-driver` on a PC with no desktop. It has run on an integrated RDNA2 Radeon with driver 32.0.21045.20006, and on testers' RX 9070 XT, RX 7900 XTX, RX 7800 XT, RX 6900 XT, RX 6700 XT, RX 6700, RX 6600 XT, and RX 5700 XT. See [AMD](amd.md).

A Windows machine on an Intel card is experimental, on Lunar Lake, Battlemage, Panther Lake and Wildcat Lake: it needs the card on `xe` and Intel's Windows installer, which the app unpacks, or `guesthouse-vm new --intel-driver`. The card has drawn Windows' desktop on an Arc B580, and the other cards are untried. See [Intel](intel.md).

A Linux machine's needs are in [`guest/linux/`](../guest/linux/README.md).

A plain VM needs QEMU with its D-Bus display, which Fedora calls `qemu-ui-dbus`, plus `qemu-img`, `swtpm`, `genisoimage` and `7z` for installs. A QEMU built as `qemu/README.md` says has no `qemu-img` of its own, so install the system's.

## The steps

`install.sh` and `uninstall.sh` are at the top of this repo. They're plain `sh`, and they're meant to be read before you run them. Nothing runs until the last line, so a download that gets cut off does nothing. To read it first, download it, read it, and run it with `sh install.sh --dry-run`, which checks everything, shows the plan and stops.

What `install.sh` does, in order:

1. Refuses to run as root, on musl, and on anything but x86_64. On NixOS it points you to the flake instead: see [nixos.md](nixos.md).
2. Looks at the distro, CPU and KVM, with no root.
3. Downloads one release into a folder of its own and checks its SHA-256 against the one in the script. It tries the copy on pkg.ruxn.com first and GitHub after, with the same hash either way.
4. Asks the release's own `guesthouse-check` what this PC can run. Its output lines are a stable format, written down in [check-output.md](check-output.md). That's what looks at the GPU and its driver. If the answer's no it still installs, with plain VMs only, and says why.
5. Shows you the plan and every command that needs root, word for word, and waits for you to type yes. Root is one `sudo`, for missing packages, QEMU's bridge helper, Guesthouse's clock helper and swtpm's AppArmor rule, or nothing at all if those are already done.
6. Has root put QEMU's bridge helper in `/usr/local/libexec/guesthouse`, setuid root, which bridged networking needs. It's built with the release, it's the one file outside your home folder, and it only goes in again when a release changes it. A setuid program can't live in your home folder, where it could be swapped or given a config you wrote. It reads `/etc/qemu/bridge.conf` and drops to the one right it needs as it starts, the same as Fedora's own. Beside it goes Guesthouse's clock helper, also setuid root, the same way. It takes only `hold` or `reset` and a card's PCI address, and holds an NVIDIA card's memory clock up while another device views a machine ([NVIDIA](nvidia.md)).
7. Where AppArmor is on and swtpm has a profile, as on Ubuntu, has root add `owner /run/user/[0-9]*/guesthouse/** rwk,` and `unix (send, receive) type=stream addr=none,` to `/etc/apparmor.d/local/usr.bin.swtpm`, the file the profile keeps for additions, and load the profile again. The profile lets swtpm write in your home folder but not in `/run/user`, where a machine's run folder is, and it only takes the data channel QEMU hands it from libvirt's QEMU, while a jailed one runs as Ubuntu's `bwrap` profiles. Without the rules a Windows machine's virtual TPM can't start. If you keep your machines outside `/home`, setup's Virtual TPM check gives the rule for that folder too.
8. Puts everything else under your own folders: the program in `~/.local/lib/guesthouse/VERSION`, reached through a `current` link, plus a launcher and icons. It writes all of that in `~/.local/lib/guesthouse/manifest`.
9. Tells the desktop the icons changed. It touches `~/.local/share/icons/hicolor`, and if there's an icon cache in it already, it rebuilds that with `gtk-update-icon-cache`. It never makes a cache, and the uninstall does the same once the icons are gone.

It never installs or changes a GPU driver, kernel arguments or services. On an immutable distro it adds no packages and prints what to layer, but the bridge helper still goes in, as `/usr/local` is writable there. On a system with no `sudo` the helper is left out, and if nothing else needs root the rest still installs: the script ends with the command root can run to put it in, and until then only bridged networking is missing. With packages missing, or swtpm's AppArmor rule still to add, it stops as before and names what root has to run first. If bridged networking says the helper is missing or can't run as root, run the installer again: it puts it back, even when the version is the same, or with no `sudo` shows root's command again.

## Packages

There are signed repositories for Debian and Ubuntu, Fedora and Arch on pkg.ruxn.com. A package holds the same release `install.sh` installs, from the same file, checked against the same hash. One key signs all three. Its fingerprint is `80BE19F31995110352B875E22425E7C41C3ACA45`, and its public half is in this repo too, as [`packaging/packages/ruxn-pkg.asc`](../packaging/packages/ruxn-pkg.asc).

Debian 13, and Ubuntu 24.04 or newer:

```sh
sudo install -d /etc/apt/keyrings
key=$(mktemp)
curl -fsSL https://pkg.ruxn.com/ruxn-pkg.gpg -o "$key"
gpg --show-keys --with-colons "$key" | grep -q '^fpr:::::::::80BE19F31995110352B875E22425E7C41C3ACA45:$' &&
  sudo install -m 644 "$key" /etc/apt/keyrings/ruxn-pkg.gpg
rm -f "$key"
printf '%s\n' 'Types: deb' 'URIs: https://pkg.ruxn.com/guesthouse/apt' 'Suites: stable' 'Components: main' \
  'Signed-By: /etc/apt/keyrings/ruxn-pkg.gpg' | sudo tee /etc/apt/sources.list.d/guesthouse.sources >/dev/null
sudo apt update && sudo apt install guesthouse
```

Keep the list's name, `guesthouse.sources`, as the app's update reads that list alone.

Fedora:

```sh
key=$(mktemp)
curl -fsSL https://pkg.ruxn.com/ruxn-pkg.asc -o "$key"
gpg --show-keys --with-colons "$key" | grep -q '^fpr:::::::::80BE19F31995110352B875E22425E7C41C3ACA45:$' &&
  sudo install -D -m 644 "$key" /etc/pki/rpm-gpg/RPM-GPG-KEY-ruxn
rm -f "$key"
printf '%s\n' '[guesthouse]' 'name=Guesthouse' 'baseurl=https://pkg.ruxn.com/guesthouse/rpm' 'enabled=1' \
  'gpgcheck=1' 'repo_gpgcheck=1' 'gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-ruxn' |
  sudo tee /etc/yum.repos.d/guesthouse.repo >/dev/null
sudo dnf install guesthouse
```

The key is the one in this repo, checked against the fingerprint above before it goes in. dnf asks once to import it, and shows the same fingerprint.

Arch:

```sh
curl -fsSL https://pkg.ruxn.com/ruxn-pkg.asc | sudo pacman-key --add -
sudo pacman-key --lsign-key 80BE19F31995110352B875E22425E7C41C3ACA45
printf '%s\n' '' '[guesthouse]' 'SigLevel = Required' 'Server = https://pkg.ruxn.com/guesthouse/arch/$arch' |
  sudo tee -a /etc/pacman.conf >/dev/null
sudo pacman -Syu guesthouse
```

Every package also has a build attestation, so `gh attestation verify FILE --repo ruxnlabs/guesthouse` checks it was built by this repo's workflow.

What a package does differently from `install.sh`:

- The release goes in `/usr/libexec/guesthouse`, for every user of the PC, not in your home folder. The launcher and icons go in `/usr/share`, and `guesthouse`, `guesthouse-vm` and `guesthouse-check` in `/usr/bin`. There's no manifest and no `uninstall.sh`: the package manager removes it, and your machines stay.
- The bridge helper comes in the package, setuid root, as `/usr/libexec/guesthouse/qemu-bridge-helper`, as a package never puts files in `/usr/local`. The app looks there too. If bridged networking says it's missing or can't run as root, install the package again.
- What Guesthouse needs from the system comes as the package's dependencies. swtpm's two AppArmor rules go in `/etc/apparmor.d/local/usr.bin.swtpm` when it's installed, as `install.sh` puts them, and come out when it's removed.
- It doesn't add you to the `kvm` group, as a package doesn't know who will use it. That's rarely needed, and setup's check says when it is.
- It always has the GPU's files. `install.sh` leaves them out on a PC that can't run a GPU machine, and then the app only offers plain ones. With a package the app offers both, and the daemon's own check still refuses to start a GPU machine this PC can't run, and says why.
- The deb picks the app with HDR when GTK is 4.16 or newer, as on Debian 13, and the one without on Ubuntu 24.04's 4.14. It picks again when GTK is upgraded.

Updates come with the package manager, and the app checks once as it opens. On Debian, Ubuntu and Fedora its Update upgrades Guesthouse alone, with apt or dnf, after a window asks for your password. On Arch it says a new version is out and to run `sudo pacman -Syu`, as Arch takes no partial upgrades.

Don't have both. An install from `install.sh` in your home folder comes first for you, and its launcher hides the package's. Remove one with its own uninstall.

## The AppImage

Each release also has `guesthouse-installer-VERSION.AppImage`, for anyone who'd rather download one file than pipe a script. It isn't a second kind of install. It carries the same release, and `install.sh` from inside it, which does every step above except the download, and puts the release in the same place. So the app, its updates and `uninstall.sh` are the same whichever way it came. It needs no FUSE library.

Opened from the file manager, it shows Guesthouse's own install window. That's the same plan the script prints, as a page: what goes where, what needs root and why, the exact command, and what it found on this PC. Install runs it, a window asks for your password if a part needs root, and then the app starts. On a PC with no GTK or libadwaita yet, a KDE desktop for one, the window uses the copies the AppImage carries, and the install adds the PC's own for the app. It installs in a terminal instead only when even those can't load, or when you pick Use a Terminal.

Started from a terminal, it installs in that terminal. Once this release or a newer one is installed, opening it again only starts the app, so it can be deleted after the first time. Updates come from the app, as for any install. `guesthouse-installer-VERSION.AppImage install --dry-run` shows the plan and stops, and `guesthouse-installer-VERSION.AppImage uninstall` runs the installed `uninstall.sh`.

The app doesn't run from inside the AppImage. Its folder goes away when the AppImage stops, and a VM's QEMU has to keep running after the app closes. Check the AppImage against `SHA256SUMS` on the release, as nothing else checks it. `gh attestation verify FILE --repo ruxnlabs/guesthouse` checks it was built by this repo's release workflow.

A word on the checksum. It proves the download is the release the script names, and that's all. It can't protect you from the script, which is why you should read it.

To update, run the script again, or `~/.local/lib/guesthouse/current/install.sh --update`. It says what will change and refuses while one of its VMs is running. VMs of another copy of Guesthouse, like a build of the repo, don't count. The old version stays until the new one is in place, so a failed update leaves you with what you had. Your machines, disks and settings aren't touched. `--check-update` says if there's a newer release and changes nothing. It reads the version in https://pkg.ruxn.com/guesthouse/latest.json, which a release puts up last of all, and asks the script on `main` if that can't be read. If your driver has moved to another branch, it tells you what that means. GPU machines won't start until the branches match, and plain VMs still run.

Machines and their disks are kept in `~/.local/share/guesthouse`. New Machine can put one machine's disk on another drive. To keep everything on another drive, write that folder's path into `~/.config/guesthouse/store` before you open the app for the first time, as setup keeps the driver's files there too. The uninstall leaves machines and their disks alone, wherever they are.

To remove it, run `~/.local/lib/guesthouse/current/uninstall.sh`. It refuses while one of its VMs is running, and removes what's in the manifest and nothing else, and a file that has changed since the install stays. The bridge helper and the clock helper go too, with one `sudo`, unless another install has replaced them since, and so does swtpm's AppArmor rule. Your machines stay too, unless you pass `--delete-machines` and then type the folder's name. The driver files the app copied from NVIDIA's installer stay, and it says where. Packages stay, as something else may use them, and it lists them.

The oldest systems it's meant for are Ubuntu 24.04 and Debian 13: it needs glibc 2.39, GTK 4.14 and libadwaita 1.5, and on anything older the script stops and says why. HDR needs GTK 4.16, so Ubuntu 24.04 gets the app without it. I've installed it on those two in containers with no GPU, but not on a real desktop of either. It has also run on Fedora, on CachyOS, which has Arch's layout, and on openSUSE.
