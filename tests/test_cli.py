from __future__ import annotations

from contextlib import redirect_stderr
from io import StringIO
import unittest
from unittest.mock import patch

from photoscanner.cli import main
from photoscanner.scanner import ScannerError


class CliTests(unittest.TestCase):
    @patch("photoscanner.cli.list_devices", side_effect=ScannerError("SANE fehlt"))
    def test_scanner_error_returns_clean_exit_code(self, _devices) -> None:
        errors = StringIO()
        with redirect_stderr(errors):
            self.assertEqual(2, main(["devices"]))
        self.assertIn("SANE fehlt", errors.getvalue())


if __name__ == "__main__":
    unittest.main()
