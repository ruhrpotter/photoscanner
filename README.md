# Photo Scanner

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

- moderne, responsive GTK4-Oberfläche
- SANE-/AirScan-Scanner auswählen und ohne erneute Gerätesuche weiterverwenden
- 300, 600 oder 1200 dpi
- automatische Fotoerkennung, Perspektivkorrektur und Begradigung
- vollständige Scanfläche ohne Erkennung speichern
- vorhandene PNG-, JPEG- oder TIFF-Scans importieren
- JPG, PNG und verlustfrei komprimiertes TIFF exportieren
- EXIF-Aufnahmedatum, Zeitzonenoffset, Softwarekennung und DPI schreiben
- vorhandene Dateien niemals überschreiben und Ausgabegruppen bei Fehlern zurückrollen
- markierte Kontrollvorschau erzeugen
- laufende Scannerprozesse per `Esc` abbrechen und zuverlässig aufräumen
- System-Dark-Mode und System-Akzentfarbe übernehmen
- eigenes CSS-Theme unter `~/.config/photoscanner/theme.css` live nachladen
- Noctalia-v5-Paletten über ein mitgeliefertes App-Theming-Template übernehmen
- CLI für Automatisierung und Stapelverarbeitung erhalten

## Voraussetzungen auf CachyOS/Arch

```bash
sudo pacman -S --needed rust clang gtk4 libadwaita opencv sane sane-airscan exiv2
```

Ob SANE den Scanner erkennt:

```bash
scanimage -L
```

Das Projekt ist auf den aktuellen CachyOS-Stack mit OpenCV 5 ausgerichtet und
verwendet deshalb dessen pkg-config-Namen `opencv5`.

## Starten

Direkt aus dem Projekt:

```bash
make run
```

Oder zunächst als Release-Build installieren:

```bash
make install-user
```

Danach steht **Photo Scanner** im App-Launcher zur Verfügung. Im Terminal lässt
sich die GUI mit `photoscanner gui` starten.

## Bedienung

1. Scanner auswählen und Fotos mit ungefähr 1 cm Abstand auflegen.
2. Aufnahmedatum, Auflösung und Ausgabeformat einstellen.
3. `Fotos automatisch trennen` oder `Gesamte Scanfläche speichern` auswählen.
4. Scan starten und die markierte Vorschau kontrollieren.

Die Oberfläche bleibt während Scan und Bildverarbeitung reaktionsfähig. Ein
laufender Vorgang lässt sich mit `Esc` abbrechen. Mit `F9` wird ein Scan
gestartet, `Strg+O` importiert eine Datei und `Strg+L` wählt den Ausgabeordner.
Bei einem schmalen niri-Tile wird die Einstellungsseite automatisch als
einblendbare Seitenleiste dargestellt.

## Themes und Noctalia

GTK4/Libadwaita übernimmt standardmäßig Hell-/Dunkelmodus, Kontrastpräferenz
und Akzentfarbe des Systems. Zusätzlich lädt Photo Scanner diese Datei:

```text
~/.config/photoscanner/theme.css
```

Änderungen werden nach dem Schreiben der Datei automatisch übernommen. Fehler
beim Lesen oder Parsen zeigt die Anwendung im Statusbereich an. Ein manuelles
Beispiel liegt in `docs/theme.css.example`.

Für Noctalia v5:

```bash
mkdir -p ~/.config/noctalia/templates
cp docs/noctalia/photoscanner.css ~/.config/noctalia/templates/photoscanner.css
cp docs/noctalia/photoscanner.toml ~/.config/noctalia/photoscanner.toml
noctalia theme --list-templates
```

Anschließend in Noctalia unter **Media & UI → Theme** das Theme erneut anwenden.
Noctalia rendert dann die aktive Material-Palette nach
`~/.config/photoscanner/theme.css`. Ein Neustart der Anwendung ist nicht nötig.

## Kommandozeile

Scanner anzeigen:

```bash
photoscanner devices
```

Scannen und automatisch trennen:

```bash
photoscanner scan --dpi 600 --date 01.09.1995 --output ~/Bilder/Archiv
```

Gesamte Scanfläche speichern:

```bash
photoscanner scan-full --dpi 600 --format tif --output ~/Bilder/Archiv
```

Vorhandene Datei trennen:

```bash
photoscanner split scan.png --output ~/Bilder/Archiv --threshold 10
```

Alle Befehle und Optionen:

```bash
photoscanner --help
```

## Qualitätssicherung

Für alle lokalen Prüfwerkzeuge:

```bash
sudo pacman -S --needed desktop-file-utils appstream cargo-audit
```

```bash
make check
make audit
```

Die Rust-Tests erzeugen synthetische Scans und prüfen unter anderem Erkennung,
Begradigung, eng nebeneinanderliegende Fotos, Scanner-Ränder, kollisionsfreie
Dateinamen, parallele Exporte, Prozessabbruch, Ressourcenlimits sowie EXIF- und
DPI-Metadaten für JPG, PNG und TIFF. Dieselben Prüfungen laufen bei Pushes und
Pull Requests in GitHub Actions.

## Lizenz

Photo Scanner steht unter der MIT-Lizenz. Details enthält `LICENSE`.
