# Windows on an Intel card

This doesn't work yet. It's the first step towards Intel's integrated graphics, starting with Lunar Lake (Arc 130V and 140V) on Linux's `xe` driver.

## What's there

The daemon built with the `intel` feature answers Intel's Windows driver in the guest well enough for it to load and make its devices, contexts, queues and memory. Nothing runs on the card yet: every bit of work the guest sends comes back as finished without running, so the guest's screen won't show anything drawn by the GPU. What this step is for is the log. Every exchange that is Intel's own gets a line in `daemon.out` starting with `intel`, so one run tells me what the real backend has to do.

It only takes Intel's driver 32.0.101.9175 in the guest, as the data it passes around changes from one release to the next.

## intel-probe

`intel-probe` prints what the host's `xe` driver says about your card: its PCI ids, memory, engines, GT and topology, the hardware config table, the GuC and HuC versions and the clocks, then the record a guest would be given. It only asks the driver questions. It makes no buffer or queue, so nothing runs on the card and your desktop isn't touched.

```sh
cargo run --release -p guesthouse-backend-intel --bin intel-probe
```

Give it a PCI address, like `0000:00:02.0`, if you have more than one Intel card. If you have a Lunar Lake laptop and want to help, open an issue with its output.
