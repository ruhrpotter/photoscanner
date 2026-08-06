use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, SystemTime};

use adw::prelude::*;
use anyhow::{Context, Result, anyhow, bail};
use chrono::{Local, NaiveDate};
use gtk::gio;
use gtk::glib;
use photoscanner::scanner::{ScannerDevice, list_devices, scan_to_file};
use photoscanner::splitter::{OutputFormat, SplitConfig, save_full_scan, split_scan};
use photoscanner::{APP_ID, APP_NAME};
use tempfile::TempDir;

const DEFAULT_STYLE: &str = include_str!("style.css");

#[derive(Clone)]
struct Ui {
    window: adw::ApplicationWindow,
    toast_overlay: adw::ToastOverlay,
    split_view: adw::OverlaySplitView,
    device_model: gtk::StringList,
    device_dropdown: gtk::DropDown,
    mode_dropdown: gtk::DropDown,
    dpi_dropdown: gtk::DropDown,
    format_dropdown: gtk::DropDown,
    date_entry: adw::EntryRow,
    auto_threshold: gtk::Switch,
    threshold: gtk::SpinButton,
    min_area: gtk::SpinButton,
    padding: gtk::SpinButton,
    quality: gtk::SpinButton,
    output_button: gtk::Button,
    scan_button: gtk::Button,
    import_button: gtk::Button,
    refresh_button: gtk::Button,
    spinner: gtk::Spinner,
    status_label: gtk::Label,
    preview_stack: gtk::Stack,
    picture: gtk::Picture,
    devices: Rc<RefCell<Vec<ScannerDevice>>>,
    output_directory: Rc<RefCell<PathBuf>>,
    busy: Rc<Cell<bool>>,
    sender: Sender<Message>,
}

enum Message {
    Devices(Result<Vec<ScannerDevice>, String>),
    Work(Result<WorkResult, String>),
}

struct WorkResult {
    title: String,
    detail: String,
    preview: PathBuf,
}

pub fn run() {
    let application = adw::Application::builder().application_id(APP_ID).build();
    application.connect_startup(|_| install_theme());
    application.connect_activate(build_window);
    // Clap has already consumed our command line. Passing the original `gui`
    // argument to GApplication would make it look like a file-open request.
    application.run_with_args(&["photoscanner"]);
}

fn config_directory() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("photoscanner")
}

