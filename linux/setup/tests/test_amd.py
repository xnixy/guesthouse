#!/usr/bin/env python3
"""The AMD card's scripts in a scratch root, with every system command a fake. The rules under test:
the host hears which way the guest's 3D went, a Mesa with no native context is built only when it
has to be, and a guest that isn't on an AMD card is left alone."""
import os
import shutil
import stat
import subprocess
import tempfile
import unittest

GUEST = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "guest")
RADEONSI = "AMD Radeon 610M (radeonsi, gfx1036, LLVM 20.1.2, DRM 3.64, 7.0.0)"
VIRGL = "virgl (AMD Radeon 610M (radeonsi, gfx1036))"


def write(path, text, executable=False):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)
    if executable:
        os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)


class Guest:
    def __init__(self, folder):
        self.root = os.path.join(folder, "root")
        self.bin = os.path.join(folder, "bin")
        self.scripts = os.path.join(folder, "scripts")
        self.calls = os.path.join(folder, "calls")
        shutil.copytree(GUEST, self.scripts)
        os.makedirs(self.root)
        open(self.calls, "w").close()
        write(os.path.join(self.root, "dev/ttyS0"), "")
        for name in ("modprobe", "systemctl", "udevadm"):
            self.fake(name, "exit 0")
        self.renderers(VIRGL, "Virtio-GPU Venus (AMD Radeon 610M (RADV GFX1036))", "venus")
        self.mesa("26.0.8-1ubuntu0.3")

    def fake(self, name, body):
        write(os.path.join(self.bin, name), f'#!/bin/bash\necho "{name} $*" >> "$CALLS"\n{body}\n', True)

    def boot_on(self, mode):
        write(os.path.join(self.root, "sys/firmware/qemu_fw_cfg/by_name/opt/guesthouse/amd-mode/raw"), mode)

    def renderers(self, gl, device, driver):
        self.fake("eglinfo", f"echo 'OpenGL core profile renderer: {gl}'")
        self.fake("vulkaninfo", f"echo '    deviceName         = {device}'; echo '    driverName         = {driver}'")

    def mesa(self, version):
        self.fake("dpkg-query", f"echo -n {version}")

    def run(self, script):
        environment = dict(os.environ, NVFWD_ROOT=self.root, CALLS=self.calls, PATH=f"{self.bin}:{os.environ['PATH']}")
        return subprocess.run(["bash", os.path.join(self.scripts, script)], env=environment,
                              capture_output=True, text=True)

    def said(self):
        with open(os.path.join(self.root, "dev/ttyS0")) as f:
            return [line.strip() for line in f]

    def called(self, text):
        with open(self.calls) as f:
            return sum(text in line for line in f)


