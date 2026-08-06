from __future__ import annotations

from io import StringIO
import unittest
from unittest.mock import patch

from rich.console import Console

from photoscanner.scanner import ScannerDevice
from photoscanner.tui import PhotoScannerTui


class TuiTests(unittest.TestCase):
    @patch("photoscanner.tui.list_devices")
    def test_remembers_selected_scanner_during_session(self, list_devices) -> None:
        brother = ScannerDevice(
            "airscan:e0:Brother MFC-L2960DW",
            "eSCL",
            "Brother MFC-L2960DW",
            "ip=192.168.178.230",
        )
        list_devices.return_value = [brother]
        tui = PhotoScannerTui(Console(file=StringIO(), force_terminal=False))

        self.assertEqual(brother, tui._device_for_scan())
        self.assertEqual(brother, tui._device_for_scan())
        list_devices.assert_called_once_with()


if __name__ == "__main__":
    unittest.main()
