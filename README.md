# Photo Scanner

[Deutsch](README.de.md) | **English**

Photo Scanner is a native Linux application for digitizing paper photos. It
scans through SANE, detects several photos placed on the scanner bed, corrects
their perspective, and saves every photo with PhotoPrism-compatible capture
metadata. It can also save the complete scan area unchanged.

The application is written in Rust. Its GTK4 and Libadwaita interface runs
natively on Wayland, while scanner and OpenCV work stays off the GUI thread.

## Features

- responsive GTK4 interface with persistent settings
- SANE and AirScan discovery at 300, 600, or 1200 dpi
- automatic photo detection, perspective correction, and deskewing
- review detected photos before saving; rotate or exclude individual photos
- batch import PNG, JPEG, and TIFF scans
- export JPG, PNG, or losslessly compressed TIFF
- write EXIF capture date, time-zone offset, software identity, and DPI
- never overwrite existing files; roll back incomplete output groups
- cancel scanner processes and report live scan progress
- zoom and pan previews
- system color-scheme and accent-color integration
- live-reloaded custom CSS and a Noctalia v5 template
- CLI automation and batch processing
- English and German interface selected from the process locale

## Requirements on CachyOS/Arch

```bash
sudo pacman -S --needed rust clang gtk4 libadwaita opencv sane sane-airscan exiv2 gettext
```

Check whether SANE sees the scanner with `scanimage -L`. The project targets
the current CachyOS OpenCV 5 stack and uses its `opencv5` pkg-config name.

## Run and install

```bash
make run
make install-user
```

The user installation adds the application, desktop metadata, icon, and German
gettext catalog. Start the installed GUI with `photoscanner gui`.

## Using the GUI

1. Select a scanner and place photos about 1 cm apart.
2. Choose the capture date, resolution, output format, and folder.
3. Select automatic separation or the complete scan area.
4. Scan, review the detected photos, rotate or deselect them, and save.

The interface stays responsive during work. `Escape` cancels an operation,
`F9` starts a scan, `Ctrl+O` imports one or more files, and `Ctrl+L` selects the
output folder. Starting a new scan discards an open review. Disable **Review
before saving** to use direct export.

## Themes and Noctalia

GTK4 and Libadwaita follow the system light/dark mode, contrast preference, and
accent color. Photo Scanner additionally loads and watches:

```text
~/.config/photoscanner/theme.css
```

An example is available at `docs/theme.css.example`. For Noctalia v5:

```bash
mkdir -p ~/.config/noctalia/templates
cp docs/noctalia/photoscanner.css ~/.config/noctalia/templates/photoscanner.css
cp docs/noctalia/photoscanner.toml ~/.config/noctalia/photoscanner.toml
noctalia theme --list-templates
```

Reapply the theme under **Media & UI → Theme**. No restart is needed when the
generated CSS changes.

## Command line

```bash
photoscanner devices
photoscanner scan --dpi 600 --date 01.09.1995 --output ~/Pictures/Archive
photoscanner scan-full --dpi 600 --format tif --output ~/Pictures/Archive
photoscanner split scan-1.png scan-2.png --output ~/Pictures/Archive --threshold 10
photoscanner --help
```

Batch splitting continues after an individual file fails and returns a non-zero
status if any file failed. Without `--output`, the CLI and GUI share
`~/Pictures/PhotoScanner` (or `output/PhotoScanner` when no pictures directory
can be determined). CLI help is English; runtime messages follow the locale.

## Quality checks

```bash
sudo pacman -S --needed desktop-file-utils appstream gettext cargo-audit
make check
make audit
```

`make check` validates formatting, Clippy, tests, CLI help, the German catalog,
desktop metadata, and AppStream metadata. Tests cover detection, deskewing,
collision-free publishing and rollback, cancellation, resource limits, review
export, and metadata.

## License

Photo Scanner is licensed under the MIT License. See `LICENSE`.
