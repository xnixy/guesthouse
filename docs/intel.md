# Windows on an Intel card

This doesn't work yet, and none of it has met a guest. It's the first step towards Intel's integrated graphics, starting with Lunar Lake (Arc 130V and 140V) on Linux's `xe` driver. Only the Arc 140V, PCI id 8086:64A0, is offered so far.

## What it's meant to do

The daemon built with the `intel` feature is meant to answer Intel's Windows driver in the guest well enough for it to load and make its devices, contexts, queues and memory. That's untested. Nothing runs on the card: every bit of work the guest sends comes back as finished without running, so the guest's screen won't show anything drawn by the GPU. The desktop may be black, or stop moving. What this step is for is the log. Every exchange that is Intel's own gets a line in `daemon.out` starting with `intel`, and with `GUESTHOUSE_VERBOSE` set the log holds the whole of each one too, so one run should tell me how far the guest got and where it stopped.

It only takes Intel's driver 32.0.101.9175 in the guest, as the data it passes around changes from one release to the next.

## Trying it

You need a Lunar Lake laptop, Linux with the `xe` driver on that card, and a test build of Guesthouse. It takes a Windows 11 ISO and about an hour.

1. Download Intel's graphics driver 32.0.101.9175 for Arc from Intel's site. The file is `gfx_win_101.9175.exe`, about 860 MB. Other releases are refused.
2. Make sure `7z` can unpack it. Intel's installer keeps its files in a RAR archive, and many distributions' `7z` leave RAR out, so it lists the file and then fails to unpack it. The app says so when that happens. 7-Zip's own Linux build from 7-zip.org works. Put it on your PATH as `7z`.
3. In the app, choose New Machine and pick your Windows ISO. Pick the Intel card. The dialog asks for Intel's installer. Choose the file, and it takes the drivers out of it. This reads the whole archive, so it takes a few minutes and about 1.8 GB.
4. Create the machine and let it install. The install is the same as for the other cards. It checks at the end that the driver's files reached Windows, and puts the loaders from Intel's INF in Windows' system folders (see "What it changes in Windows").
5. Start the machine. The app starts the daemon with its full log on. Wait a minute or two after the desktop comes up, even if it's black.
6. Run this in a terminal. `MACHINE` is the name of the machine's folder in `~/.local/share/guesthouse/machines`:

   ```sh
   ~/.local/lib/guesthouse/current/guesthouse-task dxdiag MACHINE
   ```

   It works through the app's agent, so nobody needs to look at the screen, in two parts. First a script of the app's, `api-check.ps1`, tries each API against the card: Direct3D 12 and 11 on every adapter, OpenCL, Level Zero, Vulkan and OpenGL (on a window that is never shown). Each API runs in a PowerShell process of its own with a one minute limit, so a hang or a crash in one costs only its own lines, and the report says how each ended. Direct3D and OpenCL run a second time in the 32-bit PowerShell, as the 32-bit drivers are a separate set. Nothing is installed or downloaded for this. Then Windows' own `dxdiag` runs, which makes Direct3D devices on the card too. The first part waits up to ten minutes and the second up to five. When it's done it puts four files in `test-report` in the machine's folder: `api-check.txt`, `dxdiag.txt`, `daemon.out`, and `intel-probe.txt`, which says what your host's driver knows about the card. `guesthouse-task api-check MACHINE` is the same step under another name.
7. Open an issue and attach those four files. If step 6 says the agent didn't answer, or dxdiag never finished, the guest has probably stopped. Then attach `daemon.out` from `$XDG_RUNTIME_DIR/guesthouse/run` (usually `/run/user/1000/guesthouse/run`) and say so, with `api-check.txt` if it has anything in it. `daemon.out` is the most useful of them.

`api-check.txt` starts with the loader files Windows has (OpenCL.dll, vulkan-1.dll, ze_loader.dll and so on, in System32 and SysWOW64), then has a section per API in the form of the call (`->`), its return code (`<-`), and any names returned:

```
-- opencl, 64-bit PowerShell --
  -> clGetPlatformIDs
  <- clGetPlatformIDs = 0, 1 platform(s)
     clGetPlatformInfo(NAME) = 0 "Intel(R) OpenCL Graphics"
     -> clCreateContext
     <- clCreateContext = -33 (null)
  finished
  result: exit code 0 (0x00000000)
```

A line with `->` and no `<-` after it is the call that hung or crashed the process, and `result:` says whether it exited, crashed (an exit code like 0xC0000005) or timed out. The report ends with `api-check done`.

You can shut the machine down afterwards. If you build from source, `guesthouse-task` and `intel-probe` are in `target/release`, and `cargo build --release -p guesthouse-app-core -p guesthouse-daemon --features guesthouse-daemon/intel` makes the daemon.

## A desktop you can use

Setting `GUESTHOUSE_INTEL_SOFTWARE_DESKTOP` in the daemon's environment refuses the guest Intel's Direct3D 9, 10 and 11 drivers. Windows should then draw its desktop in software, so you can use it, while Direct3D 12, OpenCL, Level Zero and Vulkan still come to the card and the log. It's off unless set, the log says which way it is, and I haven't tried it on a guest.

## intel-probe

`intel-probe` prints what the host's `xe` driver says about your card: its PCI ids, memory, engines, GT and topology, the hardware config table, the GuC and HuC versions and the clocks, then the record a guest would be given. It only asks the driver questions. It makes no buffer or queue, so nothing runs on the card and your desktop isn't touched. A release has it beside `guesthouse-task`, and step 6 runs it for you.

```sh
cargo run --release -p guesthouse-backend-intel --bin intel-probe
```

Give it a PCI address, like `0000:00:02.0`, if you have more than one Intel card. If you have a Lunar Lake laptop and want to help, open an issue with its output.

## What it changes in Windows

At install, Windows gets the user-mode part of Intel's driver in its host driver store, under the folder name the daemon gives it. Not the kernel driver, the firmware, or Intel's media software.

It also gets the libraries Intel's INF copies to Windows' system folders, which programs find by name and which lead them to Intel's OpenCL, Vulkan and Level Zero drivers. The app reads the list from the INF when the driver is unpacked (the sections for the card that copy to directory 11 or `10,SysWow64`), leaving out Intel's control library, media libraries and tools such as `vulkaninfo`, and the guest's setup script places them from the store copy. Every start checks them again and puts back any that are missing. A file is only copied when Windows lacks it or has an older one, which is the INF's own rule, so Windows' own `opengl32.dll` stays. For driver 32.0.101.9175 on the Arc 140V:

- `System32`: `vulkan-1.dll` and `vulkan-1-999-0-0-0.dll` (both from `vulkan-1-64.dll`), `OpenCL.dll` (from `Intel_OpenCL_ICD64.dll`), `ze_loader.dll`, `ze_validation_layer.dll`, `ze_tracing_layer.dll`, `ze_intel_gpu_raytracing.dll`.
- `SysWOW64`: `vulkan-1.dll` and `vulkan-1-999-0-0-0.dll` (from `vulkan-1-32.dll`), `OpenCL.dll` (from `Intel_OpenCL_ICD32.dll`), `msvcp140.dll` and `vcruntime140.dll` (from `msvcp140_32.dll` and `vcruntime140_32.dll`, the C++ runtime the 32-bit drivers need). It also gets one registry value, `HKLM\SOFTWARE\INTEL\IGFX\D3D12\EnableTrinity` set to 2, which should make Intel's Direct3D 12 driver load its older module, so a run doesn't depend on a list inside the driver. I haven't confirmed that 2 is the value that does it.
