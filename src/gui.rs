use std::cell::{Cell, RefCell};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::thread;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result, anyhow, bail};
use async_channel::Sender;
use chrono::{Local, NaiveDate};
use gtk::gio;
use gtk::glib;
use opencv::core::{Size, Vector};
use opencv::imgcodecs;
use opencv::imgproc;
use opencv::prelude::*;
use photoscanner::scanner::{
    DEVICE_DISCOVERY_TIMEOUT, ScannerCancellation, ScannerDevice, list_devices_cancellable,
    scan_to_file_cancellable,
};
use photoscanner::splitter::{OutputFormat, SplitConfig, save_full_scan, split_scan};
use photoscanner::{APP_ID, APP_NAME};
use tempfile::TempDir;

const DEFAULT_STYLE: &str = include_str!("style.css");
const PREVIEW_MAX_EDGE: i32 = 1600;
const CANCELLED_MESSAGE: &str = "Vorgang abgebrochen.";
const WORKER_PANIC_MESSAGE: &str = "Interner Fehler im Hintergrundprozess.";

struct Ui {
    window: adw::ApplicationWindow,
    toast_overlay: adw::ToastOverlay,
    split_view: adw::OverlaySplitView,
    device_model: gtk::StringList,
    device_dropdown: adw::ComboRow,
    mode_dropdown: adw::ComboRow,
    dpi_dropdown: adw::ComboRow,
    format_dropdown: adw::ComboRow,
    date_entry: adw::EntryRow,
    auto_threshold: gtk::Switch,
    threshold: gtk::SpinButton,
    min_area: gtk::SpinButton,
    padding: gtk::SpinButton,
    quality: gtk::SpinButton,
    output_button: gtk::Button,
    scan_button: gtk::Button,
    import_button: gtk::Button,
    cancel_button: gtk::Button,
    refresh_button: gtk::Button,
    spinner: gtk::Spinner,
    status_label: gtk::Label,
    preview_stack: gtk::Stack,
    picture: gtk::Picture,
    devices: Rc<RefCell<Vec<ScannerDevice>>>,
    output_directory: Rc<RefCell<PathBuf>>,
    busy: Rc<Cell<bool>>,
    sender: Sender<Message>,
    operation_id: Rc<Cell<u64>>,
    cancellation: Rc<RefCell<Option<ScannerCancellation>>>,
    application_hold: Rc<RefCell<Option<gio::ApplicationHoldGuard>>>,
    closing: Rc<Cell<bool>>,
    preview_directory: Rc<RefCell<Option<TempDir>>>,
    theme_monitor: Rc<RefCell<Option<gio::FileMonitor>>>,
    scan_action: gio::SimpleAction,
    import_action: gio::SimpleAction,
    refresh_action: gio::SimpleAction,
    output_action: gio::SimpleAction,
    cancel_action: gio::SimpleAction,
}

enum Message {
    Devices {
        operation_id: u64,
        result: Result<Vec<ScannerDevice>, String>,
    },
    Work {
        operation_id: u64,
        result: Result<WorkResult, String>,
    },
}

struct WorkResult {
    title: String,
    detail: String,
    preview: PathBuf,
    preview_directory: TempDir,
}

pub fn run() {
    let application = adw::Application::builder().application_id(APP_ID).build();
    application.connect_startup(|_| install_theme());
    application.connect_activate(|application| {
        if let Some(window) = application.windows().first() {
            window.present();
        } else {
            build_window(application);
        }
    });
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
}

