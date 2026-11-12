# Architecture

## The shape of it

```
Windows guest                 QEMU                      guesthouse-daemon
  vendor driver                                         ┌───────────────────────┐
  graphics kernel  ─ VMBus ─▶ forwards every packet ──▶ │ transport  frames     │
                              as a frame on a socket    │ messages   parsing    │
                                                        │ core       the books  │
                                                        │ backend    the vendor │
                                                        └───────────────────────┘
```

The guest thinks it's talking to a Hyper-V host. QEMU moves bytes and knows nothing about graphics. Everything else happens in the daemon: it plays the host's half of the protocol, and hands anything vendor-specific to a backend.

The daemon runs as one process per VM.

## The crates

They're in `crates/`, listed here from the bottom up. Each depends only on crates above it in this list.

| Crate | Its job |
|---|---|
| `transport` | The byte stream from QEMU: frames, packet kinds, guest page ranges. |
| `messages` | What the guest's messages look like, and how to write the answers. No state, no decisions. |
| `sensors` | The host card's newest readings, in a small file any process of the user can read. |
| `jail` | What a guest-facing process may still do once it's jailed: Landlock, seccomp, and no new privileges. |
| `backend` | The `Backend` trait: what a GPU vendor must provide. Also the decision on whether to start. |
| `backend-standin` | A pretend GPU, for tests only. |
| `backend-nvidia` | The NVIDIA backend: the host's resource manager, channels, and what the guest's driver expects to hear. |
| `backend-amd` | The AMD backend: the host's amdgpu through its DRM ioctls, and the private data AMD's Windows driver sends. Experimental. |
| `core` | `Session`: the tables for one guest, and the answer to each message. Vendor-neutral. |
| `frame-socket` | The messages that bring frames and the cursor to a viewer, on their own so a producer can use them as they are. |
| `daemon` | Slots, shared memory, sockets, the viewer's socket, and the main loop. |
| `replay` | Replays a recording and compares every reply. |
| `app-core` | The desktop app's work with no UI: machines, tasks, setup, a VM's start, `guesthouse-task`, `guesthouse-share` and `guesthouse-vm`. |
| `app` | The desktop app's GTK 4 and libadwaita pages. |

To follow one message, start at `Server::read_channel` in `daemon`, go through `Router::handle_frame`, and land in `Session::dispatch` in `core/src/session/mod.rs`. The handlers are grouped by subject in that folder.

## One thread

Everything happens on one thread. That's why the message path has no locks, and why both channels share a single order of events. Things that have to wait for a fence go in a list, and the main loop calls `Session::poll` while that list has anything in it.

The cost is that a slow backend call holds up both channels, so backend calls on the message path must not block. The one exception is moving an allocation's contents on its first lock, which spins on the copy engine for a moment, and which nothing else can get ahead of. The blocking copy order, below, is another for now, and so is fetching a picture for the frame ring when the screen is still.

### Holding a reply

The thread must never wait, because one guest thread waiting is no reason to stop the others. So a handler that can't answer yet returns `Answer::Held`, and keeps the `ReplyTicket` it was given. The reply goes out later through `Session::take_replies`, and the guest thread that sent the message stays blocked on it, which is the whole point: it can't get ahead. Every other message goes on being served.

Three things use this, and a copy into the frame ring rides along with the second of them. A submission to a queue that's too far behind holds its reply until the ring has room. A copy to the staging surface can become a job that waits for its frame, runs, and then replies, though for now that's off and `GUESTHOUSE_HELD_COPIES` turns it on: by default a copy runs on the message thread, in the order the prototype was proven in, as the held order met a fault on its first real run and is not cleared of it yet. And a message that frees or moves a surface in use by such a job is parked, whole, and run again from the start when the jobs are done. A thread instead of a list would have needed locks on every table, and a wait on the message thread stalls every other guest thread behind it.

### Waiting for fences

Nothing tells the host when a fence moves, so it has to look. `Session::poll` says how urgently: `Urgent` if the guest is blocked on something, `Background` if only the GPU's health is being watched, and `Nothing` otherwise. The main loop spins for a short while when urgent, then sleeps in very short steps. Those limits are named at the top of `daemon/src/server.rs`, and they're guesses until a real run measures them.

`GUESTHOUSE_TIMING=1` turns on timing points at each stage of a submission, and the daemon logs them every ten seconds: from arrival to the backend, in the backend, to the doorbell, to the reply, and from the doorbell until a fence is seen to move.