fn install_theme() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let bundled = gtk::CssProvider::new();
    bundled.load_from_string(DEFAULT_STYLE);
    gtk::style_context_add_provider_for_display(
        &display,
        &bundled,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let directory = config_directory();
    let path = directory.join("theme.css");
    let _ = fs::create_dir_all(&directory);
    let custom = gtk::CssProvider::new();
    load_custom_theme(&custom, &path);
    gtk::style_context_add_provider_for_display(
        &display,
        &custom,
        gtk::STYLE_PROVIDER_PRIORITY_USER,
    );

    let mut last_modified = modified(&path);
    glib::timeout_add_seconds_local(1, move || {
        let current = modified(&path);
        if current != last_modified {
            load_custom_theme(&custom, &path);
            last_modified = current;
        }
        glib::ControlFlow::Continue
    });
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn load_custom_theme(provider: &gtk::CssProvider, path: &Path) {
    if path.is_file() {
        provider.load_from_path(path);
    } else {
        provider.load_from_string("");
    }
}

fn build_window(application: &adw::Application) {
    let (sender, receiver) = mpsc::channel();
    let devices = Rc::new(RefCell::new(Vec::new()));
    let output_directory = Rc::new(RefCell::new(default_output_directory()));
    let busy = Rc::new(Cell::new(false));

    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title(APP_NAME)
        .default_width(1240)
        .default_height(820)
        .width_request(520)
        .height_request(540)
        .build();

    let split_view = adw::OverlaySplitView::builder()
        .min_sidebar_width(340.0)
        .max_sidebar_width(420.0)
        .sidebar_width_fraction(0.32)
        .enable_hide_gesture(true)
        .enable_show_gesture(true)
        .build();
    split_view.add_css_class("app-shell");

    let device_model = gtk::StringList::new(&["Scanner werden gesucht …"]);
    let device_dropdown = gtk::DropDown::builder()
        .model(&device_model)
        .hexpand(true)
        .build();
    device_dropdown.add_css_class("compact-control");

    let mode_dropdown =
        gtk::DropDown::from_strings(&["Fotos automatisch trennen", "Gesamte Scanfläche speichern"]);
    mode_dropdown.set_selected(0);
    mode_dropdown.add_css_class("compact-control");

    let dpi_dropdown = gtk::DropDown::from_strings(&["300 dpi", "600 dpi", "1200 dpi"]);
    dpi_dropdown.set_selected(1);
    dpi_dropdown.add_css_class("compact-control");

    let format_dropdown = gtk::DropDown::from_strings(&["JPG", "PNG", "TIFF"]);
    format_dropdown.set_selected(0);
    format_dropdown.add_css_class("compact-control");

    let date_entry = adw::EntryRow::builder()
        .title("Aufnahmedatum")
        .text(Local::now().format("%d.%m.%Y").to_string())
        .input_purpose(gtk::InputPurpose::Digits)
        .build();

    let auto_threshold = gtk::Switch::builder()
        .active(true)
        .valign(gtk::Align::Center)
        .build();
    let threshold = gtk::SpinButton::with_range(1.0, 255.0, 1.0);
    threshold.set_value(12.0);
    threshold.set_sensitive(false);
    threshold.set_valign(gtk::Align::Center);
    let min_area = gtk::SpinButton::with_range(0.1, 50.0, 0.1);
    min_area.set_value(2.0);
    min_area.set_digits(1);
    let padding = gtk::SpinButton::with_range(0.0, 15.0, 0.1);
    padding.set_value(1.2);
    padding.set_digits(1);
    let quality = gtk::SpinButton::with_range(1.0, 100.0, 1.0);
    quality.set_value(95.0);

    let output_button = gtk::Button::builder()
        .label(short_path(&output_directory.borrow()))
        .tooltip_text(output_directory.borrow().display().to_string())
        .valign(gtk::Align::Center)
        .build();
    let refresh_button = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Scanner neu suchen")
        .valign(gtk::Align::Center)
        .build();
    refresh_button.add_css_class("flat");

    let scan_button = gtk::Button::builder()
        .label("Scan starten")
        .icon_name("document-send-symbolic")
        .hexpand(true)
        .build();
    scan_button.add_css_class("suggested-action");
    scan_button.add_css_class("primary-action");
    let import_button = gtk::Button::builder()
        .label("Scandatei öffnen")
        .icon_name("folder-open-symbolic")
        .hexpand(true)
        .build();
    import_button.add_css_class("primary-action");

    let spinner = gtk::Spinner::new();
    let status_label = gtk::Label::builder()
        .label("Bereit zum Scannen")
        .xalign(0.0)
        .wrap(true)
        .hexpand(true)
        .build();
    let (preview_stack, picture) = build_preview();

    let ui = Ui {
        window: window.clone(),
        toast_overlay: adw::ToastOverlay::new(),
        split_view: split_view.clone(),
        device_model,
        device_dropdown,
        mode_dropdown,
        dpi_dropdown,
        format_dropdown,
        date_entry,
        auto_threshold,
        threshold,
        min_area,
        padding,
        quality,
        output_button,
        scan_button,
        import_button,
        refresh_button,
        spinner,
        status_label,
        preview_stack,
        picture,
        devices,
        output_directory,
        busy,
        sender,
    };

    split_view.set_sidebar(Some(&build_sidebar(&ui)));
    split_view.set_content(Some(&build_preview_pane(&ui)));
    ui.toast_overlay.set_child(Some(&split_view));
    window.set_content(Some(&ui.toast_overlay));

    if let Ok(condition) = adw::BreakpointCondition::parse("max-width: 860sp") {
        let breakpoint = adw::Breakpoint::new(condition);
        let collapsed = true.to_value();
        breakpoint.add_setter(&split_view, "collapsed", Some(&collapsed));
        window.add_breakpoint(breakpoint);
    }

    connect_actions(&ui);
    let receiver_ui = ui.clone();
    glib::timeout_add_local(Duration::from_millis(80), move || {
        while let Ok(message) = receiver.try_recv() {
            handle_message(&receiver_ui, message);
        }
        glib::ControlFlow::Continue
    });

    request_devices(&ui);
    window.present();
}

fn default_output_directory() -> PathBuf {
    dirs::picture_dir()
        .unwrap_or_else(|| PathBuf::from("output"))
        .join("PhotoScanner")
}

fn short_path(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Ausgabeordner")
        .to_string()
}

fn build_sidebar(ui: &Ui) -> adw::ToolbarView {
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        APP_NAME,
        "Papierfotos digitalisieren",
    )));
    toolbar.add_top_bar(&header);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.add_css_class("scanner-sidebar");

    let scanner_group = adw::PreferencesGroup::builder().title("Scan").build();
    scanner_group.add(&row_with_suffix("Scanner", None, &ui.device_dropdown));
    let refresh_row = adw::ActionRow::builder()
        .title("Geräte aktualisieren")
        .subtitle("SANE und AirScan erneut abfragen")
        .build();
    refresh_row.add_suffix(&ui.refresh_button);
    refresh_row.set_activatable_widget(Some(&ui.refresh_button));
    scanner_group.add(&refresh_row);
    scanner_group.add(&row_with_suffix("Verarbeitung", None, &ui.mode_dropdown));
    scanner_group.add(&row_with_suffix("Auflösung", None, &ui.dpi_dropdown));
    scanner_group.add(&ui.date_entry);
    content.append(&scanner_group);

    let export_group = adw::PreferencesGroup::builder().title("Ausgabe").build();
    let output_row = adw::ActionRow::builder()
        .title("Ordner")
        .subtitle("Fotos und Kontrollvorschau")
        .build();
    output_row.add_suffix(&ui.output_button);
    output_row.set_activatable_widget(Some(&ui.output_button));
    export_group.add(&output_row);
    export_group.add(&row_with_suffix("Bildformat", None, &ui.format_dropdown));
    export_group.add(&row_with_suffix(
        "JPEG-Qualität",
        Some("Nur für exportierte JPG-Fotos"),
        &ui.quality,
    ));
    content.append(&export_group);

    let detection_group = adw::PreferencesGroup::builder()
        .title("Erkennung")
        .description("Die Automatik ist für helle Scannerflächen optimiert.")
        .build();
    let auto_row = adw::ActionRow::builder()
        .title("Schwellwert automatisch")
        .subtitle("Hintergrundrauschen am Rand auswerten")
        .build();
    auto_row.add_suffix(&ui.auto_threshold);
    auto_row.set_activatable_widget(Some(&ui.auto_threshold));
    detection_group.add(&auto_row);
    detection_group.add(&row_with_suffix(
        "Manueller Schwellwert",
        None,
        &ui.threshold,
    ));
    detection_group.add(&row_with_suffix(
        "Mindestfläche (%)",
        Some("Kleine Staub- und Randflächen ignorieren"),
        &ui.min_area,
    ));
    detection_group.add(&row_with_suffix(
        "Zusätzlicher Rand (%)",
        Some("Etwas Fläche um jedes Foto erhalten"),
        &ui.padding,
    ));
    content.append(&detection_group);

    let actions = gtk::Box::new(gtk::Orientation::Vertical, 10);
    actions.append(&ui.scan_button);
    actions.append(&ui.import_button);
    content.append(&actions);

    let status = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    status.set_margin_top(4);
    status.set_margin_bottom(4);
    status.set_margin_start(12);
    status.set_margin_end(12);
    status.set_valign(gtk::Align::Center);
    status.add_css_class("status-card");
    status.append(&ui.spinner);
    status.append(&ui.status_label);
    content.append(&status);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&content)
        .build();
    toolbar.set_content(Some(&scroll));
    toolbar
}

