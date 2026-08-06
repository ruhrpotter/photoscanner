"""Interaktive Terminal-Oberfläche."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from tempfile import TemporaryDirectory

from rich.console import Console
from rich.panel import Panel
from rich.prompt import Confirm, IntPrompt, Prompt
from rich.table import Table

from .scanner import ScannerDevice, ScannerError, list_devices, scan_to_file
from .splitter import SplitConfig, SplitError, SplitResult, split_scan


@dataclass(slots=True)
class TuiSettings:
    dpi: int = 600
    output_directory: Path = Path("output")
    output_format: str = "jpg"
    jpeg_quality: int = 95
    min_area_percent: float = 2.0
    threshold: float | None = None
    padding_percent: float = 1.2

    def split_config(self, *, scanned: bool = False) -> SplitConfig:
        return SplitConfig(
            min_area_percent=self.min_area_percent,
            threshold=self.threshold,
            padding_percent=self.padding_percent,
            output_format=self.output_format,
            jpeg_quality=self.jpeg_quality,
            dpi=self.dpi if scanned else None,
        )


class PhotoScannerTui:
    def __init__(self, console: Console | None = None) -> None:
        self.console = console or Console()
        self.settings = TuiSettings()

    def run(self) -> int:
        self.console.print(
            Panel.fit(
                "[bold cyan]Photo Scanner[/bold cyan]\n"
                "Mehrere Papierfotos scannen, erkennen und einzeln speichern",
                border_style="cyan",
            )
        )
        while True:
            self._show_menu()
            choice = Prompt.ask("Auswahl", choices=["1", "2", "3", "4", "q"], default="1")
            try:
                if choice == "1":
                    self._scan_and_split()
                elif choice == "2":
                    self._split_file()
                elif choice == "3":
                    self._show_devices()
                elif choice == "4":
                    self._edit_settings()
                else:
                    self.console.print("Bis bald.")
                    return 0
            except (ScannerError, SplitError) as exc:
                self.console.print(f"[bold red]Fehler:[/bold red] {exc}")
            except KeyboardInterrupt:
                self.console.print("\n[yellow]Vorgang abgebrochen.[/yellow]")

    def _show_menu(self) -> None:
        self.console.print()
        self.console.print("[bold]1[/bold]  Scanner einlesen und Fotos trennen")
        self.console.print("[bold]2[/bold]  Vorhandene Scandatei trennen")
        self.console.print("[bold]3[/bold]  Erkannte Scanner anzeigen")
        self.console.print("[bold]4[/bold]  Einstellungen")
        self.console.print("[bold]q[/bold]  Beenden")

    def _choose_device(self, devices: list[ScannerDevice]) -> ScannerDevice:
        if len(devices) == 1:
            self.console.print(f"Scanner: [cyan]{devices[0].label}[/cyan]")
            return devices[0]
        table = Table("Nr.", "Scanner", "Typ")
        for index, device in enumerate(devices, start=1):
            table.add_row(str(index), device.label, device.kind)
        self.console.print(table)
        selected = IntPrompt.ask("Scanner", default=1)
        if not 1 <= selected <= len(devices):
            raise ScannerError("Ungültige Scanner-Auswahl.")
        return devices[selected - 1]

    def _scan_and_split(self) -> None:
        devices = list_devices()
        if not devices:
            raise ScannerError(
                "SANE hat keinen Scanner gefunden. Prüfe USB/Netzwerk und teste 'scanimage -L'."
            )
        device = self._choose_device(devices)
        self.console.print(
            "\nLege die Fotos mit [bold]mindestens 1 cm Abstand[/bold] auf das Scanbett."
        )
        if not Confirm.ask("Scan starten?", default=True):
            return
        with TemporaryDirectory(prefix="photoscanner-") as temporary:
            scan_path = Path(temporary) / "scan.png"
            with self.console.status(f"Scanne mit {self.settings.dpi} dpi ..."):
                scan_to_file(
                    scan_path,
                    device=device.name,
                    dpi=self.settings.dpi,
                )
            with self.console.status("Erkenne und begradige Fotos ..."):
                result = split_scan(
                    scan_path,
                    self.settings.output_directory,
                    self.settings.split_config(scanned=True),
                )
        self._show_result(result)

    def _split_file(self) -> None:
        raw_path = Prompt.ask("Pfad zur Scandatei").strip()
        if not raw_path:
            raise SplitError("Es wurde keine Datei angegeben.")
        with self.console.status("Erkenne und begradige Fotos ..."):
            result = split_scan(
                Path(raw_path),
                self.settings.output_directory,
                self.settings.split_config(),
            )
        self._show_result(result)

    def _show_result(self, result: SplitResult) -> None:
        self.console.print(
            f"\n[bold green]Fertig: {len(result.files)} Foto(s) gespeichert.[/bold green]"
        )
        for path in result.files:
            self.console.print(f"  {path}")
        if result.preview:
            self.console.print(f"Vorschau: [cyan]{result.preview}[/cyan]")
        self.console.print(f"Erkennungsschwellwert: {result.threshold_used:.1f}")

    def _show_devices(self) -> None:
        devices = list_devices()
        if not devices:
            self.console.print("[yellow]Kein Scanner erkannt.[/yellow]")
            return
        table = Table("Gerät", "Hersteller", "Modell", "Typ")
        for device in devices:
            table.add_row(device.name, device.vendor, device.model, device.kind)
        self.console.print(table)

    def _edit_settings(self) -> None:
        self.settings.dpi = IntPrompt.ask("Scanauflösung (dpi)", default=self.settings.dpi)
        output = Prompt.ask("Ausgabeordner", default=str(self.settings.output_directory))
        self.settings.output_directory = Path(output).expanduser()
        self.settings.output_format = Prompt.ask(
            "Bildformat", choices=["jpg", "png", "tif"], default=self.settings.output_format
        )
        if self.settings.output_format == "jpg":
            self.settings.jpeg_quality = IntPrompt.ask(
                "JPEG-Qualität", default=self.settings.jpeg_quality
            )
        threshold = Prompt.ask(
            "Erkennungsschwellwert (auto oder 1-255)",
            default="auto" if self.settings.threshold is None else str(self.settings.threshold),
        )
        self.settings.threshold = None if threshold.lower() == "auto" else float(threshold)
        self.console.print("[green]Einstellungen übernommen.[/green]")
