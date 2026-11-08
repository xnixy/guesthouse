#!/usr/bin/env python3
"""refresh.sh in a scratch root: a guest set up with older scripts takes a driver image's, the way a fresh
setup would, keeps what it had when that fails, and gpu-up.sh hands over to the new scripts at boot."""
import os
import shutil
import subprocess
import tempfile
import unittest

from test_check import FakeGuest
from test_gpu_up import Guest, write

GUEST = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "guest")
OLD_MARK = "# the older scripts\n"


class Root:
    """A guest root with older scripts installed, and an image whose setup folder is this tree's."""

    def __init__(self, folder):
        self.root = os.path.join(folder, "root")
        self.bin = os.path.join(folder, "bin")
        self.calls = os.path.join(folder, "calls")
        self.image = os.path.join(folder, "image")
        shutil.copytree(GUEST, os.path.join(self.image, "setup"), ignore=shutil.ignore_patterns("__pycache__"))
        self.installed = os.path.join(self.root, "usr/lib/nvfwd")
        shutil.copytree(GUEST, self.installed, ignore=shutil.ignore_patterns("__pycache__"))
        write(os.path.join(self.installed, "check.sh"), OLD_MARK)
        write(os.path.join(self.installed, "stale.sh"), "")
        for folder_name in ("etc/default/grub.d", "usr/local/sbin", "etc/modprobe.d", "etc/udev/rules.d"):
            os.makedirs(os.path.join(self.root, folder_name), exist_ok=True)
        open(self.calls, "w").close()
        self.fake("id", "echo 0")
        self.fake("uname", "echo 7.0.0-42-generic")
        for name in ("systemctl", "update-grub", "semanage", "restorecon"):
            self.fake(name, "exit 0")

    def fake(self, name, body):
        write(os.path.join(self.bin, name), f'#!/bin/bash\necho "{name} $*" >> "$CALLS"\n{body}\n', True)

    def run(self, *arguments):
        environment = dict(os.environ, NVFWD_ROOT=self.root, CALLS=self.calls, PATH=f"{self.bin}:/usr/bin:/bin")
        return subprocess.run(["bash", os.path.join(self.image, "setup/refresh.sh"), *arguments],
                              env=environment, capture_output=True, text=True)

    def path(self, name):
        return os.path.join(self.root, name)

    def read(self, name):
        with open(self.path(name)) as f:
            return f.read()

    def lines(self, start):
        with open(self.calls) as f:
            return [line.strip() for line in f if line.startswith(start)]


