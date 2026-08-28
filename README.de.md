# Photo Scanner

**Deutsch** | [English](README.md)

Photo Scanner ist eine native Linux-Anwendung zum Digitalisieren von
Papierfotos. Sie scannt über SANE, erkennt mehrere gleichzeitig aufgelegte
Fotos, begradigt sie und speichert jedes Foto mit PhotoPrism-kompatiblen
Aufnahmedaten. Alternativ lässt sich die gesamte Scanfläche unverändert als eine
Datei speichern.

Die Anwendung ist vollständig in Rust geschrieben. Die Oberfläche verwendet
GTK4 und Libadwaita, läuft nativ unter Wayland und passt deshalb gut zu CachyOS,
niri und Noctalia. Alle längeren Scanner- und OpenCV-Arbeiten laufen außerhalb
des GUI-Threads.

## Funktionen

- moderne, responsive GTK4-Oberfläche mit gespeicherten Einstellungen
- SANE-/AirScan-Scanner bei 300, 600 oder 1200 dpi
- automatische Fotoerkennung, Perspektivkorrektur und Begradigung
- erkannte Fotos vor dem Speichern prüfen, drehen oder abwählen
- mehrere PNG-, JPEG- oder TIFF-Scans importieren
- JPG, PNG und verlustfrei komprimiertes TIFF exportieren
- EXIF-Aufnahmedatum, Zeitzonenoffset, Softwarekennung und DPI schreiben
- vorhandene Dateien niemals überschreiben; Ausgabegruppen zurückrollen
- Scannerprozesse abbrechen und Scanfortschritt anzeigen
- Vorschauen zoomen und verschieben
- System-Dark-Mode und System-Akzentfarbe übernehmen
- eigenes CSS-Theme live nachladen und Noctalia-v5-Template bereitstellen
- CLI für Automatisierung und Stapelverarbeitung
- englische und deutsche Oberfläche anhand der Prozess-Locale

## Voraussetzungen auf CachyOS/Arch

```bash
sudo pacman -S --needed rust clang gtk4 libadwaita opencv sane sane-airscan exiv2 gettext
```

Ob SANE den Scanner erkennt, zeigt `scanimage -L`. Das Projekt ist auf den
aktuellen CachyOS-Stack mit OpenCV 5 ausgerichtet und verwendet dessen
pkg-config-Namen `opencv5`.

## Starten und installieren

```bash
make run
make install-user
```

Die Benutzerinstallation fügt Anwendung, Desktop-Metadaten, Icon und den
deutschen gettext-Katalog hinzu. Die GUI startet mit `photoscanner gui`.

## Bedienung

1. Scanner auswählen und Fotos mit ungefähr 1 cm Abstand auflegen.
2. Aufnahmedatum, Auflösung, Ausgabeformat und Ordner einstellen.
3. Automatische Trennung oder die gesamte Scanfläche auswählen.
4. Scannen, erkannte Fotos prüfen, drehen oder abwählen und speichern.

Die Oberfläche bleibt während der Arbeit reaktionsfähig. `Esc` bricht ab,
`F9` startet einen Scan, `Strg+O` importiert mehrere Dateien und `Strg+L` wählt
den Ausgabeordner. Ein neuer Scan verwirft eine offene Prüfung. **Vor dem
Speichern prüfen** lässt sich für den direkten Export abschalten.

## Themes und Noctalia

GTK4/Libadwaita übernimmt Hell-/Dunkelmodus, Kontrastpräferenz und Akzentfarbe.
Zusätzlich lädt und überwacht Photo Scanner:

```text
~/.config/photoscanner/theme.css
```

Ein Beispiel liegt in `docs/theme.css.example`. Für Noctalia v5:

```bash
mkdir -p ~/.config/noctalia/templates
cp docs/noctalia/photoscanner.css ~/.config/noctalia/templates/photoscanner.css
cp docs/noctalia/photoscanner.toml ~/.config/noctalia/photoscanner.toml
noctalia theme --list-templates
```

Anschließend unter **Media & UI → Theme** das Theme erneut anwenden. Bei
Änderungen am erzeugten CSS ist kein Neustart nötig.

## Kommandozeile

```bash
photoscanner devices
photoscanner scan --dpi 600 --date 01.09.1995 --output ~/Bilder/Archiv
photoscanner scan-full --dpi 600 --format tif --output ~/Bilder/Archiv
photoscanner split scan-1.png scan-2.png --output ~/Bilder/Archiv --threshold 10
photoscanner --help
```

Die Stapelverarbeitung läuft nach einem Fehler in einer einzelnen Datei weiter
und endet dann mit einem Fehlerstatus. Ohne `--output` verwenden CLI und GUI
gemeinsam `~/Bilder/PhotoScanner` (oder `output/PhotoScanner`, wenn kein
Bilderordner ermittelt werden kann). Die CLI-Hilfe ist Englisch;
Laufzeitmeldungen folgen der Locale.

## Qualitätssicherung

```bash
sudo pacman -S --needed desktop-file-utils appstream gettext cargo-audit
make check
make audit
```

`make check` prüft Formatierung, Clippy, Tests, CLI-Hilfe, deutschen Katalog,
Desktop- und AppStream-Metadaten. Die Tests decken unter anderem Erkennung,
Begradigung, kollisionsfreie Veröffentlichung und Rollback, Prozessabbruch,
Ressourcenlimits, Prüfexport und Metadaten ab.

## Lizenz

Photo Scanner steht unter der MIT-Lizenz. Details enthält `LICENSE`.