The tests hold this path to a standard: `core/tests/no_allocation.rs` fails if a steady flow of submissions and signals touches the allocator, and `cargo bench -p guesthouse-core` gives the time per message. Those numbers cover parsing, bookkeeping and building the reply, and not the socket, QEMU or the guest. `daemon/examples/round_trip.rs` times a round trip through the socket against any running daemon.

## When a process fails

The GPU can stop answering a process, and nothing tells the host, so a queue that passes `QUEUE_TIMEOUT` counts as failed. The guest can't be left waiting, so its fences go up anyway. That would make a program think work ran that didn't, and that's the part to get right.

- A submission that can't run fails. The host has no message for telling the guest a device is lost, but the guest's driver takes a failed submission that way, so the program hears it as it would on real Windows.
- A second failed queue takes the whole process off the GPU: its channels close, then its fences go up. Other processes carry on.
- A process that faults counts once, however many queues it had. `FaultRate` in `core/src/fault_rate.rs` keeps the last minute of these. Too many in that time stop all of the guest's GPU work until it restarts, as a guest can start new processes for as long as it likes and each fault can stall the whole card. It's a rate because a lifetime count would catch any VM that runs for long enough.
- When that happens the daemon logs it and leaves a note in the run folder, and the app shows the note on the machine's row.

## What a message can ask for

A handler answers everything, and nothing a message says may stop the daemon. So a size the host makes room for goes through `declared_size` or `declared_count` in `messages/src/wire.rs`, shared memory access returns `MemoryError` and never panics, and address arithmetic is checked. Release builds keep overflow checks on, as a stop is better than a wrong sum. The limits are the ones a real guest can't reach:

- an answer is at most 128 KiB, the most the guest's channel carries in a packet
- the IO space must hold the whole fence area, on a page, inside the shared window, and can't move once fences exist
- video memory stops at the end of the GPU address ranges, and memory waiting for its page list stops at the guest's RAM
- a list stops at a channel's ring
- a copy stops at its memory

Past these the port is stricter than the prototype, which took such messages and did something else with them. For anything a guest really sends, the two still give the same replies.

A surface's width, height, pitch and tiling are only what its description says, and they aren't tied to the size of its memory. All the daemon's copies run in its own address space, which no process shares, so `session/copy_bounds.rs` measures each one in bytes against the allocation's real size, on both sides, and a copy that doesn't fit is refused with `UNSUCCESSFUL`. Each side is held to the same rule. A tiled surface has to fit whole, source or target, and `Backend::tiled_bytes` says how big that is, as the layout is the vendor's. `Backend::copy_pixel_bytes` says how wide the source's pixels are, and the check and the copy both use that one number. A colour the backend doesn't know counts as four bytes, and the check still holds, as it measures the bytes the copy moves.

## Jails

A guest that takes over QEMU or the daemon should get as little of the user's account as possible, so both run jailed, with nothing an unprivileged user doesn't have. The `jail` crate holds the parts: a `Plan` of paths and ports for Landlock, and two seccomp filters in `filters/`.

The daemon jails itself, in `daemon/src/jail.rs`. Landlock goes on before any thread exists, as it only covers the thread that applies it. The filter goes on once the card is open and the sockets are bound, for every thread. It's an allow-list, and a call that isn't on it writes a line to `daemon.out` and gets `ENOSYS`, so a missing call costs a feature and says which. NVIDIA's ioctls aren't filtered by number: Landlock already holds the daemon to the card's device files.

QEMU is started by `guesthouse-jail`, which applies a plan the app writes in `vm_args/qemu_jail.rs` and then becomes QEMU. Windows guests and the forwarded Linux GPU get the same jail, the latter with the card's nodes from `vm_args/forwarding_jail.rs`. passt and a bridge's helper can't work inside it, so the launcher starts them first: passt gets one end of a socket pair and QEMU the other, as `-nic socket,fd=11`, and the helper's device goes to QEMU as a descriptor too. In namespaces that's a launcher of its own in front of bubblewrap, run with `--open-only`, as the helper can't gain its privileges inside bubblewrap either. passt stays in the foreground, so it's in QEMU's process group, and the launcher notes its pid and start time for the app to find it by. A USB device is opened by the app and handed to QEMU over the monitor with `add-fd`. QEMU's filter is a deny-list, as the calls QEMU makes depend on its build and devices. When QEMU has no sound, the launcher also opens every unix socket QEMU needs, its monitors and the daemon's and the TPM's, and hands them over by number, and the filter refuses QEMU any new one. Its words then hold no monitor path, so the app finds it by its pid file. Where bubblewrap can make namespaces, it goes around the launcher too, with each path of the plan bound at its own place. `jailing.rs` decides at each start, and leaves a line for the machine's row when the jail is short of something. A kernel's Landlock gets the rights it has, each on its own, and none of it needs namespaces, so Ubuntu 24.04's limits on those only cost the bubblewrap part.