class UbuntuRefresh(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.guest = Root(folder.name)
        # What Ubuntu's seed brought and the image has no copy of.
        write(os.path.join(self.guest.installed, "Xwayland"), "#!/bin/sh\n")

    def test_the_images_scripts_and_settings_go_in_and_the_seeds_extras_stay(self):
        result = self.guest.run("--if-changed")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotEqual(self.guest.read("usr/lib/nvfwd/check.sh"), OLD_MARK)
        self.assertTrue(os.path.exists(self.guest.path("usr/lib/nvfwd/Xwayland")))
        self.assertIn("PowerKeyIgnoreInhibited=yes", self.guest.read("etc/systemd/logind.conf.d/90-nvfwd.conf"))
        self.assertEqual(self.guest.lines("update-grub"), ["update-grub"], "its settings were new")
        self.assertIn("systemctl daemon-reload", self.guest.lines("systemctl"))
        self.assertIn("systemctl reload systemd-logind", self.guest.lines("systemctl"))
        self.assertFalse(os.path.exists(self.guest.path("usr/bin/Xwayland")), "no diversion, no wrapper")
        self.assertFalse(os.path.exists(self.guest.path("var/lib/nvfwd/guest-files.previous")))
        self.assertTrue(self.guest.read("var/lib/nvfwd/guest-files").strip())

    def test_the_same_image_again_does_nothing(self):
        self.guest.run("--if-changed")
        open(self.guest.calls, "w").close()
        self.assertEqual(self.guest.run("--if-changed").returncode, 3)
        self.assertEqual(self.guest.lines("systemctl"), [])
        self.assertEqual(self.guest.run().returncode, 0, "asked by hand, it runs anyway")

    def test_the_wrapper_is_refreshed_where_the_first_boot_diverted_xwayland(self):
        write(self.guest.path("usr/bin/Xwayland.real"), "")
        self.guest.run()
        self.assertEqual(self.guest.read("usr/bin/Xwayland"), "#!/bin/sh\n")

    def test_a_failure_keeps_the_scripts_it_had_and_says_why(self):
        self.guest.fake("systemctl", '[ "$1" != daemon-reload ]')
        result = self.guest.run("--if-changed")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertEqual(self.guest.read("usr/lib/nvfwd/check.sh"), OLD_MARK)
        self.assertTrue(os.path.exists(self.guest.path("usr/lib/nvfwd/stale.sh")))
        self.assertEqual(sorted(os.listdir(self.guest.installed)), sorted(os.listdir(os.path.join(self.guest.image, "setup")) + ["stale.sh", "Xwayland"]))
        self.assertFalse(os.path.exists(self.guest.path("usr/lib/nvfwd.new")))
        self.assertIn("installing the settings failed", self.guest.read("var/lib/nvfwd/guest-files-problem"))
        self.assertFalse(os.path.exists(self.guest.path("var/lib/nvfwd/guest-files")), "it tries again next boot")

    def test_it_needs_root(self):
        self.guest.fake("id", "echo 1000")
        self.assertEqual(self.guest.run().returncode, 1)
        self.assertEqual(self.guest.read("usr/lib/nvfwd/check.sh"), OLD_MARK)


class OwnRefresh(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.guest = Root(folder.name)
        write(self.guest.path("etc/nvfwd/own-install.conf"), "family=fedora\nlibraries=/usr/lib64\ndesktop=plain\n")
        link = self.guest.path("etc/systemd/system/display-manager.service")
        os.makedirs(os.path.dirname(link))
        os.symlink("/usr/lib/systemd/system/plasmalogin.service", link)
        write(self.guest.path("usr/lib/systemd/user/plasma-powerdevil.service"), "")

    def test_a_hand_installed_guest_takes_the_images_scripts_and_keeps_its_choices(self):
        write(self.guest.path("etc/xdg/powerdevilrc"), "mine\n")
        write(self.guest.path("sys/fs/selinux/enforce"), "1\n")
        result = self.guest.run("--if-changed")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotEqual(self.guest.read("usr/lib/nvfwd/check.sh"), OLD_MARK)
        self.assertFalse(os.path.exists(self.guest.path("usr/lib/nvfwd/stale.sh")))
        self.assertEqual(self.guest.read("etc/xdg/powerdevilrc"), "mine\n", "only written when missing")
        self.assertIn("desktop=plain\n", self.guest.read("etc/nvfwd/own-install.conf"))
        self.assertIn("After=nvfwd-gpu.service", self.guest.read("etc/systemd/system/plasmalogin.service.d/nvfwd.conf"))
        self.assertIn("PowerKeyIgnoreInhibited=yes", self.guest.read("etc/systemd/logind.conf.d/90-nvfwd.conf"))
        self.assertEqual(len(self.guest.lines("restorecon")), 1, "SELinux labels stay right")
        self.assertIn("systemctl enable nvfwd-gpu.service nvfwd-desktop-watch.service", self.guest.lines("systemctl"))
        self.assertEqual(self.guest.lines("dnf"), [], "no packages at boot")
        self.assertEqual(self.guest.run("--if-changed").returncode, 3)

    def test_a_missing_powerdevilrc_is_written(self):
        self.guest.run()
        self.assertIn("PowerButtonAction=8", self.guest.read("etc/xdg/powerdevilrc"))


class GpuUpHandsOver(unittest.TestCase):
    def guest(self, refresh_exit):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        guest = Guest(folder.name)
        guest.add_device()
        guest.add_layers()
        # The image's own refresh.sh, and the scripts it would leave behind.
        guest.fake("mount", f"""d=$NVFWD_ROOT/opt/nv/driver; mkdir -p $d/lib $d/bin $d/json/egl_external_platform.d $d/setup
printf 'version=615.78.08\\nid=615.78.08-abc\\ntested=yes\\n' > $d/manifest; touch $d/sources.tar
touch $d/json/egl_external_platform.d/10_nvidia_wayland.json
printf '#!/bin/bash\\necho "refresh $*" >> "$CALLS"\\necho broke > $NVFWD_ROOT/var/lib/nvfwd/guest-files-problem\\nexit {refresh_exit}\\n' > $d/setup/refresh.sh""")
        os.makedirs(os.path.join(guest.root, "var/lib/nvfwd"), exist_ok=True)
        write(os.path.join(guest.root, "usr/lib/nvfwd/gpu-up.sh"),
              '#!/bin/bash\necho "new gpu-up $NVFWD_REFRESHED" >> "$CALLS"\n', True)
        return guest

    def test_new_scripts_take_over_from_the_old(self):
        guest = self.guest(0)
        guest.run()
        self.assertEqual(guest.called("refresh --if-changed"), 1)
        self.assertEqual(guest.called("new gpu-up 1"), 1)
        self.assertEqual(guest.called("modprobe nvidia"), 0, "the new scripts do the rest")

    def test_with_nothing_new_it_carries_on(self):
        guest = self.guest(3)
        guest.run()
        self.assertEqual(guest.called("new gpu-up"), 0)
        self.assertEqual(guest.state()["gpu"], "on")

    def test_a_failed_refresh_still_brings_the_gpu_up_and_says_why(self):
        guest = self.guest(1)
        guest.run()
        self.assertEqual(guest.state()["gpu"], "on")
        with open(os.path.join(guest.root, "run/motd.d/49-nvfwd-refresh")) as f:
            self.assertIn("weren't refreshed from the driver image: broke", f.read())


class CheckSaysWhenScriptsAreBehind(unittest.TestCase):
    def test_older_scripts_and_a_failed_refresh_are_said(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        guest = FakeGuest(folder.name)
        guest.file("opt/nv/driver/setup/refresh.sh", "")
        guest.file("var/lib/nvfwd/guest-files", "0000000000000000\n")
        self.assertIn("older than the driver image's", guest.run().stdout)
        guest.file("var/lib/nvfwd/guest-files-problem", "copying the scripts failed\n")
        self.assertIn("weren't refreshed from the driver image: copying the scripts failed", guest.run().stdout)


if __name__ == "__main__":
    unittest.main()