fn row_with_suffix(
    title: &str,
    subtitle: Option<&str>,
    widget: &impl IsA<gtk::Widget>,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    if let Some(subtitle) = subtitle {
        row.set_subtitle(subtitle);
    }
    row.add_suffix(widget);
    row
}

fn build_preview() -> (gtk::Stack, gtk::Picture) {
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .hexpand(true)
        .vexpand(true)
        .build();

    let empty = gtk::Box::new(gtk::Orientation::Vertical, 12);
    empty.set_halign(gtk::Align::Center);
    empty.set_valign(gtk::Align::Center);
    let icon = gtk::Image::from_icon_name("scanner-symbolic");
    icon.set_pixel_size(72);
    icon.add_css_class("empty-preview-icon");
    let title = gtk::Label::builder().label("Noch keine Vorschau").build();
    title.add_css_class("title-2");
    let description = gtk::Label::builder()
        .label("Starte einen Scan oder öffne eine vorhandene Scandatei.")
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    description.add_css_class("dim-label");
    empty.append(&icon);
    empty.append(&title);
    empty.append(&description);

    let stack = gtk::Stack::builder()
        .hexpand(true)
        .vexpand(true)
        .transition_type(gtk::StackTransitionType::Crossfade)
        .transition_duration(180)
        .build();
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&picture, Some("picture"));
    stack.set_visible_child_name("empty");
    (stack, picture)
}

