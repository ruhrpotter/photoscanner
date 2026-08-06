from __future__ import annotations

from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from photoscanner.scanner import ScannerError, list_devices, scan_to_file


class FakeResult:
    def __init__(self, returncode: int = 0, stdout: str = "", stderr: bytes | str = b"") -> None:
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


class ScannerTests(unittest.TestCase):
    @patch("photoscanner.scanner.shutil.which", return_value="/usr/bin/scanimage")
    @patch("photoscanner.scanner.subprocess.run")
    def test_lists_devices(self, run, _which) -> None:
        run.return_value = FakeResult(
            stdout="airscan:e0:Scanner\tEpson\tET-4850\tflatbed scanner\n"
        )
        devices = list_devices()
        self.assertEqual(1, len(devices))
        self.assertEqual("airscan:e0:Scanner", devices[0].name)
        self.assertIn("Epson ET-4850", devices[0].label)

    @patch("photoscanner.scanner.shutil.which", return_value=None)
    def test_missing_scanimage_has_install_hint(self, _which) -> None:
        with self.assertRaisesRegex(ScannerError, "pacman -S sane"):
            list_devices()

    @patch("photoscanner.scanner.shutil.which", return_value="/usr/bin/scanimage")
    @patch("photoscanner.scanner.subprocess.run")
    def test_scan_is_written_atomically(self, run, _which) -> None:
        def fake_run(_command, **kwargs):
            kwargs["stdout"].write(b"PNG DATA")
            return FakeResult(stderr=b"")

        run.side_effect = fake_run
        with TemporaryDirectory() as temporary:
            destination = Path(temporary) / "scan.png"
            result = scan_to_file(destination, dpi=600)
            self.assertEqual(destination, result)
            self.assertEqual(b"PNG DATA", destination.read_bytes())
            self.assertFalse(destination.with_suffix(".png.part").exists())


if __name__ == "__main__":
    unittest.main()