class AmdUp(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.guest = Guest(folder.name)

    def up(self):
        result = self.guest.run("amd/amd-up.sh")
        self.assertEqual(result.returncode, 0, result.stderr)
        return self.guest.said()

    def test_a_boot_that_is_not_on_an_amd_card_says_nothing(self):
        self.assertEqual(self.up(), [])
        self.assertEqual(self.guest.called("systemctl"), 0)

    def test_venus_says_its_vulkan_device(self):
        self.guest.boot_on("venus")
        said = self.up()
        self.assertEqual(len(said), 1)
        self.assertTrue(said[0].startswith("guesthouse-amd venus Virtio-GPU Venus"), said)
        self.assertEqual(self.guest.called("start --no-block"), 0, "Venus needs no build")

    def test_a_mesa_with_native_context_is_not_built_again(self):
        self.guest.boot_on("native")
        self.guest.renderers(RADEONSI, "AMD Radeon 610M (RADV GFX1036)", "radv")
        said = self.up()
        self.assertTrue(said[0].startswith(f"guesthouse-amd native {RADEONSI}"), said)
        self.assertEqual(self.guest.called("start --no-block"), 0)

    def test_the_distros_mesa_on_native_context_starts_the_build(self):
        self.guest.boot_on("native")
        self.assertEqual(self.up(), [])
        self.assertEqual(self.guest.called("systemctl start --no-block nvfwd-mesa-build.service"), 1)

    def test_a_mesa_built_here_that_still_is_not_radeonsi_is_a_failure(self):
        self.guest.boot_on("native")
        self.guest.mesa("26.0.8-1ubuntu0.3+guesthouse1")
        said = self.up()
        self.assertTrue(said[0].startswith("guesthouse-amd failed GL is on virgl"), said)
        self.assertEqual(self.guest.called("start --no-block"), 0)

    def test_radv_has_to_be_there_as_well_as_radeonsi(self):
        self.guest.boot_on("native")
        self.guest.renderers(RADEONSI, "llvmpipe (LLVM 20.1.2, 256 bits)", "llvmpipe")
        self.assertTrue(self.up()[0].startswith("guesthouse-amd failed Vulkan is on llvmpipe"))

    def test_the_nvidia_step_leaves_an_amd_boot_alone(self):
        self.guest.boot_on("native")
        for name in ("blkid", "mount", "umount", "mountpoint", "ldconfig"):
            self.guest.fake(name, "exit 0")
        result = self.guest.run("gpu-up.sh")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.guest.said(), [], "the AMD step does the talking")
        self.assertFalse(os.path.exists(os.path.join(self.guest.root, "run/nvfwd/state")))


class MesaHook(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.guest = Guest(folder.name)

    def hook(self):
        result = self.guest.run("amd/mesa-hook.sh")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_an_update_of_mesa_on_an_amd_card_starts_the_build(self):
        self.guest.boot_on("native")
        self.hook()
        self.assertEqual(self.guest.called("start --no-block nvfwd-mesa-build.service"), 1)

    def test_a_mesa_built_here_or_a_guest_on_nvidia_is_left_alone(self):
        self.hook()
        self.guest.boot_on("native")
        self.guest.mesa("26.0.8-1ubuntu0.3+guesthouse1")
        self.hook()
        self.assertEqual(self.guest.called("systemctl"), 0)

    def test_a_hook_that_cannot_start_the_build_does_not_fail_the_package_manager(self):
        self.guest.boot_on("native")
        self.guest.fake("systemctl", "exit 5")
        self.hook()


class MesaService(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.guest = Guest(folder.name)
        self.guest.fake("pgrep", "exit 1")

    def service(self):
        result = self.guest.run("amd/mesa-service.sh")
        self.assertEqual(result.returncode, 0, result.stderr)
        return self.guest.said()

    def test_a_failed_build_says_why(self):
        self.guest.boot_on("native")
        self.guest.fake("df", "echo header; echo x 1 1 1024 x /")
        said = self.service()
        self.assertEqual(len(said), 1, said)
        self.assertTrue(said[0].startswith("guesthouse-amd failed it needs 8 GiB free"), said)

    def test_32_bit_mesa_is_refused_rather_than_left_behind(self):
        self.guest.boot_on("native")
        self.guest.fake("df", "echo header; echo x 1 1 99999999 x /")
        self.guest.fake("dpkg-query", 'case "$*" in *Architecture*) echo mesa-vulkan-drivers:i386 ;; '
                                      '*) echo -n 26.0.8-1ubuntu0.3 ;; esac')
        said = self.service()
        self.assertIn("32-bit Mesa packages are installed", said[0])

    def test_a_working_native_context_needs_no_build(self):
        self.guest.boot_on("native")
        self.guest.renderers(RADEONSI, "AMD Radeon 610M (RADV GFX1036)", "radv")
        said = self.service()
        self.assertTrue(said[0].startswith("guesthouse-amd native"), said)
        with open(self.guest.calls) as f:
            self.assertFalse([line for line in f if line.startswith("apt-get")])


if __name__ == "__main__":
    unittest.main()
