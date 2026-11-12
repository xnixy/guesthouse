# Running machines without a window

`guesthouse-vm` lists, starts and stops machines from a terminal. It has no window and doesn't need GTK, so it runs on a PC with no desktop, over ssh. It's in the same folder as the app, which is `~/.local/lib/guesthouse/current` after `install.sh`.

```
guesthouse-vm list
guesthouse-vm status NAME
guesthouse-vm start NAME [--no-wait]
guesthouse-vm stop NAME
guesthouse-vm off NAME
guesthouse-vm log NAME
```

NAME is a machine's id, or its name in any case. If a name fits two machines, it lists them, and the id picks one.

- `list` has a line for each machine: its name, id, kind and what it's doing.
- `start` does what Start does in the app, and prints each step as it happens. It ends when the machine is ready, with exit code 0, or prints what went wrong and exits with 1. `--no-wait` returns as soon as the start is under way, and `status` says how far it is.
- `stop` asks the guest to shut down and waits. If the guest doesn't shut down in time, the VM is turned off and it says so. `off` is Force Off: the power is cut and the guest gets no chance to save anything.
- `log` says where the log is and shows the end of it. The log is one file for every machine, so the lines aren't sorted by machine.

A machine started here is the app's to stop, and the other way round. Both look at what's running, not at who started it.

## What it doesn't do yet

Machines are made in the app for now, and so are setup, installs and every setting. That can be the app on the same PC, or on another one if you forward its display. A later version will make machines from the command line. A machine whose install didn't finish is also left to the app.

I've run a plain machine this way, on a PC with no desktop session: list, start, status, stop and off. I haven't tried a GPU machine with no window yet.

## Getting a screen

There isn't one. QEMU runs with no viewer, and the machine runs the same. To use Windows you go in over the network, with Remote Desktop:

1. In Windows, turn Remote Desktop on. This is Windows' own setting, under System in its Settings.
2. In the machine's settings in the app, under Port Forwards, add TCP with host port 3389 and guest port 3389. It can only be added while the machine is off, and it takes effect at the next start. It's `hostfwd` on QEMU's own network, so it only reaches the guest from the PC the machine runs on: `127.0.0.1:3389` there goes to port 3389 in Windows.
3. Connect Remote Desktop to `127.0.0.1:3389` on that PC. From another PC, tunnel it first with `ssh -L 3389:127.0.0.1:3389 user@host` and connect to `127.0.0.1:3389` on your own PC. The other way is Bridged networking in the machine's network settings, which gives the guest an address of its own on your network.

A Windows machine's account is on the Account page in its settings, and the password is in the `windows-password` file in the machine's folder.

## Logging in over ssh

A VM's sockets are in `$XDG_RUNTIME_DIR/guesthouse`. If that variable isn't set, Guesthouse uses `/run/user/<your user id>` when the system has made that folder for you, and only then your state folder. So a login over ssh finds the same machines as the desktop does, as long as the folder is there. With `sudo loginctl enable-linger $USER` systemd keeps your user's folder when nobody is logged in.
