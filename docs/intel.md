# Windows on an Intel card

This doesn't work yet, and none of it has met a guest. It's the first step towards Intel's integrated graphics, starting with Lunar Lake (Arc 130V and 140V) on Linux's `xe` driver.

## What it's meant to do

The daemon built with the `intel` feature is meant to answer Intel's Windows driver in the guest well enough for it to load and make its devices, contexts, queues and memory. That's untested. Nothing runs on the card: every bit of work the guest sends comes back as finished without running, so the guest's screen won't show anything drawn by the GPU. What this step is for is the log. Every exchange that is Intel's own gets a line in `daemon.out` starting with `intel`, and with `GUESTHOUSE_VERBOSE` set the log holds the whole of each one too, so one run should tell me how far the guest got and where it stopped.

It only takes Intel's driver 32.0.101.9175 in the guest, as the data it passes around changes from one release to the next.

## A desktop you can use

Setting `GUESTHOUSE_INTEL_SOFTWARE_DESKTOP` in the daemon's environment refuses the guest Intel's Direct3D 9, 10 and 11 drivers. Windows should then draw its desktop in software, so you can use it, while Direct3D 12, OpenCL, Level Zero and Vulkan still come to the card and the log. It's off unless set, the log says which way it is, and I haven't tried it on a guest.

## intel-probe

`intel-probe` prints what the host's `xe` driver says about your card: its PCI ids, memory, engines, GT and topology, the hardware config table, the GuC and HuC versions and the clocks, then the record a guest would be given. It only asks the driver questions. It makes no buffer or queue, so nothing runs on the card and your desktop isn't touched.

```sh
cargo run --release -p guesthouse-backend-intel --bin intel-probe
```

Give it a PCI address, like `0000:00:02.0`, if you have more than one Intel card. If you have a Lunar Lake laptop and want to help, open an issue with its output.
