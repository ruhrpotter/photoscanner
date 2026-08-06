from __future__ import annotations

from contextlib import redirect_stderr
from datetime import date
from io import StringIO
import unittest
from unittest.mock import patch

from photoscanner.cli import build_parser, main
from photoscanner.scanner import ScannerError


class CliTests(unittest.TestCase):
    def test_parses_german_capture_date(self) -> None:
        args = build_parser().parse_args(["scan", "--date", "01.09.1995"])
        self.assertEqual(date(1995, 9, 1), args.date)

    @patch("photoscanner.cli.list_devices", side_effect=ScannerError("SANE fehlt"))
    def test_scanner_error_returns_clean_exit_code(self, _devices) -> None:
        errors = StringIO()
        with redirect_stderr(errors):
            self.assertEqual(2, main(["devices"]))
        self.assertIn("SANE fehlt", errors.getvalue())


if __name__ == "__main__":
    unittest.main()