fn install_custom_theme(ui: &Rc<Ui>) {
    let directory = config_directory();
    if let Err(error) = fs::create_dir_all(&directory) {
        show_error(
            ui,
            &format!(
                "Theme-Ordner konnte nicht angelegt werden ({}): {error}",
                directory.display()
            ),
        );
        return;
    }

    let Some(display) = gtk::gdk::Display::default() else {
        show_error(
            ui,
            "Das benutzerdefinierte Theme konnte nicht installiert werden.",
        );
        return;
    };
    let path = directory.join("theme.css");
    let provider = gtk::CssProvider::new();
    let weak_ui = Rc::downgrade(ui);
    provider.connect_parsing_error(move |_, _, error| {
        if let Some(ui) = weak_ui.upgrade() {
            show_error(&ui, &format!("Fehler in theme.css: {error}"));
        }
    });
    if let Err(error) = load_custom_theme(&provider, &path) {
        show_error(ui, &error.to_string());
    }
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_USER,
    );

    let directory_file = gio::File::for_path(&directory);
    let monitor = match directory_file.monitor_directory(
        gio::FileMonitorFlags::WATCH_MOVES,
        None::<&gio::Cancellable>,
    ) {
        Ok(monitor) => monitor,
        Err(error) => {
            show_error(
                ui,
                &format!("Theme-Änderungen können nicht überwacht werden: {error}"),
            );
            return;
        }
    };

    let weak_ui = Rc::downgrade(ui);
    monitor.connect_changed(move |_, file, other_file, event| {
        let affects_theme = file.path().as_deref() == Some(path.as_path())
            || other_file.and_then(gio::File::path).as_deref() == Some(path.as_path());
        let should_reload = matches!(
            event,
            gio::FileMonitorEvent::ChangesDoneHint
                | gio::FileMonitorEvent::Created
                | gio::FileMonitorEvent::Deleted
                | gio::FileMonitorEvent::Moved
                | gio::FileMonitorEvent::MovedIn
                | gio::FileMonitorEvent::MovedOut
                | gio::FileMonitorEvent::Renamed
        );
        if affects_theme
            && should_reload
            && let Err(error) = load_custom_theme(&provider, &path)
            && let Some(ui) = weak_ui.upgrade()
        {
            show_error(&ui, &error.to_string());
        }
    });
    *ui.theme_monitor.borrow_mut() = Some(monitor);
}

fn load_custom_theme(provider: &gtk::CssProvider, path: &Path) -> Result<()> {
    if path.is_file() {
        let stylesheet = fs::read_to_string(path)
            .with_context(|| format!("Theme konnte nicht gelesen werden: {}", path.display()))?;
        provider.load_from_string(&stylesheet);
    } else {
        provider.load_from_string("");
    }
    Ok(())
}

