# Photo Scanner

[Deutsch](README.de.md) | **English**

[![CI](https://github.com/ruhrpotter/photoscanner/actions/workflows/ci.yml/badge.svg)](https://github.com/ruhrpotter/photoscanner/actions/workflows/ci.yml)
[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.92+](https://img.shields.io/badge/rust-1.92%2B-orange.svg?logo=rust)](Cargo.toml)

Photo Scanner is a native GTK4 application for Linux that turns flatbed batch
scans into individual, archive-ready images. Digitize several paper photos at
once: the app scans through SANE or AirScan, detects every print on the scanner
bed, corrects perspective, and writes the capture date to EXIF metadata. The
result is ready for a photo archive such as PhotoPrism.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/screenshots/review-dark.png">
  <img src="docs/screenshots/review-light.png" alt="Photo Scanner review view with three automatically detected and anonymized photos" width="1600">
</picture>

## Features

- Scan several photos in one pass at 300, 600, or 1200 dpi.
- Automatically detect, deskew, and correct the perspective of each print.
- Review results before export; rotate or exclude individual photos.
- Import batches of existing PNG, JPEG, or TIFF scans.
- Export JPG, PNG, or losslessly compressed TIFF without overwriting files.
- Write EXIF capture date, time-zone offset, software identity, and DPI.
- Keep the interface responsive, show live progress, and cancel scanner work.
- Save the complete scan area when automatic separation is not needed.
- Use the GTK4 interface on Wayland or automate workflows through the CLI.
- Follow the system color scheme and accent color, with optional custom CSS and
  a Noctalia v5 template.
- Use the English or German interface selected from the process locale.

## Compatibility and requirements

The current source targets a recent Linux desktop stack:

- Rust 1.92 or newer with Edition 2024 support
- GTK 4.22 or newer
- Libadwaita 1.9 or newer
- OpenCV 5
- SANE, plus `sane-airscan` for eSCL/AirScan network scanners
- Exiv2 and GNU gettext

Ubuntu LTS and Debian Stable do not currently ship all of these versions. The
package and build instructions below are therefore for Arch Linux and CachyOS.

Any flatbed scanner listed by `scanimage -L` should work, over USB or the
network. Photo Scanner is tested with a Brother MFC-L2960DW over eSCL/AirScan.

```bash
sudo pacman -S --needed rust clang gtk4 libadwaita opencv sane sane-airscan exiv2 gettext
```

The project uses CachyOS/Arch's `opencv5` pkg-config name.

## Run and install

Build and start the app from the repository:

```bash
make run
```

Install it for the current user, including the launcher, icon, metadata, and
German translation:

```bash
make install-user
```

The installed application starts from the desktop launcher or with
`photoscanner gui`.

## Using the GUI

1. Select a scanner and place the photos about 1 cm apart.
2. Choose the capture date, resolution, output format, and destination.
3. Select automatic separation or the complete scan area.
4. Scan, review the detected photos, rotate or deselect them, and save.

Starting another scan discards an open review. Disable **Review before saving**
when files should be exported directly.

### Keyboard shortcuts

| Action | Shortcut |
| --- | --- |
| Start a scan | <kbd>F9</kbd> |
| Cancel the current operation | <kbd>Esc</kbd> |
| Open one or more scan files | <kbd>Ctrl</kbd> + <kbd>O</kbd> |
| Choose the output folder | <kbd>Ctrl</kbd> + <kbd>L</kbd> |
| Refresh scanner discovery | <kbd>Ctrl</kbd> + <kbd>R</kbd> |
| Show or hide the settings sidebar | <kbd>F10</kbd> |
| Zoom in | <kbd>Ctrl</kbd> + <kbd>+</kbd> |
| Zoom out | <kbd>Ctrl</kbd> + <kbd>-</kbd> |
| Fit the preview | <kbd>Ctrl</kbd> + <kbd>0</kbd> |
| Quit | <kbd>Ctrl</kbd> + <kbd>Q</kbd> |

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

## Troubleshooting

### The scanner is not found

Run `scanimage -L` first. If the scanner is missing there as well, fix the SANE
setup before troubleshooting Photo Scanner. For a network scanner, install
`sane-airscan`, make sure the scanner is reachable on the local network, and
allow up to 30 seconds for discovery. Use <kbd>Ctrl</kbd> + <kbd>R</kbd> to try
again.

### The project does not build on Ubuntu or Debian

Compare the installed libraries with the minimum versions above. Current Ubuntu
LTS and Debian Stable releases are too old for this source build; use a recent
Arch/CachyOS environment or wait for the planned Flatpak package.

### Photos are not detected correctly

Leave about 1 cm between prints and use a clean, light scanner bed. If needed,
disable automatic thresholding and adjust **Manual threshold**, **Minimum
area**, or **Additional margin** in the sidebar.

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

## Contributing

Bug reports and focused pull requests are welcome. Before opening a pull
request, run `make check` and ensure the complete suite passes. Use `make audit`
for the dependency audit.

## Roadmap

- Flatpak packaging
- an AUR package
- broader testing across SANE-supported flatbed scanners

## License

Photo Scanner is licensed under the [MIT License](LICENSE).
