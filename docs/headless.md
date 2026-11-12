# Running machines without a window

`guesthouse-vm` makes, starts and stops machines from a terminal. It has no window and doesn't need GTK, so it runs on a PC with no desktop, over ssh. It's in the same folder as the app, which is `~/.local/lib/guesthouse/current` after `install.sh`. It makes the same calls the app does, so a machine made or started here is the app's too, and the other way round.

```
guesthouse-vm list
guesthouse-vm status NAME
guesthouse-vm start NAME [--no-wait]
guesthouse-vm stop NAME
guesthouse-vm off NAME
guesthouse-vm log NAME
guesthouse-vm command NAME

guesthouse-vm new NAME --iso PATH [--kind windows|ubuntu|plain] [--disk GIB] [--cores N] [--ram GIB]
                       [--user NAME] [--amd-card PCI]
guesthouse-vm install NAME [--no-wait]
guesthouse-vm remove NAME [--yes]

guesthouse-vm show NAME
guesthouse-vm set NAME SETTING VALUE
guesthouse-vm forward NAME add|remove tcp|udp HOST GUEST

guesthouse-vm setup [--driver PATH] [--card PCI]
```

NAME is a machine's id, or its name in any case. If a name fits two machines, it lists them, and the id picks one.

## Starting and stopping

- `list` has a line for each machine: its name, id, kind and what it's doing.
- `start` does what Start does in the app, and prints each step as the task writes it to the log. It ends when the machine is ready, with exit code 0, or prints what went wrong and exits with 1. `--no-wait` returns as soon as the start is under way, and `status` says how far it is.
- `stop` asks the guest to shut down and waits. If the guest doesn't shut down in time, the VM is turned off and it says so. `off` is Force Off: the power is cut and the guest gets no chance to save anything.
- `log` says where the log is and shows the end of it. The log is one file for every machine, so the lines aren't sorted by machine.

## Making a machine

The disc decides what the machine is, as it does in the app's New Machine dialog: a Windows 11 Pro disc is a Windows machine with GPU-PV, an Ubuntu server disc is Ubuntu, and anything else is a plain VM that you install by hand. `new` says which it found. `--kind` is there to say what you expect, and it refuses if the disc is something else here. A disc that would be Windows or Ubuntu on a PC with the GPU part is a plain VM on a PC without it, and `new` says so.

What you don't give, you get as the dialog would start you: the sizes for that kind, with RAM as high as this PC allows, and for Windows the NVIDIA card, or an AMD card with `--amd-card`. The dialog's refusals come out as they are, in plain words, and nothing is made.

`new` makes the disk and its settings and stops there. A Windows or Ubuntu machine is then installed with `install`, which follows the install step by step like `start`. The guest account's password is made at the install and kept in the machine's folder. There's no way to choose it here.

`remove` says what it would delete, and does it when you add `--yes`. A machine whose disk is outside its folder only loses its entry, and the disk stays.

## Settings

`show` lists a machine's settings, with the port forwards. `set` changes one:

| Setting | Value | Machines |
|---|---|---|
| `cores`, `ram` | a whole number, within what this PC allows | all |
| `vram` | the video memory cap in GiB, 0 for none | Windows |
| `sound` | on or off | Windows |
| `screen` | 1920x1080, 2560x1440 or 3840x2160 | Windows |
| `refresh` | Hz | Windows |
| `network` | nat, none or bridged | not Ubuntu |
| `passt` | on or off, with NAT | not Ubuntu |
| `bridge` | the bridge's name | not Ubuntu |
| `share` | a folder, or `none` | not Ubuntu |

These are the settings pages' own checks, and a running machine refuses what the page greys out, which is all of them except `screen` and `refresh`. Those take effect at once if Windows is up on the display driver, as they do in the app. Everything else in a machine's settings is still the app's, such as HDR, V-Cache pinning, the clipboard, USB devices and snapshots.

`forward` adds or removes a port on this PC that reaches a port in the guest. It can only be changed while the machine is off.

## Setup

`setup` does what the first-run wizard does for GPU machines. It looks at the PC and prints each check as the wizard shows it, and stops if one failed. It then copies NVIDIA's driver files from the installer you give with `--driver`, which is the `.exe` for the version it names, and writes down the card. With more than one NVIDIA card it takes the best supported one, as the wizard does, unless you name one with `--card`. The wizard has no way to choose, so that part is only here. An AMD card's driver is still unpacked in the app, in the New Machine dialog.

I couldn't run `setup` on a real card, because the PC I tried this on has none. Its checks, the copy of the driver and the choice of the card are tested with pretend cards.

## What it doesn't do yet

I've run a plain machine this way on a PC with no desktop session, from `new` to `remove`. I haven't tried a GPU machine with no window, or an install of Windows or Ubuntu with `install`.

A plain VM that you install by hand has no screen here either, and a disc that waits for a key, as Windows' does, gets no key. QEMU's own monitor socket in the machine's run folder can send one, but Guesthouse doesn't wrap that yet.

## Getting a screen

There isn't one. QEMU runs with no viewer, and the machine runs the same. To use Windows you go in over the network, with Remote Desktop:

1. In Windows, turn Remote Desktop on. This is Windows' own setting, under System in its Settings.
2. With the machine off, forward the port: `guesthouse-vm forward NAME add tcp 3389 3389`. It takes effect at the next start. It's `hostfwd` on QEMU's own network, so it only reaches the guest from the PC the machine runs on: `127.0.0.1:3389` there goes to port 3389 in Windows.
3. Connect Remote Desktop to `127.0.0.1:3389` on that PC. From another PC, tunnel it first with `ssh -L 3389:127.0.0.1:3389 user@host` and connect to `127.0.0.1:3389` on your own PC. The other way is `guesthouse-vm set NAME network bridged`, which gives the guest an address of its own on your network.

A Windows machine's password is in the `windows-password` file in the machine's folder, and `show` says where the folder is.

## Starting it by hand

`guesthouse-vm command NAME` prints what would start a machine: the daemon with its environment, then QEMU's words, as a script. It starts nothing. It is for running the machine from your own script or from libvirt, and [standalone](standalone.md) says how.

## Logging in over ssh

A VM's sockets are in `$XDG_RUNTIME_DIR/guesthouse`. If that variable isn't set, Guesthouse uses `/run/user/<your user id>` when the system has made that folder for you, and only then your state folder. So a login over ssh finds the same machines as the desktop does, as long as the folder is there. With `sudo loginctl enable-linger $USER` systemd keeps your user's folder when nobody is logged in.
