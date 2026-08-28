# Ship-Readiness Plan — Photo Scanner

Actionable work plan derived from the 2026-08-28 ship-readiness review. Each task is
self-contained so a single agent can pick it up cold. Work through the phases in
order; tasks inside a phase are independent unless a dependency is named.

## Global conventions (read first, apply to every task)

- Language of code, identifiers, and comments: follow the existing file. UI strings,
  error messages, and doc comments are currently **German** — keep writing German
  strings in tasks 1–11. Task 12 converts all user-facing strings to English msgids
  with a German translation; do not anticipate that conversion earlier.
- Every task must end with `make check` passing (fmt, clippy `-D warnings`, tests,
  `--help`, desktop-file and appstream validation). New behavior needs a test where
  the code is testable without hardware (splitter/scanner layers); GUI-only changes
  need at least a manual test note in the commit message.
- One task = one commit on a feature branch, conventional-commit style matching the
  existing history (`fix: …`, `feat: …`, German or English body is fine — existing
  history uses English subjects).
- Never regress the existing guarantees: no file overwrites (`persist_noclobber`,
  group publish + rollback), cancellation must reap child processes, no work on the
  GTK main thread, all subprocess calls use argument arrays (never a shell).
- Key files: `src/gui.rs` (GTK4/libadwaita UI), `src/scanner.rs` (scanimage
  process control), `src/splitter.rs` (detection + export), `src/metadata.rs`
  (exiv2), `src/cli.rs`, `src/main.rs`, `src/style.css`.

---

## Phase 1 — Quick fixes

### Task 1: Visible cancel button

**Why:** `win.cancel` exists but is only reachable via Escape (`gui.rs:731`). During a
long scan the UI shows a spinner with no visible way out.

**Steps:**
1. In `build_window` (`src/gui.rs`), create a `cancel_button: gtk::Button` with label
   `"Abbrechen"`, icon `process-stop-symbolic`, CSS class `destructive-action`,
   `action_name` `win.cancel`. Add it to the `Ui` struct.
2. In `build_sidebar`, append it to the `actions` box below `import_button`.
3. In `set_busy` (`gui.rs:1122`), call `ui.cancel_button.set_visible(busy)`. Initialize
   hidden (`set_visible(false)` after construction, or rely on the initial
   `update_control_states`/`set_busy` path — verify it starts hidden on launch).
4. Keep the existing Escape accelerator. Add accessible properties analogous to the
   other buttons (`Label`, `KeyShortcuts("Escape")`).
5. `cancel_current_work` already disables the action after the first click — verify the
   button visually reflects that (it will, via action enabled state).

**Acceptance:** Button invisible when idle; appears during device discovery, scan, and
import; clicking it cancels (status shows "Abbruch angefordert …"), and it disappears
when the operation ends. `make check` passes.

### Task 2: Fix stale device list when import supersedes discovery

**Why:** `start_import`/`start_scan` call `begin_operation`, bumping `operation_id`. A
device-discovery result arriving afterwards is dropped by the id check at the top of
`handle_message` (`gui.rs:1034`), leaving the dropdown stuck on
"Scanner werden gesucht …" with scanning disabled until a manual refresh.

**Steps:**
1. Add `discovery_pending: Rc<Cell<bool>>` to `Ui`.
2. Set it to `true` at the start of `request_devices`, to `false` whenever a
   `Message::Devices` with the **current** id is handled (both Ok and Err arms).
3. In `handle_message`, after fully handling a `Message::Work` result (both arms,
   after `set_busy(ui, false, …)`), check `discovery_pending.get()` — if true, the
   discovery this flag belongs to was cancelled by `begin_operation`; call
   `request_devices(ui)` to restart it.
4. Guard against loops: `request_devices` early-returns when `ui.busy` is set, which
   stays correct; the re-request happens only after busy is cleared.
5. Also handle the window-close path: `connect_close_request` cancels the pending
   operation — no re-request must fire (the `closing` flag check at the top of
   `handle_message` already returns early; confirm ordering).

**Acceptance:** Manual test: trigger `Strg+O` immediately after launch while
"Scanner werden gesucht …" is showing, complete or cancel the import → device search
restarts automatically and the dropdown ends in a real state ("Scanner bereit" or
"Kein Scanner erkannt"). `make check` passes.

### Task 3: Repo hygiene

