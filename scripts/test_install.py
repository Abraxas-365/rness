#!/usr/bin/env python3
"""Offline installer acceptance; never writes to the real home directory."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[1]


class InstallerTests(unittest.TestCase):
    def test_install_upgrade_and_preserve_user_configuration(self):
        with tempfile.TemporaryDirectory(prefix="rness install ") as temporary:
            root = Path(temporary)
            home = root / "home"
            home.mkdir()
            binary = root / "test binary"
            binary.write_text("#!/bin/sh\nexit 0\n")
            binary.chmod(0o755)
            env = dict(os.environ, HOME=str(home))
            command = ["bash", str(REPO / "install.sh"), "--binary", str(binary)]

            def run(*args):
                return subprocess.run(command + list(args), env=env, cwd=root,
                                      capture_output=True, text=True)

            result = run()
            self.assertEqual(result.returncode, 0, result.stderr)
            installed = home / ".local/bin/rness"
            self.assertEqual(installed.read_bytes(), binary.read_bytes())
            config = home / ".rness"
            for source in (REPO / "flavors/default").rglob("*"):
                if source.is_file():
                    self.assertEqual(source.read_bytes(), (config / source.relative_to(REPO / "flavors/default")).read_bytes())
            (config / "init.lua").write_text("-- user configuration\n")
            (config / "private.txt").write_text("keep me")
            snapshot = {str(p.relative_to(config)): p.read_bytes() for p in config.rglob("*") if p.is_file()}
            binary.write_text("#!/bin/sh\nexit 42\n")
            self.assertNotEqual(run().returncode, 0)
            self.assertNotEqual(installed.read_bytes(), binary.read_bytes())
            result = run("--replace-binary")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(installed.read_bytes(), binary.read_bytes())
            self.assertEqual(snapshot, {str(p.relative_to(config)): p.read_bytes() for p in config.rglob("*") if p.is_file()})
            self.assertFalse(list(installed.parent.glob(".rness-install.*")))

    def test_existing_symlinks_and_invalid_arguments(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "binary"
            binary.write_text("#!/bin/sh\nexit 0\n")
            binary.chmod(0o755)
            config_target = root / "existing"
            config_target.mkdir()
            (root / ".rness").symlink_to(config_target, target_is_directory=True)
            env = dict(os.environ, HOME=str(root))
            command = ["bash", str(REPO / "install.sh"), "--binary", str(binary)]
            result = subprocess.run(command, env=env, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(list(config_target.iterdir()), [])
            installed = root / ".local/bin/rness"
            installed.unlink()
            installed.symlink_to(binary)
            self.assertNotEqual(subprocess.run(command + ["--replace-binary"], env=env, capture_output=True).returncode, 0)
            self.assertEqual(binary.read_text(), "#!/bin/sh\nexit 0\n")
            for args in [["--unknown"], ["--bin-dir"], ["--binary", "missing"]]:
                result = subprocess.run(["bash", str(REPO / "install.sh")] + args, env=env, capture_output=True)
                self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
