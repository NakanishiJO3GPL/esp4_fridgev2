"""Test SDK provenance guards with a modeled Git executable, without network."""
import pathlib
import re
import subprocess
import tempfile
import unittest

PROJECT = pathlib.Path(__file__).resolve().parents[1]
CHECK = PROJECT / "idf/official_sdk.cmake"
COMMIT = re.search(r'set\(expected_commit "([0-9a-f]{40})"\)', CHECK.read_text())[1]
OFFICIAL = "https://github.com/espressif/esp-idf.git"


class OfficialSdkTests(unittest.TestCase):
    def check(self, origin=OFFICIAL, commit=COMMIT, dirty=False, override=False,
              git_failure=False):
        with tempfile.TemporaryDirectory(dir=PROJECT / "target") as directory:
            root = pathlib.Path(directory)
            sdk = root / "sdk"
            camera = sdk / "components/esp_driver_cam"
            camera.mkdir(parents=True)
            if override:
                camera = root / "override"
                camera.mkdir()
            git = root / "git"
            git.write_text(
                "#!/usr/bin/env python3\n"
                "import sys\n"
                "args = sys.argv[1:]\n"
                "if args == ['--version']:\n"
                "    print('git version 2.45.0')\n"
                "elif 'remote' in args:\n"
                f"    print({origin!r})\n"
                "elif 'rev-parse' in args:\n"
                f"    print({commit!r})\n"
                "elif 'diff' in args:\n"
                f"    sys.exit({2 if git_failure else int(dirty)})\n"
                "else:\n"
                "    sys.exit(2)\n"
            )
            git.chmod(0o755)
            script = root / "check.cmake"
            script.write_text(
                f'set(GIT_EXECUTABLE "{git}")\n'
                f'include("{CHECK}")\n'
                f'fridge_check_official_sdk("{sdk}" "{camera}")\n'
            )
            return subprocess.run(["cmake", "-P", str(script)],
                                  capture_output=True, text=True, timeout=10)

    def test_official_clean_sdk_passes(self):
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_foreign_origin_rejected(self):
        result = self.check(origin="https://github.com/other/esp-idf.git")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Git checkout from", result.stderr)

    def test_different_revision_rejected(self):
        result = self.check(commit="0" * 40)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("official 6.1 commit", result.stderr)

    def test_modified_sdk_rejected(self):
        result = self.check(dirty=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Modified ESP-IDF SDK rejected", result.stderr)

    def test_driver_override_rejected(self):
        result = self.check(override=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("CSI driver override rejected", result.stderr)

    def test_git_check_failure_rejected(self):
        result = self.check(git_failure=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Modified ESP-IDF SDK rejected", result.stderr)

    def test_cargo_and_guard_commit_pins_match(self):
        config = (PROJECT / ".cargo/config.toml").read_text()
        self.assertIn(f'value = "commit:{COMMIT}", force = true', config)
        self.assertIn(f'value = "{OFFICIAL}", force = true', config)


if __name__ == "__main__":
    unittest.main()