## Slots

`GUESTHOUSE_SLOT` names the VM, from 0 to 3, and unset means 0. Everything the daemon touches is named from it in `daemon/src/slot.rs`: the run folder, the guest's memory, the ports. Slot 0 has the plain names, and other slots add their number. Anything else in the variable stops the daemon, because a guess could reach another VM's files.

## The guest's memory

A GPU guest's RAM and its IO window are memfds. The app makes both before QEMU or the daemon starts, and each finds them open at the same descriptor numbers (`daemon/src/handover.rs`). Nothing of the guest is a file anyone can open by name, so `/dev/shm`'s size doesn't limit a machine, and the memory goes back as soon as the last of QEMU and the daemon has gone. Each memfd is named for its slot, as `gpupv-ram-2`, which is how the app tells from `/proc` what each slot holds when it works out how much RAM it can offer. For a QEMU the app didn't start, the daemon can make two files instead and map those (`daemon/src/memory_files.rs`), which QEMU opens by the same paths. The daemon opens them before its jail goes on. [Standalone](docs/standalone.md) says how that's used.

## The vendor line

A vendor backend implements `Backend` and depends on the `backend` crate. It never sees the core. It gets plain values, and asks for what it needs through `HostServices`. Going the other way, the core treats a vendor's private data as opaque bytes and passes them along unread. The one exception is the small record for surfaces the guest's kernel makes itself, which is our own format, in `messages/src/standard.rs`.

If you find yourself wanting to name a vendor in `core`, something belongs in a backend instead.

`backend-amd` is the second backend, through this same trait. With both built in, each guest's daemon opens the card the app named for that machine. An Intel backend should fit the same way, and there's no stub for one.

`backend-nvidia` goes through `rm::Rm` for everything it asks of the host's driver. `rm/linux.rs` is the real one and `rm/fake.rs` plays its part in tests, so the whole crate can be tested with no card.

`backend-amd` goes through amdgpu's render node. Each guest address space gets its own VM on the card, every submission names the buffers its space holds so amdgpu keeps them in place while it runs, and tiled frames reach the screen through a copy on the card's DMA engine. A frame for the viewer's ring is a copy to a surface of the slot's shape: one plain DMA copy when both have the same pitch, as the CPU reads video memory slowly, and otherwise the screen's way. The display driver's slots are linear buffers made with CPU access, so the CPU reads their stamps.

## Frames

`FrameSink` in `core/src/frames/` is the boundary between what makes the guest's frames and what shows them. A sink hears of rings of buffers and the frames in them, and doesn't know how a frame came to be. Copies to the staging surface make frames for the basic display, the indirect display driver makes its own, and the only sink is `ViewerPort` in `daemon/src/viewer.rs`, which serves `frames.sock` in the run folder.

The staging surface lies in guest pages, so a viewer can't use it where it is. The daemon keeps its own ring of slots in video memory, exported with `Backend::export_memory`, and each screen frame gets a second copy, into the next free slot. A screen frame is a copy from a primary, DWM's or a fullscreen program's, with no driver data. A windowed program's present can look the same on AMD, but it comes from a back buffer and goes to a staging surface of its own, for DWM to put in its window. The guest's image is read directly if it's the screen's size, and otherwise the staging surface is, because each slot needs the whole picture. On NVIDIA an export goes through an RM file and a GEM object on the render node, in `rm/dmabuf.rs`.