fn build_preview_pane(ui: &Ui) -> adw::ToolbarView {
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        "Vorschau",
        "Erkannte Fotogrenzen",
    )));
    let sidebar_button = gtk::Button::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text("Einstellungen ein- oder ausblenden")
        .build();
    let split_view = ui.split_view.clone();
    sidebar_button
        .connect_clicked(move |_| split_view.set_show_sidebar(!split_view.shows_sidebar()));
    header.pack_start(&sidebar_button);
    toolbar.add_top_bar(&header);

    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.set_margin_top(24);
    frame.set_margin_bottom(24);
    frame.set_margin_start(24);
    frame.set_margin_end(24);
    frame.add_css_class("preview-card");
    frame.append(&ui.preview_stack);

    let shell = gtk::Box::new(gtk::Orientation::Vertical, 0);
    shell.set_hexpand(true);
    shell.set_vexpand(true);
    shell.add_css_class("preview-shell");
    shell.append(&frame);
    toolbar.set_content(Some(&shell));
    toolbar
}

fn connect_actions(ui: &Ui) {
    let threshold = ui.threshold.clone();
    ui.auto_threshold
        .connect_active_notify(move |switch| threshold.set_sensitive(!switch.is_active()));

    let output_ui = ui.clone();
    ui.output_button
        .connect_clicked(move |_| choose_output_directory(&output_ui));

    let refresh_ui = ui.clone();
    ui.refresh_button
        .connect_clicked(move |_| request_devices(&refresh_ui));

    let scan_ui = ui.clone();
    ui.scan_button
        .connect_clicked(move |_| start_scan(&scan_ui));

    let import_ui = ui.clone();
    ui.import_button
        .connect_clicked(move |_| choose_import_file(&import_ui));
}

fn choose_output_directory(ui: &Ui) {
    if ui.busy.get() {
        return;
    }
    let dialog = gtk::FileDialog::builder()
        .title("Ausgabeordner auswählen")
        .modal(true)
        .build();
    dialog.set_initial_folder(Some(&gio::File::for_path(&*ui.output_directory.borrow())));
    let output = ui.output_directory.clone();
    let button = ui.output_button.clone();
    dialog.select_folder(Some(&ui.window), None::<&gio::Cancellable>, move |result| {
        if let Ok(file) = result
            && let Some(path) = file.path()
        {
            button.set_label(&short_path(&path));
            button.set_tooltip_text(Some(&path.display().to_string()));
            *output.borrow_mut() = path;
        }
    });
}