**Steps:**
1. Delete `.venv/` and `tests/__pycache__/` (then the now-empty `tests/` directory)
   from the working tree. They are gitignored leftovers from the Python era — nothing
   is tracked, so this is a local cleanup, no commit content besides possibly the
   empty `tests/` removal.
2. Trim `.gitignore`: remove the Python-only entries (`.venv/`, `__pycache__/`,
   `*.py[cod]`, `*.egg-info/`, `.pytest_cache/`, `.ruff_cache/`, `build/`, `dist/`)
   — keep `target/`, `output/`, `samples/`.

**Acceptance:** `git status` clean, `make check` passes.

### Task 4: Align CLI and GUI default output directory

**Why:** CLI defaults to relative `output/` (`cli.rs:79`), GUI to
`~/Bilder/PhotoScanner` (`gui.rs:434`).

**Steps:**
1. Move `default_output_directory()` from `src/gui.rs` into `src/lib.rs` (or a small
   shared module) as a public function.
2. In `src/cli.rs`, drop the static `default_value = "output"`; make `output:
   Option<PathBuf>` and resolve `export.output.unwrap_or_else(default_output_directory)`
   at use sites, so `--help` can still show the default via `default_value_os_t` if
   preferred (either mechanism is fine; keep `--help` informative).
3. Update README "Kommandozeile" section to mention the default.

**Acceptance:** `photoscanner split scan.png` writes to `~/Bilder/PhotoScanner` (or
`output/` only if `dirs::picture_dir()` is unavailable, matching the GUI fallback).
Existing tests untouched (they always pass `--output`/explicit dirs). `make check`.

---

## Phase 2 — UX improvements

### Task 5: Settings persistence

**Why:** Output folder, DPI, format, mode, quality, and detection parameters reset
every launch.

**Design decision (made):** use `glib::KeyFile` at
`config_directory().join("settings.ini")` — consistent with the existing
`theme.css` location, no schema compilation, no new dependency (glib is already
there). Do not use GSettings.

**Steps:**
1. New module `src/gui_settings.rs` (or a section in `gui.rs`) with a plain struct
   `PersistedSettings { output_directory, dpi_index, format_index, mode_index,
   quality, min_area, padding, auto_threshold, threshold, capture_date: Option<NaiveDate> }`
   and `load() -> PersistedSettings` / `save(&self) -> Result<()>`.
2. `load()` reads the KeyFile, tolerates a missing/corrupt file (return defaults),
   and **clamps every numeric value to the widget ranges** already used in
   `build_window` (dpi/format/mode indices bounds-checked against the model sizes).
3. Apply loaded values in `build_window` after the widgets exist, before
   `connect_actions`.
4. Save on two triggers: in `connect_close_request`, and after every successful
   `Message::Work` (so a crash loses nothing important). Collect current widget
   state into the struct; write with `KeyFile::save_to_file`. Ignore save errors
   apart from a `show_error` toast on the close path being unnecessary — log to
   stderr instead (never block closing).
5. `capture_date`: store the last **successfully used** date (set it where
   `collect_config` succeeds inside a completed work item), ISO format `%Y-%m-%d`.
   On startup, prefill `date_entry` with it; fall back to today. (Task 8 builds on
   this.)
