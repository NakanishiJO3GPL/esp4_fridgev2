"""Validate the build-local Hosted overlay without modifying the dependency."""
import pathlib
import subprocess
import tempfile
import unittest

PROJECT = pathlib.Path(__file__).resolve().parents[1]
UPSTREAM = PROJECT.parent / "esp_hosted/host/mcu/eh_host_mcu_transport/src/eh_host_bus_sdio.c"
OVERLAY = PROJECT / "components/fridge_camera/hosted_backpressure.cmake"


class HostedBackpressureTests(unittest.TestCase):
    def apply(self, source):
        with tempfile.TemporaryDirectory(dir=PROJECT / "target") as directory:
            directory = pathlib.Path(directory)
            input_path = directory / "input.c"
            output_path = directory / "output.c"
            script_path = directory / "apply.cmake"
            input_path.write_text(source)
            script_path.write_text(
                f'include("{OVERLAY}")\n'
                f'fridge_patch_hosted_sdio("{input_path}" "{output_path}")\n'
            )
            result = subprocess.run(
                ["cmake", "-P", str(script_path)], capture_output=True, text=True,
                timeout=10,
            )
            return result, output_path.read_text() if output_path.exists() else None

    def test_only_credit_timeout_discard_is_replaced(self):
        original = UPSTREAM.read_text()
        result, patched = self.apply(original)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("break;                                /* drop this aggregate */", patched)
        self.assertIn("aggregate retained", patched)
        self.assertIn("if (!s_running)\n                return;", patched)
        self.assertIn("eh_host_port_task_delay_ms(1);\n            continue;", patched)
        # Packet assembly, normal SDIO writes and post-send credit accounting
        # remain byte-identical. Only the unavailable-credit branch changes.
        before = original.index("\t\t\tif (has_ctrl || sdio_aggr_credit_timeouts")
        after = original.index("\n\t\t}\n\t\tsdio_aggr_credit_timeouts = 0;", before)
        self.assertTrue(patched.startswith(original[:before]))
        self.assertTrue(patched.endswith(original[after:]))
        self.assertEqual(UPSTREAM.read_text(), original)

    def test_changed_upstream_fails_closed(self):
        original = UPSTREAM.read_text()
        changed = original.replace("/* drop this aggregate */", "/* changed implementation */")
        result, patched = self.apply(changed)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("credit loop changed", result.stderr)
        self.assertIsNone(patched)

    def test_ambiguous_match_fails_closed(self):
        original = UPSTREAM.read_text()
        result, patched = self.apply(original + original)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("match is ambiguous", result.stderr)
        self.assertIsNone(patched)


if __name__ == "__main__":
    unittest.main()