fn choose_import_file(ui: &Ui) {
    if ui.busy.get() {
        return;
    }
    let dialog = gtk::FileDialog::builder()
        .title("Scandatei öffnen")
        .modal(true)
        .build();
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Bilder"));
    filter.add_mime_type("image/png");
    filter.add_mime_type("image/jpeg");
    filter.add_mime_type("image/tiff");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    dialog.set_filters(Some(&filters));
    let import_ui = ui.clone();
    dialog.open(Some(&ui.window), None::<&gio::Cancellable>, move |result| {
        if let Ok(file) = result
            && let Some(path) = file.path()
        {
            start_import(&import_ui, path);
        }
    });
}

fn collect_config(ui: &Ui, scanned: bool) -> Result<SplitConfig> {
    let capture_date = NaiveDate::parse_from_str(ui.date_entry.text().trim(), "%d.%m.%Y")
        .context("Aufnahmedatum muss als TT.MM.JJJJ angegeben werden")?;
    let dpi = match ui.dpi_dropdown.selected() {
        0 => 300,
        2 => 1200,
        _ => 600,
    };
    let output_format = match ui.format_dropdown.selected() {
        1 => OutputFormat::Png,
        2 => OutputFormat::Tiff,
        _ => OutputFormat::Jpeg,
    };
    Ok(SplitConfig {
        min_area_percent: ui.min_area.value(),
        threshold: (!ui.auto_threshold.is_active()).then(|| ui.threshold.value()),
        padding_percent: ui.padding.value(),
        output_format,
        jpeg_quality: ui.quality.value_as_int(),
        dpi: scanned.then_some(dpi),
        capture_date: Some(capture_date),
        ..SplitConfig::default()
    })
}

fn start_scan(ui: &Ui) {
    if ui.busy.get() {
        return;
    }
    let selected = ui.device_dropdown.selected() as usize;
    let Some(device) = ui.devices.borrow().get(selected).cloned() else {
        show_error(ui, "Es ist kein Scanner ausgewählt.");
        return;
    };
    let config = match collect_config(ui, true) {
        Ok(config) => config,
        Err(error) => {
            show_error(ui, &error.to_string());
            return;
        }
    };
    let full_scan = ui.mode_dropdown.selected() == 1;
    let dpi = config.dpi.unwrap_or(600);
    let output = ui.output_directory.borrow().clone();
    let sender = ui.sender.clone();
    set_busy(ui, true, &format!("Scanne mit {dpi} dpi …"));
    thread::spawn(move || {
        let result = scan_work(&device, dpi, &output, &config, full_scan)
            .map_err(|error| format!("{error:#}"));
        let _ = sender.send(Message::Work(result));
    });
}

fn scan_work(
    device: &ScannerDevice,
    dpi: u32,
    output: &Path,
    config: &SplitConfig,
    full_scan: bool,
) -> Result<WorkResult> {
    let temporary = TempDir::with_prefix("photoscanner-")?;
    let source = temporary.path().join("scan.png");
    scan_to_file(&source, Some(&device.name), dpi, Duration::from_secs(600))?;
    if full_scan {
        let path = save_full_scan(&source, output, config, None)?;
        return Ok(WorkResult {
            title: "Vollständiger Scan gespeichert".to_string(),
            detail: path.display().to_string(),
            preview: path,
        });
    }
    let result = split_scan(&source, output, config, None, true)?;
    let preview = result
        .preview
        .clone()
        .or_else(|| result.files.first().cloned())
        .ok_or_else(|| anyhow!("Keine Vorschaudatei erzeugt"))?;
    Ok(WorkResult {
        title: format!("{} Foto(s) gespeichert", result.files.len()),
        detail: format!(
            "{} · Schwellwert {:.1}",
            output.display(),
            result.threshold_used
        ),
        preview,
    })
}

