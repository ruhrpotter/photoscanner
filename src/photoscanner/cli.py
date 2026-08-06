"""Kommandozeilen-Einstiegspunkt für TUI und Automatisierung."""

from __future__ import annotations

import argparse
from pathlib import Path
from tempfile import TemporaryDirectory

from rich.console import Console

from . import __version__
from .scanner import ScannerError, list_devices, scan_to_file
from .splitter import SplitConfig, SplitError, split_scan
from .tui import PhotoScannerTui


def _add_split_options(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("-o", "--output", type=Path, default=Path("output"), help="Ausgabeordner")
    parser.add_argument("--format", choices=["jpg", "png", "tif"], default="jpg")
    parser.add_argument("--quality", type=int, default=95, help="JPEG-Qualität (1-100)")
    parser.add_argument(
        "--min-area",
        type=float,
        default=2.0,
        metavar="PROZENT",
        help="Mindestfläche eines Fotos relativ zum Scan (Standard: 2.0)",
    )
    parser.add_argument(
        "--threshold",
        type=float,
        default=None,
        metavar="WERT",
        help="Manueller Hintergrund-Schwellwert (1-255; Standard: automatisch)",
    )
    parser.add_argument(
        "--padding",
        type=float,
        default=1.2,
        metavar="PROZENT",
        help="Zusätzlicher Rand je Seite (Standard: 1.2)",
    )
    parser.add_argument("--prefix", help="Dateinamen-Präfix")
    parser.add_argument("--no-preview", action="store_true", help="Kein markiertes Vorschaubild")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="photoscanner",
        description="Mehrere Papierfotos scannen und automatisch einzeln speichern.",
    )
    parser.add_argument("--version", action="version", version=f"%(prog)s {__version__}")
    subparsers = parser.add_subparsers(dest="command")

    subparsers.add_parser("devices", help="Von SANE erkannte Scanner anzeigen")

    split_parser = subparsers.add_parser("split", help="Vorhandene Scandatei aufteilen")
    split_parser.add_argument("source", type=Path, help="Scanbild (PNG, JPG oder TIFF)")
    _add_split_options(split_parser)

    scan_parser = subparsers.add_parser("scan", help="Scannen und Ergebnis aufteilen")
    scan_parser.add_argument("--device", help="SANE-Gerätename; sonst erstes erkanntes Gerät")
    scan_parser.add_argument("--dpi", type=int, default=600, help="Scanauflösung (Standard: 600)")
    _add_split_options(scan_parser)
    return parser


def _config_from_args(args: argparse.Namespace, *, scanned: bool = False) -> SplitConfig:
    return SplitConfig(
        min_area_percent=args.min_area,
        threshold=args.threshold,
        padding_percent=args.padding,
        output_format=args.format,
        jpeg_quality=args.quality,
        dpi=args.dpi if scanned else None,
    )


def _print_result(console: Console, result: object) -> None:
    files = result.files
    console.print(f"[bold green]{len(files)} Foto(s) gespeichert:[/bold green]")
    for path in files:
        console.print(str(path))
    if result.preview:
        console.print(f"Vorschau: {result.preview}")


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.command is None:
        return PhotoScannerTui().run()

    console = Console()
    try:
        if args.command == "devices":
            devices = list_devices()
            if not devices:
                console.print("[yellow]Kein Scanner erkannt.[/yellow]")
                return 1
            for device in devices:
                console.print(f"{device.name}\t{device.vendor}\t{device.model}\t{device.kind}")
            return 0

        if args.command == "split":
            result = split_scan(
                args.source,
                args.output,
                _config_from_args(args),
                prefix=args.prefix,
                save_preview=not args.no_preview,
            )
            _print_result(console, result)
            return 0

        devices = list_devices()
        if args.device:
            device_name = args.device
        elif devices:
            device_name = devices[0].name
            console.print(f"Verwende Scanner: {devices[0].label}")
        else:
            raise ScannerError("SANE hat keinen Scanner gefunden.")
        with TemporaryDirectory(prefix="photoscanner-") as temporary:
            scan_path = Path(temporary) / "scan.png"
            console.print(f"Scanne mit {args.dpi} dpi ...")
            scan_to_file(scan_path, device=device_name, dpi=args.dpi)
            result = split_scan(
                scan_path,
                args.output,
                _config_from_args(args, scanned=True),
                prefix=args.prefix,
                save_preview=not args.no_preview,
            )
        _print_result(console, result)
        return 0
    except (ScannerError, SplitError, ValueError) as exc:
        Console(stderr=True).print(f"[bold red]Fehler:[/bold red] {exc}")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
