"""Anbindung an Scanner, die von SANE/scanimage unterstützt werden."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import shutil
import subprocess


class ScannerError(RuntimeError):
    """Ein verständlich darstellbarer Fehler der Scanner-Anbindung."""


@dataclass(frozen=True, slots=True)
class ScannerDevice:
    name: str
    vendor: str = ""
    model: str = ""
    kind: str = ""

    @property
    def label(self) -> str:
        description = " ".join(part for part in (self.vendor, self.model) if part)
        return f"{description or self.name} ({self.name})"


def scanimage_available() -> bool:
    return shutil.which("scanimage") is not None


def list_devices(timeout: int = 15) -> list[ScannerDevice]:
    """Gibt alle von scanimage erkannten Geräte zurück."""
    if not scanimage_available():
        raise ScannerError(
            "SANE ist nicht installiert: Das Programm 'scanimage' wurde nicht gefunden. "
            "Unter CachyOS/Arch kann es mit 'sudo pacman -S sane sane-airscan' "
            "installiert werden."
        )

    try:
        result = subprocess.run(
            ["scanimage", "-f", "%d\t%v\t%m\t%t%n"],
            check=False,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as exc:
        raise ScannerError("Die Scannersuche hat zu lange gedauert.") from exc
    except OSError as exc:
        raise ScannerError(f"scanimage konnte nicht gestartet werden: {exc}") from exc

    if result.returncode != 0:
        detail = result.stderr.strip() or "unbekannter SANE-Fehler"
        raise ScannerError(f"Scanner konnten nicht abgefragt werden: {detail}")

    devices: list[ScannerDevice] = []
    for line in result.stdout.splitlines():
        if not line.strip():
            continue
        fields = line.split("\t")
        fields.extend([""] * (4 - len(fields)))
        device = ScannerDevice(*(field.strip() for field in fields[:4]))
        # Das SANE-v4l-Backend meldet integrierte Webcams als Scanner. Für das
        # Digitalisieren von Papierfotos sind diese Geräte ungeeignet und
        # würden nur die Auswahl in der TUI verfälschen.
        if not device.name.startswith("v4l:"):
            devices.append(device)
    return devices


def scan_to_file(
    destination: Path,
    *,
    device: str | None = None,
    dpi: int = 600,
    mode: str = "Color",
    timeout: int = 600,
) -> Path:
    """Scannt die gesamte Glasfläche verlustfrei als PNG."""
    if not scanimage_available():
        raise ScannerError(
            "Scannen ist nicht möglich, weil 'scanimage' fehlt. "
            "Installiere unter CachyOS/Arch: sudo pacman -S sane sane-airscan"
        )
    if not 75 <= dpi <= 2400:
        raise ScannerError("Die Auflösung muss zwischen 75 und 2400 dpi liegen.")

    destination = destination.expanduser().resolve()
    destination.parent.mkdir(parents=True, exist_ok=True)
    command = ["scanimage"]
    if device:
        command.extend(["--device-name", device])
    command.extend(
        [
            "--mode",
            mode,
            "--resolution",
            str(dpi),
            "--format",
            "png",
        ]
    )

    temporary = destination.with_suffix(destination.suffix + ".part")
    try:
        with temporary.open("wb") as output:
            result = subprocess.run(
                command,
                check=False,
                stdout=output,
                stderr=subprocess.PIPE,
                timeout=timeout,
            )
        if result.returncode != 0:
            temporary.unlink(missing_ok=True)
            detail = result.stderr.decode(errors="replace").strip()
            raise ScannerError(f"Der Scan ist fehlgeschlagen: {detail or 'unbekannter Fehler'}")
        if not temporary.exists() or temporary.stat().st_size == 0:
            temporary.unlink(missing_ok=True)
            raise ScannerError("Der Scanner hat keine Bilddaten geliefert.")
        temporary.replace(destination)
    except subprocess.TimeoutExpired as exc:
        temporary.unlink(missing_ok=True)
        raise ScannerError("Der Scan hat das Zeitlimit überschritten.") from exc
    except OSError as exc:
        temporary.unlink(missing_ok=True)
        raise ScannerError(f"Die Scandatei konnte nicht geschrieben werden: {exc}") from exc
    return destination