fn start_import(ui: &Ui, source: PathBuf) {
    if ui.busy.get() {
        return;
    }
    let config = match collect_config(ui, false) {
        Ok(config) => config,
        Err(error) => {
            show_error(ui, &error.to_string());
            return;
        }
    };
    let output = ui.output_directory.borrow().clone();
    let sender = ui.sender.clone();
    set_busy(ui, true, "Analysiere Scandatei …");
    thread::spawn(move || {
        let result = (|| -> Result<WorkResult> {
            if !source.is_file() {
                bail!("Die Scandatei existiert nicht mehr: {}", source.display());
            }
            let result = split_scan(&source, &output, &config, None, true)?;
            let preview = result
                .preview
                .clone()
                .or_else(|| result.files.first().cloned())
                .ok_or_else(|| anyhow!("Keine Vorschaudatei erzeugt"))?;
            Ok(WorkResult {
                title: format!("{} Foto(s) aus Datei gespeichert", result.files.len()),
                detail: format!(
                    "{} · Schwellwert {:.1}",
                    output.display(),
                    result.threshold_used
                ),
                preview,
            })
        })()
        .map_err(|error| format!("{error:#}"));
        let _ = sender.send(Message::Work(result));
    });
}

fn request_devices(ui: &Ui) {
    if ui.busy.get() {
        return;
    }
    ui.refresh_button.set_sensitive(false);
    ui.scan_button.set_sensitive(false);
    ui.status_label.set_label("Suche Scanner …");
    ui.spinner.start();
    let sender = ui.sender.clone();
    thread::spawn(move || {
        let result = list_devices(Duration::from_secs(15)).map_err(|error| error.to_string());
        let _ = sender.send(Message::Devices(result));
    });
}

fn handle_message(ui: &Ui, message: Message) {
    match message {
        Message::Devices(Ok(devices)) => {
            while ui.device_model.n_items() > 0 {
                ui.device_model.remove(0);
            }
            for device in &devices {
                ui.device_model.append(&device.label());
            }
            *ui.devices.borrow_mut() = devices;
            ui.device_dropdown.set_selected(0);
            ui.refresh_button.set_sensitive(true);
            ui.spinner.stop();
            if ui.devices.borrow().is_empty() {
                ui.device_model.append("Kein Scanner erkannt");
                ui.scan_button.set_sensitive(false);
                ui.status_label.set_label("Kein Scanner erkannt");
            } else {
                ui.scan_button.set_sensitive(true);
                ui.status_label.set_label("Scanner bereit");
            }
        }
        Message::Devices(Err(error)) => {
            ui.refresh_button.set_sensitive(true);
            ui.scan_button
                .set_sensitive(!ui.devices.borrow().is_empty());
            ui.spinner.stop();
            ui.status_label.set_label("Scannersuche fehlgeschlagen");
            show_error(ui, &error);
        }
        Message::Work(Ok(result)) => {
            set_busy(ui, false, &result.title);
            ui.status_label
                .set_label(&format!("{}\n{}", result.title, result.detail));
            ui.picture
                .set_file(Some(&gio::File::for_path(&result.preview)));
            ui.preview_stack.set_visible_child_name("picture");
            ui.toast_overlay.add_toast(adw::Toast::new(&result.title));
        }
        Message::Work(Err(error)) => {
            set_busy(ui, false, "Vorgang fehlgeschlagen");
            show_error(ui, &error);
        }
    }
}

fn set_busy(ui: &Ui, busy: bool, status: &str) {
    ui.busy.set(busy);
    ui.scan_button
        .set_sensitive(!busy && !ui.devices.borrow().is_empty());
    ui.import_button.set_sensitive(!busy);
    ui.refresh_button.set_sensitive(!busy);
    ui.output_button.set_sensitive(!busy);
    ui.status_label.set_label(status);
    if busy {
        ui.spinner.start();
    } else {
        ui.spinner.stop();
    }
}

fn show_error(ui: &Ui, message: &str) {
    ui.status_label.set_label(message);
    ui.toast_overlay
        .add_toast(adw::Toast::builder().title(message).timeout(6).build());
}
