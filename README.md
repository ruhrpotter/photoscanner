# Photo Scanner

Photo Scanner ist eine lokale Terminal-Anwendung für Flachbettscanner. Sie scannt
mehrere gleichzeitig aufgelegte Papierfotos, erkennt deren Grenzen, begradigt
leichte Drehungen und speichert jedes Foto als eigene Bilddatei. Zu jedem Lauf
entsteht zusätzlich eine markierte Vorschau zur schnellen Kontrolle.

## Voraussetzungen

- Python 3.11 oder neuer
- ein von SANE unterstützter Scanner
- CachyOS/Arch: `sudo pacman -S sane sane-airscan`

`sane-airscan` wird für viele moderne Netzwerk- und USB-Geräte mit eSCL/AirScan
benötigt. Ob der Scanner sichtbar ist, zeigt anschließend `scanimage -L`.

## Installation

```bash
make install
```

Dadurch wird die lokale Umgebung `.venv` angelegt und die Anwendung inklusive
ihrer Bildverarbeitungsabhängigkeiten installiert.

## Interaktive Verwendung

```bash
make run
```

Im Menü kann direkt gescannt, eine vorhandene Scandatei importiert oder der
Scanner geprüft werden. Standardmäßig werden die Bilder unter `output/`
gespeichert. Vor jedem Scan fragt die TUI nach dem Aufnahmedatum. Enter übernimmt
das heutige Datum; historische Fotos können zum Beispiel mit `01.09.1995`
versehen werden.

Der einmal ausgewählte Scanner bleibt für die laufende TUI-Sitzung gespeichert.
Weitere Scans starten deshalb ohne erneute, langsame SANE-Gerätesuche. Menüpunkt
`3` aktualisiert die Geräteliste weiterhin bewusst.

Für gute Ergebnisse:

- Scannerauflösung 600 dpi für normale Papierfotos
- Fotos nicht überlappen lassen
- rund 1 cm Abstand zwischen den Fotos und zum Rand lassen
- Scanfläche und Fotos möglichst staubfrei halten
- Scannerdeckel schließen

## Kommandozeile

Scanner anzeigen:

```bash
.venv/bin/photoscanner devices
```

Scannen und direkt trennen:

```bash
.venv/bin/photoscanner scan --dpi 600 --output ~/Bilder/Archiv
```

Mit einem manuell gewählten Aufnahmedatum:

```bash
.venv/bin/photoscanner scan --dpi 600 --date 01.09.1995 --output ~/Bilder/Archiv
```

Eine vorhandene Datei trennen:

```bash
.venv/bin/photoscanner split scan.png --output ~/Bilder/Archiv --prefix urlaub_1998
```

Wenn die automatische Erkennung zu viel oder zu wenig markiert, kann der
Schwellwert manuell gesetzt werden. Ein kleinerer Wert erkennt schwächere
Unterschiede zum Scanbett, ein größerer Wert ignoriert mehr Hintergrund:

```bash
.venv/bin/photoscanner split scan.png --threshold 10
```

Alle Optionen zeigt `.venv/bin/photoscanner split --help`.

## Ausgabe

Die Standardnamen sehen so aus:

```text
scan_20260806_143012_01.jpg
scan_20260806_143012_02.jpg
scan_20260806_143012_vorschau.jpg
```

Vorhandene Dateien werden nie überschrieben. JPG (Qualität 95), PNG und
verlustfrei komprimiertes TIFF werden unterstützt. Beim direkten Scannen wird
die eingestellte dpi-Zahl in die Einzelfotos übernommen.

Alle erzeugten Fotos und die Vorschau erhalten das ausgewählte lokale Datum als
EXIF `DateTimeOriginal`, `DateTimeDigitized` und `DateTime`. Der Dateiname und
der Datei-Zeitstempel verwenden denselben Zeitpunkt. Ohne manuelle Eingabe gilt
das heutige Datum. Dadurch erkennt PhotoPrism die Bilder zuverlässig mit dem
Scan-Tag oder dem ausgewählten historischen Aufnahmedatum.

## Tests

```bash
make check
```

Die Tests erzeugen einen synthetischen Scan mit drei gedrehten Fotos und prüfen
Erkennung, Begradigung, DPI-Metadaten, Dateiexport und Scannerfehler ohne echte
Hardware.