6. Unit-test `load`/`save` round-trip and the clamping with a temp dir (inject the
   path — make the functions take `&Path` so tests don't touch the real config).

**Acceptance:** Change output dir, DPI, format, detection values; restart; everything
restored. Corrupt `settings.ini` by hand → app starts with defaults, no crash.
`make check` passes.

### Task 6: Error details dialog

**Why:** Full `scanimage`/`exiv2` stderr currently lands in a 6-second toast and the
status label (`show_error`, `gui.rs:1154`) — unreadable for long errors.

**Steps:**
1. Add `last_error: Rc<RefCell<Option<String>>>` to `Ui`.
2. Change `show_error` to: store the full message in `last_error`; set the status
   label to a **first-line-only** summary (truncate at the first `\n`, max ~120
   chars with ellipsis); show a toast with the summary, `timeout(6)`, and — when the
   full message is longer than the summary — a toast button
   (`button_label("Details")`) wired to a new action `win.error-details`.
3. `win.error-details` opens an `adw::AlertDialog` titled "Fehlerdetails" whose extra
   child is a `gtk::ScrolledWindow` (max height ~360) containing a selectable,
   wrapping `gtk::Label` with the stored message. One "Schließen" response.
4. Keep the accessible announcement (`announce`) with the full message unchanged —
   screen-reader users should not lose information.

**Acceptance:** Provoke a long error (e.g. select a device, unplug it, scan). Toast
shows a short summary with a Details button; dialog shows the full stderr,
selectable. Short errors show no Details button. `make check` passes.

### Task 7: Scan progress reporting

**Why:** Only a spinner during scans; `scanimage --progress` provides percentages.

**Steps — scanner layer (`src/scanner.rs`):**
1. Add a progress callback to the scan path:
   `pub fn scan_to_file_with_progress(…, progress: Option<Box<dyn Fn(f64) + Send>>)`.
   Keep `scan_to_file_cancellable` as a thin wrapper passing `None` (public API
   stays source-compatible).
2. Pass `--progress` to the `scanimage` invocation in `scan_to_file_with_program`.
3. `scanimage --progress` writes lines like `Progress: 31.4%` to stderr, terminated
   with `\r`. Replace the plain `pipe_reader` on stderr with a parsing drain thread:
   read chunks, split the stream on both `\r` and `\n`, for each complete token
   match `Progress: <float>%` → invoke the callback with the value clamped to
   `0.0..=100.0`; every non-progress token goes into the capped error buffer
   (existing `STDERR_LIMIT` semantics) so real error text is preserved and progress
   spam never fills the cap.
4. Unit test with a fake scanimage script (reuse the `fake_scanimage` helper) that
   emits interleaved progress lines and an error line: assert callback values
   arrive in order and the returned/reported stderr contains only the error line.

**Steps — GUI (`src/gui.rs`):**
5. Add `Message::Progress { operation_id: u64, percent: f64 }`. In `start_scan`'s
   worker, build the callback from a clone of `ui.sender` using
   `send_blocking` — but use `try_send` semantics (`force_send` on a bounded channel
   would drop; instead simply ignore a full channel: `let _ = sender.try_send(…)`)
   so a slow main loop never blocks the drain thread.
6. In `handle_message`, on `Progress` with the current id (do **not** clear
   cancellation/hold for progress messages — restructure the top of the function so
   the take() of cancellation/application_hold only happens for terminal
   `Devices`/`Work` messages), update a new `gtk::ProgressBar` in the status card
   (`fraction = percent / 100.0`) and set the status label to
   `format!("Scanne … {percent:.0} %")`.
7. Show the progress bar only while a scan is running; hide it in `set_busy(false)`
   and for non-scan operations. Import/split have no meaningful progress — leave the
   spinner for those.

**Acceptance:** scanner-layer test green; manual scan shows a moving bar; cancelling
mid-scan still reaps the process and hides the bar. `make check` passes.

### Task 8: Date picker + sticky date (depends on Task 5)

**Steps:**
1. Keep the free-text `adw::EntryRow` (power users type fast) but add a suffix
   `gtk::MenuButton` (icon `x-office-calendar-symbolic`, flat) opening a
   `gtk::Popover` containing a `gtk::Calendar`.
2. Opening the popover: parse the entry; if valid, preselect that date on the
   calendar. On `day-selected` + activation (`day-selected` is fine), write
   `%d.%m.%Y` back into the entry and popdown.
3. Validate on `changed` (debounced is unnecessary — parse is cheap): toggle the
   existing accessible invalid state and add/remove the `error` CSS class on the row
   so users see problems before pressing scan. Keep the hard validation in
   `collect_config` unchanged.
4. Sticky default comes from Task 5 (`capture_date` in settings). Verify the entry is
   prefilled with the last used date on restart.
5. Accessible properties on the MenuButton ("Kalender öffnen").

**Acceptance:** Calendar selects into the entry; invalid text shows the error state
live; restart restores last used date. `make check` passes.

### Task 9: Preview zoom and pan

**Steps:**
1. Raise `PREVIEW_MAX_EDGE` (`gui.rs:28`) from 1600 to 3200 and update the
   `bounded_preview_limits_the_largest_edge` test accordingly.
2. Wrap `ui.picture` in a `gtk::ScrolledWindow` (both policies Automatic) inside the
   stack page. Track `zoom: Rc<Cell<f64>>` with the sentinel `0.0` = "fit" (default).
3. Fit mode: current behavior (`content_fit Contain`, `can_shrink(true)`, no size
   request). Zoom mode: set an explicit size request on the picture of
   `paintable_size * zoom` and `content_fit Fill`; the scrolled window provides pan
   (GTK4 ScrolledWindow drag-to-pan works via touch; also acceptable with scrollbars
   only).
4. Actions `win.zoom-in`, `win.zoom-out`, `win.zoom-fit` with accels `<Primary>plus`,
   `<Primary>minus`, `<Primary>0`; zoom steps ×1.25 clamped to `0.25..=4.0`; header
   bar buttons in the preview pane (`zoom-in-symbolic`, `zoom-out-symbolic`,
   `zoom-fit-best-symbolic`), only sensitive when a preview is shown.
5. `EventControllerScroll` on the preview area: Ctrl+wheel zooms around the pointer
   (adjust the ScrolledWindow adjustments after resizing to keep the pointer point
   stable; a simple center-preserving approximation is acceptable for v1).
6. Reset to fit whenever a new preview is set (`Message::Work` Ok arm).

**Acceptance:** Wheel/keyboard/buttons zoom; pan works; new scan resets to fit; empty
state unaffected. `make check` passes (updated test).

### Task 10: Batch import

**Steps — GUI:**
1. In `choose_import_file`, switch to `dialog.open_multiple(…)`; collect
   `Vec<PathBuf>`.
2. Generalize `start_import` to take `Vec<PathBuf>`: single worker thread iterates
   the files sequentially, calling the existing per-file logic; check
   `ensure_not_cancelled` between files.
3. Aggregate into one `WorkResult`: title
   `"{total} Foto(s) aus {n} Datei(en) gespeichert"`, detail = output dir +
   per-file failures (file name + first error line), preview = preview of the **last
   successful** file. If *all* files fail, return Err with the collected messages.
4. Status text during work: `"Analysiere Datei {i} von {n} …"` — send via the
   `Message::Progress` mechanism from Task 7 (`percent = i/n*100`) or a dedicated
   status variant; reuse, don't duplicate.

**Steps — CLI:**
5. `Command::Split { source: PathBuf }` → `sources: Vec<PathBuf>` with
   `#[arg(required = true, num_args = 1..)]`. Loop, print per-file results, exit
   non-zero if any file failed (after processing all).
6. README: document multi-file `split` and the multi-select import.

**Acceptance:** Import 3 files where one is corrupt → 2 processed, failure listed in
detail/dialog, exit path sane; CLI `photoscanner split a.png b.png` works.
`make check` passes.

---

## Phase 3 — Review-before-save (biggest task, do after Phase 2)

### Task 11: Review detected photos before export (rotate / deselect)

**Why:** Scans currently go straight to disk. Paper photos laid sideways/upside-down
are archived that way; false detections can't be removed.

**Design:**
- After scan/import + detection, the worker **stages** results in a `TempDir`
  instead of publishing: full-resolution warped photos as PNG
  (`photo_01.png`, …), plus thumbnails (JPEG, max edge ~360) and the marked
  overview preview. Nothing is written to the output directory yet.
- The preview pane gains a third stack page `"review"`: the overview preview on top,
  below it a `gtk::FlowBox` of thumbnail cards. Each card: thumbnail `gtk::Picture`,
  an include `gtk::CheckButton` (default on), and a rotate button
  (`object-rotate-right-symbolic`; each click +90°, quarter-turn counter per photo,
  thumbnail re-rendered via `gdk_pixbuf::Pixbuf::rotate_simple`).
- Review header actions: `"{n} Fotos speichern"` (suggested-action) and
  `"Verwerfen"`. Saving spawns a worker that loads each included staged PNG,
  applies `opencv::core::rotate` per its quarter-turn count, and publishes the group
  through the existing collision-free path. Discard drops the TempDir.
- "Gesamte Scanfläche speichern" mode keeps the current direct-save behavior
  (out of scope for review).
- Cancellation: window close or a new operation while a review is open discards the
  staged TempDir (it's a `TempDir`, so dropping it is enough — make sure the review
  state struct owns it).

**Steps — splitter (`src/splitter.rs`):**
1. Make the region type public: rename `PhotoRegion` → `pub struct DetectedRegion`
   (fields pub) or export the existing `DetectedPhoto` path — pick the minimal
   surface: `pub fn analyze_scan(source, config) -> Result<AnalyzedScan>` where
   `AnalyzedScan { image: Mat, regions: Vec<DetectedRegion>, threshold: f64,
   embedded_dpi: Option<u32> }` (wraps `read_image` + `detect_photo_regions`).
2. `pub fn export_photos(photos: &[Mat], analyzed: &AnalyzedScan (or the needed
   parts), output_directory, config, prefix, preview_regions: Option<&[DetectedRegion]>)
   -> Result<SplitResult>` — extract the staging/publishing second half of
   `split_scan` (stage each Mat, optional preview from `image` + the included
   regions, `publish_staged_group`). The preview must mark only the *included*
   regions, renumbered 1..n in export order.
3. Reimplement `split_scan` as `analyze_scan` + warp + `export_photos` so the CLI and
   all existing tests keep their exact behavior (numbering, rollback, no-clobber).
4. New tests: `export_photos` respects rotation (feed a rotated Mat, check output
   dims), subset export renumbers files `_01.._0n` contiguously, preview marks only
   included regions.

**Steps — GUI (`src/gui.rs`):**
5. Extend the worker result: new variant `Message::ReviewReady { operation_id,
   review: ReviewData }` with `ReviewData { staging: TempDir, photos:
   Vec<ReviewPhoto>, preview: PathBuf, threshold: f64, source_meta: (config bits
   needed for export: dpi, capture date, format, quality, output dir) }`,
   `ReviewPhoto { full_path: PathBuf, thumbnail_path: PathBuf, region:
   DetectedRegion, quarter_turns: Cell<u8>, include: Cell<bool> }`.
6. `scan_work`/import worker: in split mode call `analyze_scan`, warp each region,
   write staged PNGs + thumbnails into the TempDir, build `ReviewData`. Full-scan
   mode unchanged (still returns `Message::Work`).
7. On `ReviewReady`: `set_busy(false, "…{n} Fotos erkannt – bitte prüfen")`, build the
   review page (rebuild FlowBox children each time), show stack page `"review"`.
   Keep the sidebar sensitive except scan/import (a new scan replaces the review —
   acceptable; `begin_operation` + explicit drop of the stored review state).
8. Save action: collect included photos + turns, spawn worker → `core::rotate` +
   `export_photos`, report via the existing `Message::Work` path (toast, final
   preview page shows the marked overview). Discard action: drop review state, back
   to `"picture"`/`"empty"` page.
9. Keyboard: `Enter`/default widget = save while review is open; document `F9`
   starts a new scan and discards.
10. A setting `"Vor dem Speichern prüfen"` (Switch in the sidebar, persisted via
    Task 5, default **on**) lets automation-minded users keep the old direct-save
    flow. When off, behavior is exactly as before this task.

**Acceptance:** Scan with 3 photos → review page with 3 cards; deselect one, rotate
another twice, save → 2 files, rotated one is 180°, names `_01`/`_02`, preview marks
2 regions; discard leaves output directory untouched; switch off → old flow.
All splitter tests green, new tests added. `make check` passes.

---

## Phase 4 — Internationalization and distribution

### Task 12: i18n — English + German (gettext)

**Why:** Everything is hardcoded German, including Arch-specific hints
(`scanner.rs:49`). Target: English as source language (msgids), German as the first
translation, runtime selection via the user's locale.

**Steps:**
1. Add dependency `gettext-rs` (crate `gettext-rs`, package feature
  `gettext-system` so the system libintl is used — glibc has it built in).
2. `src/main.rs`: before anything else call `setlocale(LocaleCategory::LcAll, "")`,
   `bindtextdomain("photoscanner", locale_dir)`, `textdomain("photoscanner")`,
   `bind_textdomain_codeset("photoscanner", "UTF-8")`. `locale_dir`: try
   `$XDG_DATA_HOME/locale` (user install), fall back to `/usr/share/locale`.
   Make it a small helper in `lib.rs`.
3. Convert **all user-facing strings to English** and wrap them:
   - `gui.rs`: every label, title, subtitle, tooltip, status text, toast, accessible
     property → `gettext("…")`; formatted strings via `gettext` + runtime formatting
     (small helper `fn tr(s: &str) -> String`, and for placeholders keep
     `format!` around gettext'd templates with named `{}` order preserved — for
     plural forms `ngettext` where counts appear: "N photo(s) saved" →
     `ngettext("One photo saved", "{} photos saved", n)`).
   - `scanner.rs`/`splitter.rs`/`metadata.rs`: the `thiserror` `#[error("…")]`
     derives can't call gettext. Replace the derive **messages** with manual
     `impl Display` blocks that call `gettext!`/`ngettext` (keep `#[derive(Error)]`
     for `source()` via `#[source]` attributes; `thiserror` allows omitting
     `#[error]` only without derive — so implement `std::fmt::Display` by hand and
     derive only `Debug` + implement `std::error::Error` manually, or keep
     `thiserror` with `#[error(transparent)]` wrappers; choose the simpler manual
     Display + `impl Error` with `source()`).
   - Distro hints: generalize "Unter CachyOS/Arch kann es mit 'sudo pacman -S …'
     installiert werden" → English msgid "Install it with your package manager
     (e.g. 'sudo pacman -S sane sane-airscan' on Arch)." German translation may keep
     the CachyOS wording.
   - `cli.rs`: clap `about`/help texts — use `#[command(about = …)]` with
     `&'static str` from a `once_cell`/`LazyLock` gettext lookup, or simpler: leave
     clap help English-only and note it in README (decide: **leave CLI help
     English**, translate only runtime messages — lower risk, standard practice).
4. Create `po/` infrastructure:
   - `po/POTFILES` listing the Rust sources.
   - Extract with `xtr` (`cargo install xtr`; `xtr src/main.rs -o po/photoscanner.pot`
     — xtr walks the module tree). Add `make pot` target.
   - `po/de.po` translated fully (the current German strings ARE the translation —
     move them there verbatim).
   - `make locale` target: `msgfmt po/de.po -o … /de/LC_MESSAGES/photoscanner.mo`;
     hook into `install-user` (install to `~/.local/share/locale/de/LC_MESSAGES/`)
     and `make check` (add `msgfmt --check` for every po file).
5. Desktop file + metainfo: English default `Name=`/`Comment=`/`<summary>` etc. with
   `[de]`/`xml:lang="de"` entries for the German versions.
6. README: rewrite `README.md` in English; move the German original to
   `README.de.md`; cross-link both at the top.
7. Tests that assert German message fragments (e.g.
   `rejects_oversized_png_before_decoding_it` checks `"zu groß"`): run tests under
   `LC_ALL=C` semantics — assert the **English** msgid now; ensure `make check`
   exports `LC_ALL=C` for `cargo test` so CI is locale-independent.
8. CI: add `gettext` package (provides msgfmt/xgettext) and `cargo install xtr` is
   NOT needed in CI if the pot file is committed — commit `photoscanner.pot` and
   `de.po`, validate with `msgfmt --check` only.

**Acceptance:** `LANG=de_DE.UTF-8 photoscanner gui` fully German;
`LANG=en_US.UTF-8` fully English; missing translation falls back to English;
`make check` (incl. msgfmt check) green.

### Task 13 (optional, for public distribution): metainfo polish + Flatpak

Only start when a public repo URL exists.

1. Metainfo: add `<url type="homepage">` (repo URL), 2+ `<screenshot>` entries with
   hosted PNGs (1600×~1000, light + dark), `<branding>` colors; then drop the
   `--override=url-homepage-missing=pedantic` from the Makefile appstream call.
2. Screenshots: take with a synthetic preview loaded (use `samples/`), both themes.
3. Flatpak manifest `de.martin.PhotoScanner.json`: `org.gnome.Platform` runtime
   (matching GNOME 49+), modules for `opencv` (build with the same feature set:
   imgcodecs/imgproc only), `sane-backends`, `sane-airscan`, `exiv2`.
   Finish-args: `--socket=wayland`, `--socket=fallback-x11`, `--device=all` (USB
   scanners), `--share=network` (AirScan/eSCL), `--filesystem=xdg-pictures`.
   Known pain: SANE in Flatpak reaches USB scanners via `--device=all` and network
   scanners natively; document limitations in README.
4. CI job that builds the Flatpak (`flatpak-builder`) on tags.

**Acceptance:** `appstreamcli validate --strict` passes without overrides; Flatpak
builds and scans via AirScan from inside the sandbox.

---

## Suggested execution order

| Order | Task | Size | Depends on |
|-------|------|------|-----------|
| 1 | 3 Repo hygiene | XS | — |
| 2 | 1 Cancel button | S | — |
| 3 | 2 Device-list race | S | — |
| 4 | 4 Output default | S | — |
| 5 | 5 Settings persistence | M | — |
| 6 | 6 Error dialog | S | — |
| 7 | 7 Scan progress | M | — |
| 8 | 8 Date picker | S | 5 |
| 9 | 9 Zoom/pan | M | — |
| 10 | 10 Batch import | M | 7 (status reuse) |
| 11 | 11 Review-before-save | L | 5 (setting), ideally after 6–10 |
| 12 | 12 i18n EN/DE | L | after 1–11 (string sweep) |
| 13 | 13 Flatpak/metainfo | M | 12, public repo |