fn build_window(application: &adw::Application) {
    let (sender, receiver) = async_channel::bounded(8);
    let devices = Rc::new(RefCell::new(Vec::new()));
    let output_directory = Rc::new(RefCell::new(default_output_directory()));
    let busy = Rc::new(Cell::new(false));
    let operation_id = Rc::new(Cell::new(0));
    let cancellation = Rc::new(RefCell::new(None));
    let application_hold = Rc::new(RefCell::new(None));
    let closing = Rc::new(Cell::new(false));
    let preview_directory = Rc::new(RefCell::new(None));
    let theme_monitor = Rc::new(RefCell::new(None));

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
    let device_dropdown = adw::ComboRow::builder()
        .title("Scanner")
        .model(&device_model)
        .build();

    let mode_model =
        gtk::StringList::new(&["Fotos automatisch trennen", "Gesamte Scanfläche speichern"]);
    let mode_dropdown = adw::ComboRow::builder()
        .title("Verarbeitung")
        .model(&mode_model)
        .selected(0)
        .build();

    let dpi_model = gtk::StringList::new(&["300 dpi", "600 dpi", "1200 dpi"]);
    let dpi_dropdown = adw::ComboRow::builder()
        .title("Auflösung")
        .model(&dpi_model)
        .selected(1)
        .build();

    let format_model = gtk::StringList::new(&["JPG", "PNG", "TIFF"]);
    let format_dropdown = adw::ComboRow::builder()
        .title("Bildformat")
        .model(&format_model)
        .selected(0)
        .build();

    let date_entry = adw::EntryRow::builder()
        .title("Aufnahmedatum")
        .text(Local::now().format("%d.%m.%Y").to_string())
        .input_purpose(gtk::InputPurpose::FreeForm)
        .build();
    date_entry.update_property(&[gtk::accessible::Property::Description(
        "Aufnahmedatum im Format Tag Punkt Monat Punkt Jahr",
    )]);

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
    refresh_button.add_css_class("icon-action");
    refresh_button.update_property(&[
        gtk::accessible::Property::Label("Scanner neu suchen"),
        gtk::accessible::Property::Description("SANE- und AirScan-Geräte erneut abfragen"),
        gtk::accessible::Property::KeyShortcuts("Control+R"),
    ]);

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
    let cancel_button = gtk::Button::builder()
        .label("Abbrechen")
        .icon_name("process-stop-symbolic")
        .hexpand(true)
        .visible(false)
        .build();
    cancel_button.add_css_class("destructive-action");
    cancel_button.update_property(&[
        gtk::accessible::Property::Label("Abbrechen"),
        gtk::accessible::Property::KeyShortcuts("Escape"),
    ]);

    let scan_action = gio::SimpleAction::new("scan", None);
    let import_action = gio::SimpleAction::new("import", None);
    let refresh_action = gio::SimpleAction::new("refresh", None);
    let output_action = gio::SimpleAction::new("choose-output", None);
    let cancel_action = gio::SimpleAction::new("cancel", None);
    cancel_action.set_enabled(false);
    scan_button.set_action_name(Some("win.scan"));
    import_button.set_action_name(Some("win.import"));
    cancel_button.set_action_name(Some("win.cancel"));
    refresh_button.set_action_name(Some("win.refresh"));
    output_button.set_action_name(Some("win.choose-output"));
    scan_button.update_property(&[gtk::accessible::Property::KeyShortcuts("F9")]);
    import_button.update_property(&[gtk::accessible::Property::KeyShortcuts("Control+O")]);
    output_button.update_property(&[gtk::accessible::Property::KeyShortcuts("Control+L")]);

    let spinner = gtk::Spinner::new();
    let status_label = gtk::Label::builder()
        .label("Bereit zum Scannen")
        .xalign(0.0)
        .wrap(true)
        .hexpand(true)
        .build();
    status_label.set_accessible_role(gtk::AccessibleRole::Status);
    let (preview_stack, picture) = build_preview();

    let ui = Rc::new(Ui {
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
        cancel_button,
        refresh_button,
        spinner,
        status_label,
        preview_stack,
        picture,
        devices,
        output_directory,
        busy,
        sender,
        operation_id,
        cancellation,
        application_hold,
        closing,
        preview_directory,
        theme_monitor,
        scan_action,
        import_action,
        refresh_action,
        output_action,
        cancel_action,
    });

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
    install_custom_theme(&ui);
    let receiver_ui = Rc::clone(&ui);
    glib::spawn_future_local(async move {
        while let Ok(message) = receiver.recv().await {
            handle_message(&receiver_ui, message);
        }
    });

    let cancellation = Rc::clone(&ui.cancellation);
    let application_hold = Rc::clone(&ui.application_hold);
    let closing = Rc::clone(&ui.closing);
    let sender = ui.sender.clone();
    ui.window.connect_close_request(move |_| {
        closing.set(true);
        if let Some(token) = cancellation.borrow().as_ref() {
            token.cancel();
        } else {
            application_hold.borrow_mut().take();
            sender.close();
        }
        glib::Propagation::Proceed
    });

    request_devices(&ui);
    update_control_states(&ui);
    ui.window.set_default_widget(Some(&ui.scan_button));
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

    let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    content.add_css_class("scanner-sidebar");

    let scanner_group = adw::PreferencesGroup::builder().title("Scan").build();
    scanner_group.add(&ui.device_dropdown);
    let refresh_row = adw::ActionRow::builder()
        .title("Geräte aktualisieren")
        .subtitle("SANE und AirScan erneut abfragen")
        .build();
    refresh_row.add_suffix(&ui.refresh_button);
    refresh_row.set_activatable_widget(Some(&ui.refresh_button));
    scanner_group.add(&refresh_row);
    scanner_group.add(&ui.mode_dropdown);
    scanner_group.add(&ui.dpi_dropdown);
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
    export_group.add(&ui.format_dropdown);
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

    let actions = gtk::Box::new(gtk::Orientation::Vertical, 8);
    actions.append(&ui.scan_button);
    actions.append(&ui.import_button);
    actions.append(&ui.cancel_button);
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
    row.set_activatable_widget(Some(widget));
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
    icon.set_accessible_role(gtk::AccessibleRole::Presentation);
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
        .action_name("win.toggle-sidebar")
        .build();
    sidebar_button.add_css_class("icon-action");
    sidebar_button.update_property(&[
        gtk::accessible::Property::Label("Einstellungen ein- oder ausblenden"),
        gtk::accessible::Property::Description(
            "Öffnet oder schließt die Seitenleiste mit den Scaneinstellungen",
        ),
        gtk::accessible::Property::KeyShortcuts("F10"),
    ]);
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

fn connect_actions(ui: &Rc<Ui>) {
    let weak_ui = Rc::downgrade(ui);
    ui.device_dropdown.connect_selected_notify(move |_| {
        if let Some(ui) = weak_ui.upgrade() {
            update_device_tooltip(&ui);
        }
    });

    let weak_ui = Rc::downgrade(ui);
    ui.auto_threshold.connect_active_notify(move |_| {
        if let Some(ui) = weak_ui.upgrade() {
            update_control_states(&ui);
        }
    });

    let weak_ui = Rc::downgrade(ui);
    ui.mode_dropdown.connect_selected_notify(move |_| {
        if let Some(ui) = weak_ui.upgrade() {
            update_control_states(&ui);
        }
    });

    let weak_ui = Rc::downgrade(ui);
    ui.format_dropdown.connect_selected_notify(move |_| {
        if let Some(ui) = weak_ui.upgrade() {
            update_control_states(&ui);
        }
    });

    let weak_ui = Rc::downgrade(ui);
    ui.output_action.connect_activate(move |_, _| {
        if let Some(ui) = weak_ui.upgrade() {
            choose_output_directory(&ui);
        }
    });
    let weak_ui = Rc::downgrade(ui);
    ui.refresh_action.connect_activate(move |_, _| {
        if let Some(ui) = weak_ui.upgrade() {
            request_devices(&ui);
        }
    });
    let weak_ui = Rc::downgrade(ui);
    ui.scan_action.connect_activate(move |_, _| {
        if let Some(ui) = weak_ui.upgrade() {
            start_scan(&ui);
        }
    });
    let weak_ui = Rc::downgrade(ui);
    ui.import_action.connect_activate(move |_, _| {
        if let Some(ui) = weak_ui.upgrade() {
            choose_import_file(&ui);
        }
    });
    let weak_ui = Rc::downgrade(ui);
    ui.cancel_action.connect_activate(move |_, _| {
        if let Some(ui) = weak_ui.upgrade() {
            cancel_current_work(&ui);
        }
    });

    let toggle_sidebar = gio::SimpleAction::new("toggle-sidebar", None);
    let weak_ui = Rc::downgrade(ui);
    toggle_sidebar.connect_activate(move |_, _| {
        if let Some(ui) = weak_ui.upgrade() {
            ui.split_view
                .set_show_sidebar(!ui.split_view.shows_sidebar());
        }
    });

    let close = gio::SimpleAction::new("close", None);
    let weak_ui = Rc::downgrade(ui);
    close.connect_activate(move |_, _| {
        if let Some(ui) = weak_ui.upgrade() {
            ui.window.close();
        }
    });

    ui.window.add_action(&ui.scan_action);
    ui.window.add_action(&ui.import_action);
    ui.window.add_action(&ui.refresh_action);
    ui.window.add_action(&ui.output_action);
    ui.window.add_action(&ui.cancel_action);
    ui.window.add_action(&toggle_sidebar);
    ui.window.add_action(&close);

    if let Some(application) = ui.window.application() {
        application.set_accels_for_action("win.scan", &["F9"]);
        application.set_accels_for_action("win.import", &["<Primary>o"]);
        application.set_accels_for_action("win.refresh", &["<Primary>r"]);
        application.set_accels_for_action("win.choose-output", &["<Primary>l"]);
        application.set_accels_for_action("win.toggle-sidebar", &["F10"]);
        application.set_accels_for_action("win.cancel", &["Escape"]);
        application.set_accels_for_action("win.close", &["<Primary>q"]);
    }
}

fn update_control_states(ui: &Ui) {
    let idle = !ui.busy.get();
    let splitting = idle && ui.mode_dropdown.selected() == 0;
    ui.auto_threshold.set_sensitive(splitting);
    ui.threshold
        .set_sensitive(splitting && !ui.auto_threshold.is_active());
    ui.min_area.set_sensitive(splitting);
    ui.padding.set_sensitive(splitting);
    ui.quality
        .set_sensitive(idle && ui.format_dropdown.selected() == 0);
}

fn update_device_tooltip(ui: &Ui) {
    let selected = ui.device_dropdown.selected() as usize;
    let tooltip = ui.devices.borrow().get(selected).map(ScannerDevice::label);
    ui.device_dropdown.set_tooltip_text(tooltip.as_deref());
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

fn choose_import_file(ui: &Rc<Ui>) {
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
    let import_ui = Rc::clone(ui);
    dialog.open(Some(&ui.window), None::<&gio::Cancellable>, move |result| {
        if let Ok(file) = result
            && let Some(path) = file.path()
        {
            start_import(&import_ui, path);
        }
    });
}

fn collect_config(ui: &Ui, scanned: bool) -> Result<SplitConfig> {
    let capture_date = match NaiveDate::parse_from_str(ui.date_entry.text().trim(), "%d.%m.%Y") {
        Ok(date) => {
            ui.date_entry
                .update_state(&[gtk::accessible::State::Invalid(
                    gtk::AccessibleInvalidState::False,
                )]);
            date
        }
        Err(error) => {
            ui.date_entry
                .update_state(&[gtk::accessible::State::Invalid(
                    gtk::AccessibleInvalidState::True,
                )]);
            ui.date_entry.grab_focus();
            return Err(error).context("Aufnahmedatum muss als TT.MM.JJJJ angegeben werden");
        }
    };
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
    let (operation_id, cancellation) = begin_operation(ui);
    set_busy(ui, true, &format!("Scanne mit {dpi} dpi …"));
    thread::spawn(move || {
        let result = run_worker(|| {
            scan_work(&device, dpi, &output, &config, full_scan, &cancellation).map_err(|error| {
                if cancellation.is_cancelled() {
                    CANCELLED_MESSAGE.to_string()
                } else {
                    format!("{error:#}")
                }
            })
        });
        let _ = sender.send_blocking(Message::Work {
            operation_id,
            result,
        });
    });
}

fn scan_work(
    device: &ScannerDevice,
    dpi: u32,
    output: &Path,
    config: &SplitConfig,
    full_scan: bool,
    cancellation: &ScannerCancellation,
) -> Result<WorkResult> {
    ensure_not_cancelled(cancellation)?;
    let temporary = TempDir::with_prefix("photoscanner-")?;
    let source = temporary.path().join("scan.png");
    scan_to_file_cancellable(
        &source,
        Some(&device.name),
        dpi,
        Duration::from_secs(600),
        cancellation,
    )?;
    ensure_not_cancelled(cancellation)?;
    if full_scan {
        let path = save_full_scan(&source, output, config, None)?;
        ensure_not_cancelled(cancellation)?;
        let (preview, preview_directory) = bounded_preview(&path)?;
        return Ok(WorkResult {
            title: "Vollständiger Scan gespeichert".to_string(),
            detail: path.display().to_string(),
            preview,
            preview_directory,
        });
    }
    let result = split_scan(&source, output, config, None, true)?;
    ensure_not_cancelled(cancellation)?;
    let preview_source = result
        .preview
        .clone()
        .or_else(|| result.files.first().cloned())
        .ok_or_else(|| anyhow!("Keine Vorschaudatei erzeugt"))?;
    let (preview, preview_directory) = bounded_preview(&preview_source)?;
    Ok(WorkResult {
        title: format!("{} Foto(s) gespeichert", result.files.len()),
        detail: format!(
            "{} · Schwellwert {:.1}",
            output.display(),
            result.threshold_used
        ),
        preview,
        preview_directory,
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
    let full_scan = ui.mode_dropdown.selected() == 1;
    let output = ui.output_directory.borrow().clone();
    let sender = ui.sender.clone();
    let (operation_id, cancellation) = begin_operation(ui);
    set_busy(ui, true, "Analysiere Scandatei …");
    thread::spawn(move || {
        let result = run_worker(|| {
            (|| -> Result<WorkResult> {
                ensure_not_cancelled(&cancellation)?;
                if !source.is_file() {
                    bail!("Die Scandatei existiert nicht mehr: {}", source.display());
                }
                if full_scan {
                    let path = save_full_scan(&source, &output, &config, None)?;
                    ensure_not_cancelled(&cancellation)?;
                    let (preview, preview_directory) = bounded_preview(&path)?;
                    return Ok(WorkResult {
                        title: "Vollständige Scandatei gespeichert".to_string(),
                        detail: path.display().to_string(),
                        preview,
                        preview_directory,
                    });
                }
                let result = split_scan(&source, &output, &config, None, true)?;
                ensure_not_cancelled(&cancellation)?;
                let preview_source = result
                    .preview
                    .clone()
                    .or_else(|| result.files.first().cloned())
                    .ok_or_else(|| anyhow!("Keine Vorschaudatei erzeugt"))?;
                let (preview, preview_directory) = bounded_preview(&preview_source)?;
                Ok(WorkResult {
                    title: format!("{} Foto(s) aus Datei gespeichert", result.files.len()),
                    detail: format!(
                        "{} · Schwellwert {:.1}",
                        output.display(),
                        result.threshold_used
                    ),
                    preview,
                    preview_directory,
                })
            })()
            .map_err(|error| {
                if cancellation.is_cancelled() {
                    CANCELLED_MESSAGE.to_string()
                } else {
                    format!("{error:#}")
                }
            })
        });
        let _ = sender.send_blocking(Message::Work {
            operation_id,
            result,
        });
    });
}

fn request_devices(ui: &Ui) {
    if ui.busy.get() {
        return;
    }
    let (operation_id, cancellation) = begin_operation(ui);
    ui.refresh_action.set_enabled(false);
    ui.scan_action.set_enabled(false);
    ui.cancel_action.set_enabled(true);
    ui.cancel_button.set_visible(true);
    set_status(
        ui,
        "Suche Scanner …",
        gtk::AccessibleAnnouncementPriority::Low,
    );
    ui.spinner.start();
    let sender = ui.sender.clone();
    thread::spawn(move || {
        let result = run_worker(|| {
            list_devices_cancellable(DEVICE_DISCOVERY_TIMEOUT, &cancellation).map_err(|error| {
                if cancellation.is_cancelled() {
                    CANCELLED_MESSAGE.to_string()
                } else {
                    error.to_string()
                }
            })
        });
        let _ = sender.send_blocking(Message::Devices {
            operation_id,
            result,
        });
    });
}

fn handle_message(ui: &Ui, message: Message) {
    let message_operation_id = match &message {
        Message::Devices { operation_id, .. } | Message::Work { operation_id, .. } => *operation_id,
    };
    if message_operation_id != ui.operation_id.get() {
        return;
    }
    if matches!(message, Message::Devices { .. }) {
        ui.cancel_action.set_enabled(false);
        ui.cancel_button.set_visible(false);
    }
    ui.cancellation.borrow_mut().take();
    ui.application_hold.borrow_mut().take();
    if ui.closing.get() {
        ui.sender.close();
        return;
    }

    match message {
        Message::Devices {
            result: Ok(devices),
            ..
        } => {
            while ui.device_model.n_items() > 0 {
                ui.device_model.remove(0);
            }
            for device in &devices {
                ui.device_model.append(&device.display_name());
            }
            *ui.devices.borrow_mut() = devices;
            ui.device_dropdown.set_selected(0);
            update_device_tooltip(ui);
            ui.refresh_action.set_enabled(true);
            ui.spinner.stop();
            if ui.devices.borrow().is_empty() {
                ui.device_model.append("Kein Scanner erkannt");
                ui.scan_action.set_enabled(false);
                set_status(
                    ui,
                    "Kein Scanner erkannt",
                    gtk::AccessibleAnnouncementPriority::Medium,
                );
                ui.import_button.grab_focus();
            } else {
                ui.scan_action.set_enabled(true);
                set_status(
                    ui,
                    "Scanner bereit",
                    gtk::AccessibleAnnouncementPriority::Low,
                );
                ui.device_dropdown.grab_focus();
            }
        }
        Message::Devices {
            result: Err(error), ..
        } => {
            ui.refresh_action.set_enabled(true);
            ui.scan_action.set_enabled(!ui.devices.borrow().is_empty());
            ui.spinner.stop();
            if error != CANCELLED_MESSAGE {
                show_error(ui, &error);
                ui.refresh_button.grab_focus();
            }
        }
        Message::Work {
            result: Ok(result), ..
        } => {
            set_busy(ui, false, &result.title);
            set_status(
                ui,
                &format!("{}\n{}", result.title, result.detail),
                gtk::AccessibleAnnouncementPriority::Medium,
            );
            ui.picture
                .set_file(Some(&gio::File::for_path(&result.preview)));
            ui.picture.update_property(&[
                gtk::accessible::Property::Label("Scanvorschau"),
                gtk::accessible::Property::Description(&result.title),
            ]);
            *ui.preview_directory.borrow_mut() = Some(result.preview_directory);
            ui.preview_stack.set_visible_child_name("picture");
            ui.toast_overlay.add_toast(adw::Toast::new(&result.title));
        }
        Message::Work {
            result: Err(error), ..
        } => {
            if error.starts_with(CANCELLED_MESSAGE) {
                set_busy(ui, false, "Vorgang abgebrochen");
            } else {
                set_busy(ui, false, "Vorgang fehlgeschlagen");
                show_error(ui, &error);
            }
        }
    }
}

fn set_busy(ui: &Ui, busy: bool, status: &str) {
    ui.busy.set(busy);
    ui.scan_action
        .set_enabled(!busy && !ui.devices.borrow().is_empty());
    ui.import_action.set_enabled(!busy);
    ui.refresh_action.set_enabled(!busy);
    ui.output_action.set_enabled(!busy);
    ui.cancel_action.set_enabled(busy);
    ui.cancel_button.set_visible(busy);
    ui.device_dropdown.set_sensitive(!busy);
    ui.mode_dropdown.set_sensitive(!busy);
    ui.dpi_dropdown.set_sensitive(!busy);
    ui.format_dropdown.set_sensitive(!busy);
    ui.date_entry.set_sensitive(!busy);
    update_control_states(ui);
    ui.preview_stack
        .update_state(&[gtk::accessible::State::Busy(busy)]);
    set_status(
        ui,
        status,
        if busy {
            gtk::AccessibleAnnouncementPriority::Low
        } else {
            gtk::AccessibleAnnouncementPriority::Medium
        },
    );
    if busy {
        ui.spinner.start();
    } else {
        ui.spinner.stop();
    }
}

fn show_error(ui: &Ui, message: &str) {
    set_status(ui, message, gtk::AccessibleAnnouncementPriority::High);
    ui.toast_overlay
        .add_toast(adw::Toast::builder().title(message).timeout(6).build());
}

fn set_status(ui: &Ui, message: &str, priority: gtk::AccessibleAnnouncementPriority) {
    ui.status_label.set_label(message);
    ui.status_label.announce(message, priority);
}

fn begin_operation(ui: &Ui) -> (u64, ScannerCancellation) {
    if let Some(previous) = ui.cancellation.borrow_mut().take() {
        previous.cancel();
    }
    ui.application_hold.borrow_mut().take();
    let operation_id = ui.operation_id.get().wrapping_add(1);
    ui.operation_id.set(operation_id);
    let cancellation = ScannerCancellation::new();
    *ui.cancellation.borrow_mut() = Some(cancellation.clone());
    *ui.application_hold.borrow_mut() = ui
        .window
        .application()
        .map(|application| application.hold());
    (operation_id, cancellation)
}

fn cancel_current_work(ui: &Ui) {
    if let Some(cancellation) = ui.cancellation.borrow().as_ref() {
        cancellation.cancel();
        ui.cancel_action.set_enabled(false);
        set_status(
            ui,
            "Abbruch angefordert …",
            gtk::AccessibleAnnouncementPriority::Medium,
        );
    }
}

fn ensure_not_cancelled(cancellation: &ScannerCancellation) -> Result<()> {
    if cancellation.is_cancelled() {
        bail!(CANCELLED_MESSAGE);
    }
    Ok(())
}

fn run_worker<T>(work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|_| Err(WORKER_PANIC_MESSAGE.to_string()))
}

fn bounded_preview(source: &Path) -> Result<(PathBuf, TempDir)> {
    let image = imgcodecs::imread(source, imgcodecs::IMREAD_COLOR)
        .with_context(|| format!("Vorschau konnte nicht gelesen werden: {}", source.display()))?;
    if image.empty() {
        bail!("Vorschau enthält keine Bilddaten: {}", source.display());
    }

    let largest_edge = image.cols().max(image.rows());
    let scale = (PREVIEW_MAX_EDGE as f64 / largest_edge as f64).min(1.0);
    let mut preview = Mat::default();
    if scale < 1.0 {
        let target = Size::new(
            (image.cols() as f64 * scale).round().max(1.0) as i32,
            (image.rows() as f64 * scale).round().max(1.0) as i32,
        );
        imgproc::resize(&image, &mut preview, target, 0.0, 0.0, imgproc::INTER_AREA)?;
    } else {
        preview = image;
    }

    let directory = TempDir::with_prefix("photoscanner-preview-")?;
    let path = directory.path().join("preview.jpg");
    let parameters = Vector::from_slice(&[imgcodecs::IMWRITE_JPEG_QUALITY, 86]);
    if !imgcodecs::imwrite(&path, &preview, &parameters)? {
        bail!("Vorschaudatei konnte nicht gespeichert werden");
    }
    Ok((path, directory))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencv::core::{CV_8UC3, Scalar};

    #[test]
    fn bounded_preview_limits_the_largest_edge() {
        let source_directory = TempDir::new().unwrap();
        let source = source_directory.path().join("large.png");
        let image = Mat::new_rows_cols_with_default(
            1200,
            2400,
            CV_8UC3,
            Scalar::new(40.0, 80.0, 120.0, 0.0),
        )
        .unwrap();
        assert!(imgcodecs::imwrite_def(&source, &image).unwrap());

        let (preview, owner) = bounded_preview(&source).unwrap();
        assert!(preview.starts_with(owner.path()));
        let image = imgcodecs::imread(&preview, imgcodecs::IMREAD_COLOR).unwrap();
        assert_eq!(image.cols().max(image.rows()), PREVIEW_MAX_EDGE);
        assert_eq!((image.cols(), image.rows()), (1600, 800));
    }
}