A viewer whose screen is on another GPU, as on most laptops, can't take the card's buffers. Its hello asks for frames in memory (`wants_memory`; the app decides in `app-core/src/screen_gpu.rs`, by whether a connected screen is on a GPU other than the machine's card, NVIDIA or AMD). Its slots are then sealed memfds here, mapped for the GPU with `Backend::adopt_host_pages`, and the viewer copies each frame out. The screen's copies write them as usual. The indirect display driver's frames are in guest video memory, so each one is copied over when it lands, alone on the copy engine and waited for. A finished frame goes before a screen copy that hasn't started yet, as a guest still drawing on its basic display always has the next one queued. Those viewers aren't offered HDR.

The rules:

- A ring exists only while a viewer is here, so a guest with nobody watching pays nothing. Each viewer gets new buffers, and so does each new screen size, under a new generation.
- `Slots` is the only thing that decides who has a slot: free, being written, waiting to leave, or the viewer's. A slot goes to the viewer when its frame has really left, and comes back only by a release of a slot the viewer has. Anything else a viewer says changes nothing.
- A frame goes out once its copy has finished on the GPU. A newer frame replaces one that hasn't left, and its slot is free again.
- With no free slot the frame is skipped. Nothing ever waits on a viewer.
- A ring copy reads the guest's image, so it runs before the guest's reply, in the order the prototype was proven in. The blocking order waits for it on the message thread, and the held order keeps its job and its held reply until it's done. Either way it costs the guest one more copy on the GPU. If it fails, a frame is lost and the guest hears nothing.
- Making a ring, closing one and catching up all happen in `Session::poll`, away from any message. Making a ring takes a few calls to the card, once for each viewer or screen size.
- A skipped frame might be the last one for a while, as an idle desktop draws nothing. So when a slot is free and the screen has been still for a moment, the ring fetches the picture from the staging surface. A new viewer gets its first picture the same way.
- Frames go out as SDR XRGB8888. The ring can't hold a 10-bit picture, and a plain copy can't narrow one, so those frames don't reach the viewer yet. HDR comes with the indirect display driver.

## Three layers inside a backend

Different cards and drivers differ in different ways, and those ways shouldn't get tangled. A backend is built from these parts:

- **Vendor.** The backend crate itself.
- **Generation.** A small profile of what an architecture changes: class numbers, channel types, shader version rules, which engines get a node and what the record says of them. Blackwell has run, and Ada, Ampere and Turing are there to try. Their video nodes are listed and take no work yet. A card with no profile is refused.
- **Driver branch.** A second profile, beside generations, of what a driver branch changes in its data layouts: control and allocation structure sizes on the host, and the layout of the guest's record.

Generations and branches stay separate, so they never multiply into a matrix of every generation with every branch. `Backend::open` is where a backend picks one of each. Nothing in the trait is per-generation or per-branch, so all of this stays inside the backend. In `backend-nvidia` they're in `profile/`: `host.rs` for the host's branch, `guest.rs` for the guest's, and `generation.rs`.

Cards are never listed in code as behaviour. Name, video memory, engines and copy engine details are read from the live card, and end up in `AdapterFacts`. The only per-card list a backend may keep is a label, `CardLabel::Tested` or `Experimental`, and that label changes nothing the daemon does.

## Starting up

`daemon/src/startup.rs` runs three steps:

1. `Backend::survey` looks at what's installed and reports the driver versions and branches, and what's been tested. It must not send any request to the host's driver. Reading a version from sysfs is fine, and opening the device is not.
2. `decide_start` in `backend/src/driver.rs` judges the report, the same way for every vendor:
   - a known branch at a tested version runs
   - a known branch at another point release runs, with a warning
   - an unknown branch is refused
   - host and guest on different branches are refused, until that pair has been tested
3. Only then does `Backend::open` touch the card.

Which card it opens comes from setup, which writes down the card's PCI address. The driver's numbers aren't names: a card's device file number is the order the driver probed the cards in, and the RM's device instance the order it set them up in, and the two can differ and change between boots. So `rm/listing.rs` finds the device file for the address in `/proc/driver/nvidia/gpus`, and `rm/instance.rs` asks the RM which device instance the GPU at that address is. A setup with no address works on a PC with one card, and is refused with two. This has been tested with pretend cards and not yet on a real PC with two. The Linux forwarding device in the QEMU patch is given the same address and the card's device file, looks the device instance up the same way before the guest runs, and shows its guest that card alone, as its device 0 (`hw/misc/nvfwd/card.c`).

A refusal is a plain sentence, and it comes before any request reaches the host's driver, since we'd otherwise be handing a driver structures of a size it may not expect.

A real check has to compare what it found against the branch profiles. Asking the driver for its version and repeating that back to it passes on every version, so it proves nothing.

The driver version the guest is told comes from setup, which reads it from the driver's INF (`driver_version_from_inf`). It reaches the core through `AdapterFacts::guest_driver_version`, and it's never a constant.

## The record-and-compare test

The prototype is the reference for behaviour, and the replay tool is how we check we match it.

A recording holds what the prototype received and sent in a real session, along with what the replay can't work out alone: when fences moved, when the prototype took GPU addresses for its own channels, and whether each submission reached the GPU. The format is described in `replay/src/recording.rs`.

`guesthouse-replay FILE` feeds the received frames through the real `Router`, compares every reply byte for byte, and names the first difference with a likely cause.

There's a catch to keep in mind. With no vendor backend, `RecordedAnswers` plays that part by reading vendor data out of the recording, and bytes taken from the recording can't fail to match. So the report keeps three counts apart:

- replies the core worked out alone, which are real evidence
- replies with vendor fields taken from the recording, where the rest of the reply is real evidence
- replies supplied whole by the recording, which are no evidence at all

The host's own clock in the clock answer is masked, since it can never repeat. Messages the host sends by itself are compared as a set and not in order, because in a real session their order depends on the GPU's timing.

## GPU addresses

The host hands out addresses from `GPU_RANGES`, less anything a backend keeps clear with `Backend::kept_gpu_ranges`. A guest's reservation either takes the next hand-out or, if it names an address or a window the next one doesn't fit, is placed. Placed ranges are per process, so the same address can mean different memory in two processes, and every lookup by GPU address names its space. Hand-outs step over placed ranges in all processes.

## Compute and video

Nothing here is forwarded to the host's driver for this. CUDA's memory is plain allocations and mappings, and its submissions name a ring position, which `ring_position.rs` turns into a list.

A context on a video node gets a channel on that engine, in `Channel::new`, and its commands run as they are. It's the list path, the same as any context. Vulkan's contexts on those nodes are the exception: its video queue families can also transfer, so they stay on copy channels, and a Vulkan Video program would land on one.

The guest's video runtime holds its engines and buffers with mutexes: it waits, sends work, and signals. So a sync object that has no fence has tokens, in `sync_table.rs`. A wait on a queue takes one when its gate opens, and a signal gives it back once the queue has got there. Waits stand in line, first come first served, because a copy and the encoder that reads what it wrote share a mutex and nothing else orders them. Without the tokens all the encoders run at once on shared state, and without the line an encoder now and then gets an old picture. The only sign of either is damaged video.

NVIDIA limits how many encoding sessions a GeForce card runs at once, and this project doesn't lift that. When a guest asks to open one, the daemon takes a slot from the host's driver with the same control a native session uses, and passes on the driver's answer, in `encode_sessions.rs`. So the count is the driver's own, shared with the host and every other VM, and there's no number of ours to go stale. A slot goes back when its session or its process ends, when the guest restarts, and on its own when a daemon dies.

## Host sensors

`backend-nvidia/src/sensors` reads the card through the vendor's management library, loaded at run time. One daemon per host does this, on its own thread, and writes a snapshot to a small shared file. The message path only reads that snapshot. With no library, or no fresh snapshot, the performance queries give their fixed answers.

## The app

`app-core` needs no display, so nearly all of the app can be tested alone. Each machine is one JSON file. Anything slow runs as a task: `guesthouse-task` is a process of its own that outlives the app and leaves its steps in a small file, so the app can close at any moment and read where things are when it opens again. The app also updates itself, in `updates.rs`: it runs the installed `install.sh --update --from-app=PID` with every VM off, and pkexec asks for the password in a window if root is needed.

`Context` holds what the code needs from the PC, and tests swap its parts for stand-ins. A VM's QEMU arguments are built as data in `vm_args`. How a VM and the daemon come up is behind `VmHost`, and everything that knows the daemon's start-up is in `daemon.rs`, so a change there stays there.

Nothing is relative to a project folder. Machines are in the data store, which `~/.config/guesthouse/store` can move, sockets are in the XDG runtime folder, the log is in the state folder, and QEMU is in a program folder found from where it runs. Each machine has its own token for the guest's agent. That folder isn't in this repo. Without it the app uses the system's QEMU, and it finds what goes into a guest in the repo it was built from, if there is one.

What goes into a Windows guest is in `guest/`. The app finds it in the program folder, or in the repo if it's a build of the repo. There's no token or password in there: each machine gets its own at install, and they go on an answer disc the app makes and deletes afterwards. Having those files doesn't mean a GPU can work, so a build of the repo also has to find a QEMU with the patch before it offers one.

Ubuntu guests take a route of their own, in `crates/app-core/src/linux/` and `actions/ubuntu.rs`, with what goes into a guest in `linux/` at the top of the repo. Ubuntu's installer runs with no hands from a seed disc, and the host's driver reaches the guest on a read-only image, made from the files of the driver that's loaded. An unknown driver branch turns the GPU off and the VM still boots, and another point release warns. That GPU is a preview so far, and the app says so.

The guest's driver calls go through `nvfwd`, a device in the QEMU patch, to the host's driver, on the vCPU that made them. No daemon carries the calls on this route. The device works on a private copy of each call and holds it to an allow list and to limits per VM, and `linux/README.md` has the details. QEMU runs in the same jail as Windows' here, with the card's nodes added.

The viewer only knows `FrameSource`, which has three kinds. `SocketFrames` takes a guest's frames from the frame socket, with a new texture for each frame, and gives a slot back when GTK lets go of its texture. `ForwardedFrames` takes an Ubuntu guest's frames from its QEMU device. `DbusFrames` listens to QEMU's D-Bus display, which is all that firmware, an install and a plain VM have, and a guest on a GPU has it too until its first frame and again after a reset.

### Windows' screen

A Windows machine ends up on our display driver, from `drivers/windows-display/`, with no hands. `actions/display.rs` does this on every boot, so a new machine gets it on its first GPU boot, in the install's own task, and an existing one gets it at its next start. It asks Windows for the device by its hardware id, and if it's missing it sends the package through the agent, runs `sign-and-install.ps1` as SYSTEM, and leaves the logon task that makes it the only display. The certificate is made in that guest, and the settings say what it means. Then `data/apply-display.ps1` runs in the user's session, with the machine's size, rate and HDR, and says what Windows really gave.

Three things can say what's on the screen, and the app believes the one that knows. Windows knows the rate. The daemon knows the size, whether it's HDR and whether a frame has landed, as it has the frames, and leaves that in `display-ring` in the run folder (`daemon/src/display_note.rs`). `display.json` in the machine's folder keeps the rest: whether the driver is in, which build, and why not. The row goes by those and never by what was asked for. A rate changed by hand inside Windows only shows after the next change or boot.

Nothing here can stop a machine. A driver that won't install is taken out with `guest/display-remove.ps1`, certificate and all. Windows goes back to the basic display by itself, and the reason stays in `display.json`. A start won't try that build again, but a new build or the button in the settings will.

An open ring proves nothing about the picture, so only a frame that has landed on it counts, and the daemon's note says so. A healthy driver can't be mistaken for a dead one here, even on a still desktop or at the logon screen: it only opens a ring while it's holding a frame from Windows, so the first frame lands within a few milliseconds of the ring. The session's script isn't trusted over that either. If it says nothing, as it does with nobody logged on, frames still decide. A build that's waiting for a restart gets no credit, as the frames until then are the old build's.

When Windows says it's on our display and no frame comes, what happens depends on what's going on. At boot, a build that has never sent a picture goes at once. One that has gets a second boot, as a slow boot shouldn't cost it, and two blank boots in a row take it out, as the screen would otherwise stay black for good. A change to a running machine never takes it out and only leaves a note on the row. Neither does a guest whose GPU work the daemon has stopped.

Once ours is the only display, the basic display is off, so the viewer has nothing to fall back on. If the guest's GPU work stops, or the daemon dies, the picture stays as it was, with no live frames, and the way out is Shut Down or Force Off. The row's GPU note says so.

The built driver isn't in git. It's in the program folder's `display-driver/` if the packaging was given one, with its licences, scripts and settings, and with `package.txt` to tell builds apart. A build with none says so on the row.

HDR is the machine's setting and the screen's ability together, because a viewer that can't take an HDR frame gets no frame. Only the app can find out about the screen, so it leaves the answer in its settings (`window/screen_hdr/`) for tasks to read. It asks the compositor through Wayland's colour management protocol: a monitor whose description uses PQ is in HDR. That works on KWin, on Mutter 48 and later, and on any other compositor that has the protocol. Without it, Mutter's D-Bus can still answer on an older GNOME. Anywhere else nobody can tell, which means SDR, and `GUESTHOUSE_SCREEN_HDR` says yes or no by hand.

A guest's driver can send HDR frames and its hardware cursor over the same socket. The viewer offers `XB30` only if the display has it, so an SDR desktop never meets an HDR frame. GTK needs `GDK_DEBUG=color-mgmt` for Wayland's colour management, so `main.rs` sets it before GTK starts. It's a debug flag, so a new GTK could change it. The cursor is the host's pointer wearing the guest's picture, as that moves with no delay. While the mouse is captured there's no pointer, so `cursor_layer` draws it in the window.

## Not here yet

Not ported yet: the arena (video memory the guest's CPU can reach), the experiment switches in the prototype's run folder, and turning 10-bit frames into 8-bit ones for an app's staging surface.
