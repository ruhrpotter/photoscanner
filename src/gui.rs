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
use chrono::{Datelike, Local, NaiveDate};
use gdk_pixbuf::{Pixbuf, PixbufRotation};
use gtk::gio;
use gtk::glib;
use opencv::core::{self, Size, Vector};
use opencv::imgcodecs;
use opencv::imgproc;
use opencv::prelude::*;
use photoscanner::i18n::{tr, tr_args, trn};
use photoscanner::scanner::{
    DEVICE_DISCOVERY_TIMEOUT, ScannerCancellation, ScannerDevice, list_devices_cancellable,
    scan_to_file_with_progress,
};
use photoscanner::splitter::{
    AnalyzedScan, DetectedRegion, OutputFormat, SplitConfig, analyze_scan, export_photos,
    save_detection_preview, save_full_scan, split_scan, warp_detected_photo,
};
use photoscanner::{APP_ID, APP_NAME};
use tempfile::TempDir;

use crate::gui_settings::PersistedSettings;

const DEFAULT_STYLE: &str = include_str!("style.css");
const PREVIEW_MAX_EDGE: i32 = 3200;

fn cancelled_message() -> String {
    tr("Operation cancelled.")
}

fn worker_panic_message() -> String {
    tr("Internal background worker error.")
}

struct Ui {
    window: adw::ApplicationWindow,
    toast_overlay: adw::ToastOverlay,
    split_view: adw::OverlaySplitView,
    device_model: gtk::StringList,
    device_dropdown: adw::ComboRow,
    mode_dropdown: adw::ComboRow,
    review_before_save: gtk::Switch,
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
    progress_bar: gtk::ProgressBar,
    status_label: gtk::Label,
    preview_stack: gtk::Stack,
    picture: gtk::Picture,
    preview_scroller: gtk::ScrolledWindow,
    review_overview: gtk::Picture,
    review_detected_label: gtk::Label,
    review_selection_label: gtk::Label,
    review_flow: gtk::FlowBox,
    review_save_button: gtk::Button,
    zoom: Rc<Cell<f64>>,
    review_zoom: Cell<f64>,
    review_images: RefCell<Vec<ReviewImage>>,
    devices: Rc<RefCell<Vec<ScannerDevice>>>,
    output_directory: Rc<RefCell<PathBuf>>,
    busy: Rc<Cell<bool>>,
    discovery_pending: Rc<Cell<bool>>,
    sender: Sender<Message>,
    operation_id: Rc<Cell<u64>>,
    cancellation: Rc<RefCell<Option<ScannerCancellation>>>,
    application_hold: Rc<RefCell<Option<gio::ApplicationHoldGuard>>>,
    closing: Rc<Cell<bool>>,
    settings_path: PathBuf,
    last_capture_date: Rc<Cell<Option<NaiveDate>>>,
    last_error: Rc<RefCell<Option<String>>>,
    review_state: Rc<RefCell<Option<StoredReview>>>,
    preview_directory: Rc<RefCell<Option<TempDir>>>,
    theme_monitor: Rc<RefCell<Option<gio::FileMonitor>>>,
    scan_action: gio::SimpleAction,
    import_action: gio::SimpleAction,
    refresh_action: gio::SimpleAction,
    output_action: gio::SimpleAction,
    cancel_action: gio::SimpleAction,
    zoom_in_action: gio::SimpleAction,
    zoom_out_action: gio::SimpleAction,
    zoom_fit_action: gio::SimpleAction,
    save_review_action: gio::SimpleAction,
    discard_review_action: gio::SimpleAction,
}

enum Message {
    Progress {
        operation_id: u64,
        percent: f64,
    },
    Status {
        operation_id: u64,
        text: String,
    },
    ScanComplete {
        operation_id: u64,
        text: String,
    },
    ReviewReady {
        operation_id: u64,
        result: Result<ReviewData, String>,
    },
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
    capture_date: NaiveDate,
}

enum ScanOutcome {
    Work(WorkResult),
    Review(ReviewData),
}

#[derive(Clone, Copy)]
enum ScanMode {
    Direct,
    Review,
    Full,
}

fn status_after_scan(mode: ScanMode) -> String {
    match mode {
        ScanMode::Full => tr("Saving scan…"),
        ScanMode::Direct | ScanMode::Review => tr("Analyzing scan file…"),
    }
}

struct ScanCallbacks {
    progress: Option<Box<dyn Fn(f64) + Send>>,
    complete: Option<Box<dyn Fn() + Send>>,
}

struct ReviewPhotoData {
    full_path: PathBuf,
    thumbnail_path: PathBuf,
    group_index: usize,
    region: DetectedRegion,
}

struct ReviewGroup {
    analyzed: AnalyzedScan,
}

struct ReviewData {
    staging: TempDir,
    photos: Vec<ReviewPhotoData>,
    groups: Vec<ReviewGroup>,
    overview: PathBuf,
    config: SplitConfig,
    output: PathBuf,
    failures: Vec<String>,
}

struct ReviewSelection {
    include: Rc<Cell<bool>>,
    quarter_turns: Rc<Cell<u8>>,
}

struct StoredReview {
    data: ReviewData,
    selections: Vec<ReviewSelection>,
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
            &tr_args(
                "Could not create the theme directory ({directory}): {error}",
                &[
                    ("directory", directory.display().to_string()),
                    ("error", error.to_string()),
                ],
            ),
        );
        return;
    }

    let Some(display) = gtk::gdk::Display::default() else {
        show_error(ui, &tr("The custom theme could not be installed."));
        return;
    };
    let path = directory.join("theme.css");
    let provider = gtk::CssProvider::new();
    let weak_ui = Rc::downgrade(ui);
    provider.connect_parsing_error(move |_, _, error| {
        if let Some(ui) = weak_ui.upgrade() {
            show_error(
                &ui,
                &tr_args(
                    "Error in theme.css: {error}",
                    &[("error", error.to_string())],
                ),
            );
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
                &tr_args(
                    "Theme changes cannot be monitored: {error}",
                    &[("error", error.to_string())],
                ),
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
        let stylesheet = fs::read_to_string(path).with_context(|| {
            tr_args(
                "Could not read the theme: {path}",
                &[("path", path.display().to_string())],
            )
        })?;
        provider.load_from_string(&stylesheet);
    } else {
        provider.load_from_string("");
    }
    Ok(())
}

fn build_window(application: &adw::Application) {
    let ui = build_ui(application);
    request_devices(&ui);
    ui.window.present();
}

fn build_ui(application: &adw::Application) -> Rc<Ui> {
    let (sender, receiver) = async_channel::bounded(8);
    let settings_path = config_directory().join("settings.ini");
    let persisted_settings = PersistedSettings::load(&settings_path);
    let devices = Rc::new(RefCell::new(Vec::new()));
    let output_directory = Rc::new(RefCell::new(persisted_settings.output_directory.clone()));
    let busy = Rc::new(Cell::new(false));
    let discovery_pending = Rc::new(Cell::new(false));
    let operation_id = Rc::new(Cell::new(0));
    let cancellation = Rc::new(RefCell::new(None));
    let application_hold = Rc::new(RefCell::new(None));
    let closing = Rc::new(Cell::new(false));
    let last_capture_date = Rc::new(Cell::new(persisted_settings.capture_date));
    let last_error = Rc::new(RefCell::new(None));
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
        .max_sidebar_width(380.0)
        .sidebar_width_fraction(0.29)
        .enable_hide_gesture(true)
        .enable_show_gesture(true)
        .build();
    split_view.add_css_class("app-shell");

    let device_model = gtk::StringList::new(&[&tr("Searching for scanners…")]);
    let device_dropdown = adw::ComboRow::builder()
        .title(tr("Scanner"))
        .model(&device_model)
        .build();

    let mode_model = gtk::StringList::new(&[
        &tr("Automatically separate photos"),
        &tr("Save the entire scan area"),
    ]);
    let mode_dropdown = adw::ComboRow::builder()
        .title(tr("Processing"))
        .use_subtitle(true)
        .expression(gtk::PropertyExpression::new(
            gtk::StringObject::static_type(),
            None::<gtk::Expression>,
            "string",
        ))
        .model(&mode_model)
        .selected(persisted_settings.mode_index)
        .build();
    let review_before_save = gtk::Switch::builder()
        .active(persisted_settings.review_before_save)
        .valign(gtk::Align::Center)
        .build();

    let dpi_model = gtk::StringList::new(&["300 dpi", "600 dpi", "1200 dpi"]);
    let dpi_dropdown = adw::ComboRow::builder()
        .title(tr("Resolution"))
        .model(&dpi_model)
        .selected(persisted_settings.dpi_index)
        .build();

    let format_model = gtk::StringList::new(&["JPG", "PNG", "TIFF"]);
    let format_dropdown = adw::ComboRow::builder()
        .title(tr("Image format"))
        .model(&format_model)
        .selected(persisted_settings.format_index)
        .build();

    let date_entry = adw::EntryRow::builder()
        .title(tr("Capture date"))
        .text(
            persisted_settings
                .capture_date
                .unwrap_or_else(|| Local::now().date_naive())
                .format("%d.%m.%Y")
                .to_string(),
        )
        .input_purpose(gtk::InputPurpose::FreeForm)
        .build();
    date_entry.update_property(&[gtk::accessible::Property::Description(&tr(
        "Capture date in day dot month dot year format",
    ))]);
    let calendar = gtk::Calendar::new();
    let calendar_popover = gtk::Popover::builder().child(&calendar).build();
    let calendar_button = gtk::MenuButton::builder()
        .icon_name("x-office-calendar-symbolic")
        .popover(&calendar_popover)
        .valign(gtk::Align::Center)
        .build();
    calendar_button.add_css_class("flat");
    calendar_button.update_property(&[gtk::accessible::Property::Label(&tr("Open calendar"))]);
    date_entry.add_suffix(&calendar_button);
    connect_date_picker(&date_entry, &calendar, &calendar_popover);

    let auto_threshold = gtk::Switch::builder()
        .active(persisted_settings.auto_threshold)
        .valign(gtk::Align::Center)
        .build();
    let threshold = gtk::SpinButton::with_range(1.0, 255.0, 1.0);
    threshold.set_value(persisted_settings.threshold);
    threshold.set_sensitive(false);
    threshold.set_valign(gtk::Align::Center);
    let min_area = gtk::SpinButton::with_range(0.1, 50.0, 0.1);
    min_area.set_value(persisted_settings.min_area);
    min_area.set_digits(1);
    let padding = gtk::SpinButton::with_range(0.0, 15.0, 0.1);
    padding.set_value(persisted_settings.padding);
    padding.set_digits(1);
    let quality = gtk::SpinButton::with_range(1.0, 100.0, 1.0);
    quality.set_value(persisted_settings.quality);

    let output_button = gtk::Button::builder()
        .label(short_path(&output_directory.borrow()))
        .tooltip_text(output_directory.borrow().display().to_string())
        .valign(gtk::Align::Center)
        .build();
    if let Some(label) = output_button.child().and_downcast::<gtk::Label>() {
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        label.set_max_width_chars(16);
    }
    let refresh_button = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text(tr("Search for scanners again"))
        .valign(gtk::Align::Center)
        .build();
    refresh_button.add_css_class("flat");
    refresh_button.add_css_class("icon-action");
    refresh_button.update_property(&[
        gtk::accessible::Property::Label(&tr("Search for scanners again")),
        gtk::accessible::Property::Description(&tr("Query SANE and AirScan devices again")),
        gtk::accessible::Property::KeyShortcuts("Control+R"),
    ]);

    let scan_button_content = adw::ButtonContent::builder()
        .label(tr("Start scan"))
        .icon_name("document-send-symbolic")
        .can_shrink(true)
        .build();
    let scan_button = gtk::Button::builder()
        .child(&scan_button_content)
        .hexpand(true)
        .build();
    scan_button.add_css_class("suggested-action");
    scan_button.add_css_class("primary-action");
    let import_button_content = adw::ButtonContent::builder()
        .label(tr("Open scan file"))
        .icon_name("folder-open-symbolic")
        .can_shrink(true)
        .build();
    let import_button = gtk::Button::builder()
        .child(&import_button_content)
        .hexpand(true)
        .build();
    import_button.add_css_class("secondary-action");
    let cancel_button_content = adw::ButtonContent::builder()
        .label(tr("Cancel"))
        .icon_name("process-stop-symbolic")
        .can_shrink(true)
        .build();
    let cancel_button = gtk::Button::builder()
        .child(&cancel_button_content)
        .hexpand(true)
        .visible(false)
        .build();
    cancel_button.add_css_class("secondary-action");
    cancel_button.update_property(&[
        gtk::accessible::Property::Label(&tr("Cancel")),
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
    spinner
        .bind_property("spinning", &spinner, "visible")
        .sync_create()
        .build();
    let progress_bar = gtk::ProgressBar::builder()
        .visible(false)
        .width_request(120)
        .valign(gtk::Align::Center)
        .build();
    let status_label = gtk::Label::builder()
        .label(tr("Ready to scan"))
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(48)
        .hexpand(true)
        .build();
    status_label.set_accessible_role(gtk::AccessibleRole::Status);
    let (
        preview_stack,
        picture,
        preview_scroller,
        review_overview,
        review_detected_label,
        review_selection_label,
        review_flow,
        review_save_button,
    ) = build_preview();
    let zoom = Rc::new(Cell::new(0.0));
    let review_state = Rc::new(RefCell::new(None));

    let zoom_in_action = gio::SimpleAction::new("zoom-in", None);
    let zoom_out_action = gio::SimpleAction::new("zoom-out", None);
    let zoom_fit_action = gio::SimpleAction::new("zoom-fit", None);
    let save_review_action = gio::SimpleAction::new("save-review", None);
    let discard_review_action = gio::SimpleAction::new("discard-review", None);
    zoom_in_action.set_enabled(false);
    zoom_out_action.set_enabled(false);
    zoom_fit_action.set_enabled(false);
    save_review_action.set_enabled(false);
    discard_review_action.set_enabled(false);

    let ui = Rc::new(Ui {
        window: window.clone(),
        toast_overlay: adw::ToastOverlay::new(),
        split_view: split_view.clone(),
        device_model,
        device_dropdown,
        mode_dropdown,
        review_before_save,
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
        progress_bar,
        status_label,
        preview_stack,
        picture,
        preview_scroller,
        review_overview,
        review_detected_label,
        review_selection_label,
        review_flow,
        review_save_button,
        zoom,
        review_zoom: Cell::new(1.0),
        review_images: RefCell::new(Vec::new()),
        devices,
        output_directory,
        busy,
        discovery_pending,
        sender,
        operation_id,
        cancellation,
        application_hold,
        closing,
        settings_path,
        last_capture_date,
        last_error,
        review_state,
        preview_directory,
        theme_monitor,
        scan_action,
        import_action,
        refresh_action,
        output_action,
        cancel_action,
        zoom_in_action,
        zoom_out_action,
        zoom_fit_action,
        save_review_action,
        discard_review_action,
    });

    split_view.set_sidebar(Some(&build_sidebar(&ui)));
    split_view.set_content(Some(&build_preview_pane(&ui)));
    split_view.set_vexpand(true);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&split_view);
    root.append(&build_action_bar(&ui));
    ui.toast_overlay.set_child(Some(&root));
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

    let weak_ui = Rc::downgrade(&ui);
    ui.window.connect_close_request(move |_| {
        if let Some(ui) = weak_ui.upgrade() {
            save_settings(&ui);
            ui.closing.set(true);
            if let Some(token) = ui.cancellation.borrow().as_ref() {
                token.cancel();
            } else {
                ui.application_hold.borrow_mut().take();
                ui.sender.close();
            }
        }
        glib::Propagation::Proceed
    });

    update_control_states(&ui);
    ui.window.set_default_widget(Some(&ui.scan_button));
    ui
}

fn short_path(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| tr("Output folder"))
}

fn build_sidebar(ui: &Ui) -> adw::ToolbarView {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_css_class("scanner-sidebar");
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        APP_NAME,
        &tr("Digitize paper photos"),
    )));
    toolbar.add_top_bar(&header);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 24);
    content.set_margin_top(12);
    content.set_margin_bottom(24);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let scanner_group = adw::PreferencesGroup::builder().title(tr("Scan")).build();
    scanner_group.set_header_suffix(Some(&ui.refresh_button));
    scanner_group.add(&ui.device_dropdown);
    scanner_group.add(&ui.mode_dropdown);
    let review_row = adw::ActionRow::builder()
        .title(tr("Review before saving"))
        .subtitle(tr("Select and rotate detected photos"))
        .build();
    review_row.add_suffix(&ui.review_before_save);
    review_row.set_activatable_widget(Some(&ui.review_before_save));
    scanner_group.add(&review_row);
    scanner_group.add(&ui.dpi_dropdown);
    scanner_group.add(&ui.date_entry);
    content.append(&scanner_group);

    let export_group = adw::PreferencesGroup::builder().title(tr("Output")).build();
    let output_row = adw::ActionRow::builder()
        .title(tr("Folder"))
        .subtitle(tr("Photos and detection preview"))
        .build();
    output_row.add_suffix(&ui.output_button);
    output_row.set_activatable_widget(Some(&ui.output_button));
    export_group.add(&output_row);
    export_group.add(&ui.format_dropdown);
    export_group.add(&row_with_suffix(
        &tr("JPEG quality"),
        Some(&tr("Only for exported JPG photos")),
        &ui.quality,
    ));
    content.append(&export_group);

    let detection_group = adw::PreferencesGroup::new();
    let detection = adw::ExpanderRow::builder()
        .title(tr("Detection"))
        .subtitle(tr(
            "Automatic detection is optimized for light scanner beds.",
        ))
        .subtitle_lines(2)
        .build();
    let auto_row = adw::ActionRow::builder()
        .title(tr("Automatic threshold"))
        .subtitle(tr("Evaluate background noise along the edges"))
        .build();
    auto_row.add_suffix(&ui.auto_threshold);
    auto_row.set_activatable_widget(Some(&ui.auto_threshold));
    detection.add_row(&auto_row);
    detection.add_row(&row_with_suffix(
        &tr("Manual threshold"),
        None,
        &ui.threshold,
    ));
    detection.add_row(&row_with_suffix(
        &tr("Minimum area (%)"),
        Some(&tr("Ignore small dust and edge areas")),
        &ui.min_area,
    ));
    detection.add_row(&row_with_suffix(
        &tr("Additional margin (%)"),
        Some(&tr("Keep some space around each photo")),
        &ui.padding,
    ));
    detection_group.add(&detection);
    content.append(&detection_group);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&content)
        .build();
    toolbar.set_content(Some(&scroll));
    toolbar
}

fn build_action_bar(ui: &Ui) -> adw::WrapBox {
    let status = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    status.set_valign(gtk::Align::Center);
    status.set_hexpand(true);
    status.add_css_class("action-bar-status");
    status.append(&ui.spinner);
    status.append(&ui.status_label);
    status.append(&ui.progress_bar);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_homogeneous(true);
    actions.set_halign(gtk::Align::End);
    actions.add_css_class("action-bar-actions");
    actions.append(&ui.import_button);
    actions.append(&ui.scan_button);
    actions.append(&ui.cancel_button);

    let bar = adw::WrapBox::builder()
        .orientation(gtk::Orientation::Horizontal)
        .child_spacing(12)
        .line_spacing(8)
        .natural_line_length(820)
        .wrap_policy(adw::WrapPolicy::Natural)
        .build();
    bar.add_css_class("action-bar");
    bar.append(&status);
    bar.append(&actions);
    bar
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

fn build_preview() -> (
    gtk::Stack,
    gtk::Picture,
    gtk::ScrolledWindow,
    gtk::Picture,
    gtk::Label,
    gtk::Label,
    gtk::FlowBox,
    gtk::Button,
) {
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&picture)
        .build();

    let review_overview = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    review_overview.add_css_class("review-overview-image");
    review_overview.update_property(&[
        gtk::accessible::Property::Label(&tr("Scan overview")),
        gtk::accessible::Property::Description(&tr(
            "Green outlines show the detected areas in the original scan.",
        )),
    ]);

    let overview_frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    overview_frame.set_overflow(gtk::Overflow::Hidden);
    overview_frame.add_css_class("review-overview-frame");
    overview_frame.set_valign(gtk::Align::Center);
    let overview_height = adw::Clamp::builder()
        .orientation(gtk::Orientation::Vertical)
        .maximum_size(104)
        .tightening_threshold(104)
        .child(&review_overview)
        .build();
    let overview_size = adw::Clamp::builder()
        .maximum_size(128)
        .tightening_threshold(128)
        .child(&overview_height)
        .build();
    overview_frame.append(&overview_size);

    let overview_title = gtk::Label::builder()
        .label(tr("Scan overview"))
        .xalign(0.0)
        .build();
    overview_title.add_css_class("heading");
    let review_detected_label = gtk::Label::builder().xalign(0.0).wrap(true).build();
    review_detected_label.add_css_class("title-3");
    let overview_description = gtk::Label::builder()
        .label(tr(
            "Green outlines show the detected areas in the original scan.",
        ))
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(38)
        .build();
    overview_description.add_css_class("dim-label");
    let overview_copy = gtk::Box::new(gtk::Orientation::Vertical, 8);
    overview_copy.set_valign(gtk::Align::Center);
    overview_copy.set_hexpand(true);
    overview_copy.append(&overview_title);
    overview_copy.append(&review_detected_label);
    overview_copy.append(&overview_description);

    let overview_card = gtk::Box::new(gtk::Orientation::Horizontal, 20);
    overview_card.set_margin_top(4);
    overview_card.set_margin_bottom(4);
    overview_card.set_margin_start(4);
    overview_card.set_margin_end(4);
    overview_card.add_css_class("review-overview-card");
    overview_card.append(&overview_frame);
    overview_card.append(&overview_copy);

    let review_flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .row_spacing(16)
        .column_spacing(16)
        .max_children_per_line(4)
        .min_children_per_line(1)
        .homogeneous(true)
        .hexpand(true)
        .halign(gtk::Align::Fill)
        .valign(gtk::Align::Start)
        .build();
    review_flow.add_css_class("review-gallery");
    let review_title = gtk::Label::builder()
        .label(tr("Review detected photos"))
        .xalign(0.0)
        .wrap(true)
        .hexpand(true)
        .build();
    review_title.add_css_class("title-1");
    let review_subtitle = gtk::Label::builder()
        .label(tr("Review the selection and orientation before export."))
        .xalign(0.0)
        .wrap(true)
        .build();
    review_subtitle.add_css_class("dim-label");
    let review_heading = gtk::Box::new(gtk::Orientation::Vertical, 6);
    review_heading.set_hexpand(true);
    review_heading.append(&review_title);
    review_heading.append(&review_subtitle);
    let review_save_button = gtk::Button::builder()
        .label(tr("Save photos"))
        .action_name("win.save-review")
        .build();
    review_save_button.add_css_class("suggested-action");
    review_save_button.add_css_class("review-primary-action");
    let review_discard_button = gtk::Button::builder()
        .label(tr("Discard"))
        .action_name("win.discard-review")
        .build();
    let review_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    review_buttons.set_valign(gtk::Align::Center);
    review_buttons.append(&review_discard_button);
    review_buttons.append(&review_save_button);
    let review_header = adw::WrapBox::builder()
        .orientation(gtk::Orientation::Horizontal)
        .child_spacing(16)
        .line_spacing(12)
        .natural_line_length(720)
        .wrap_policy(adw::WrapPolicy::Natural)
        .build();
    review_header.add_css_class("review-header");
    review_header.append(&review_heading);
    review_header.append(&review_buttons);

    let gallery_title = gtk::Label::builder()
        .label(tr("Detected photos"))
        .xalign(0.0)
        .hexpand(true)
        .build();
    gallery_title.add_css_class("title-3");
    let review_selection_label = gtk::Label::builder().xalign(1.0).wrap(true).build();
    review_selection_label.add_css_class("review-selection-counter");
    review_selection_label.set_accessible_role(gtk::AccessibleRole::Status);
    let gallery_heading = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    gallery_heading.set_margin_start(4);
    gallery_heading.set_margin_end(4);
    gallery_heading.append(&gallery_title);
    gallery_heading.append(&review_selection_label);

    let review_content = gtk::Box::new(gtk::Orientation::Vertical, 20);
    review_content.add_css_class("review-content");
    review_content.append(&overview_card);
    review_content.append(&gallery_heading);
    review_content.append(&review_flow);
    let review_clamp = adw::Clamp::builder()
        .maximum_size(1120)
        .tightening_threshold(960)
        .child(&review_content)
        .build();
    let review_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    review_page.add_css_class("review-page");
    review_page.append(&review_header);
    review_page.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let review_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&review_clamp)
        .build();
    review_page.append(&review_scroll);

    let empty = gtk::Box::new(gtk::Orientation::Vertical, 12);
    empty.set_margin_start(24);
    empty.set_margin_end(24);
    empty.set_halign(gtk::Align::Center);
    empty.set_valign(gtk::Align::Center);
    let icon = gtk::Image::from_icon_name("scanner-symbolic");
    icon.set_pixel_size(48);
    icon.set_accessible_role(gtk::AccessibleRole::Presentation);
    icon.add_css_class("empty-preview-icon");
    let title = gtk::Label::builder().label(tr("No preview yet")).build();
    title.add_css_class("title-1");
    let description = gtk::Label::builder()
        .label(tr("Start a scan or open an existing scan file."))
        .wrap(true)
        .justify(gtk::Justification::Center)
        .max_width_chars(38)
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
    stack.add_named(&scroller, Some("picture"));
    stack.add_named(&review_page, Some("review"));
    stack.set_visible_child_name("empty");
    (
        stack,
        picture,
        scroller,
        review_overview,
        review_detected_label,
        review_selection_label,
        review_flow,
        review_save_button,
    )
}

fn build_preview_pane(ui: &Ui) -> adw::ToolbarView {
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        &tr("Preview"),
        &tr("Detected photo boundaries"),
    )));
    let sidebar_button = gtk::Button::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text(tr("Show or hide settings"))
        .action_name("win.toggle-sidebar")
        .build();
    sidebar_button.add_css_class("icon-action");
    sidebar_button.add_css_class("flat");
    sidebar_button.update_property(&[
        gtk::accessible::Property::Label(&tr("Show or hide settings")),
        gtk::accessible::Property::Description(&tr(
            "Opens or closes the sidebar containing the scan settings",
        )),
        gtk::accessible::Property::KeyShortcuts("F10"),
    ]);
    header.pack_start(&sidebar_button);
    let zoom_controls = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    zoom_controls.add_css_class("zoom-controls");
    for (icon, tooltip, action) in [
        ("zoom-out-symbolic", tr("Zoom out"), "win.zoom-out"),
        (
            "zoom-fit-best-symbolic",
            tr("Fit to window"),
            "win.zoom-fit",
        ),
        ("zoom-in-symbolic", tr("Zoom in"), "win.zoom-in"),
    ] {
        let button = gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(tooltip)
            .action_name(action)
            .build();
        button.add_css_class("icon-action");
        button.add_css_class("flat");
        zoom_controls.append(&button);
    }
    header.pack_end(&zoom_controls);
    toolbar.add_top_bar(&header);

    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.set_margin_top(12);
    frame.set_margin_bottom(16);
    frame.set_margin_start(16);
    frame.set_margin_end(16);
    frame.set_overflow(gtk::Overflow::Hidden);
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

    let error_details = gio::SimpleAction::new("error-details", None);
    let weak_ui = Rc::downgrade(ui);
    error_details.connect_activate(move |_, _| {
        if let Some(ui) = weak_ui.upgrade() {
            show_error_details(&ui);
        }
    });

    let weak_ui = Rc::downgrade(ui);
    ui.zoom_in_action.connect_activate(move |_, _| {
        if let Some(ui) = weak_ui.upgrade() {
            zoom_preview(&ui, 1.25);
        }
    });
    let weak_ui = Rc::downgrade(ui);
    ui.zoom_out_action.connect_activate(move |_, _| {
        if let Some(ui) = weak_ui.upgrade() {
            zoom_preview(&ui, 1.0 / 1.25);
        }
    });
    let weak_ui = Rc::downgrade(ui);
    ui.zoom_fit_action.connect_activate(move |_, _| {
        if let Some(ui) = weak_ui.upgrade() {
            if ui.preview_stack.visible_child_name().as_deref() == Some("review") {
                set_review_zoom(&ui, 1.0);
            } else {
                set_preview_zoom(&ui, 0.0);
            }
        }
    });

    let weak_ui = Rc::downgrade(ui);
    ui.save_review_action.connect_activate(move |_, _| {
        if let Some(ui) = weak_ui.upgrade() {
            save_review(&ui);
        }
    });
    let weak_ui = Rc::downgrade(ui);
    ui.discard_review_action.connect_activate(move |_, _| {
        if let Some(ui) = weak_ui.upgrade() {
            discard_review(&ui);
        }
    });

    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak_ui = Rc::downgrade(ui);
    scroll.connect_scroll(move |controller, _, dy| {
        if !controller
            .current_event_state()
            .contains(gtk::gdk::ModifierType::CONTROL_MASK)
        {
            return glib::Propagation::Proceed;
        }
        if let Some(ui) = weak_ui.upgrade() {
            zoom_preview(&ui, if dy < 0.0 { 1.25 } else { 1.0 / 1.25 });
        }
        glib::Propagation::Stop
    });
    ui.preview_stack.add_controller(scroll);

    ui.window.add_action(&ui.scan_action);
    ui.window.add_action(&ui.import_action);
    ui.window.add_action(&ui.refresh_action);
    ui.window.add_action(&ui.output_action);
    ui.window.add_action(&ui.cancel_action);
    ui.window.add_action(&toggle_sidebar);
    ui.window.add_action(&close);
    ui.window.add_action(&error_details);
    ui.window.add_action(&ui.zoom_in_action);
    ui.window.add_action(&ui.zoom_out_action);
    ui.window.add_action(&ui.zoom_fit_action);
    ui.window.add_action(&ui.save_review_action);
    ui.window.add_action(&ui.discard_review_action);

    if let Some(application) = ui.window.application() {
        application.set_accels_for_action("win.scan", &["F9"]);
        application.set_accels_for_action("win.import", &["<Primary>o"]);
        application.set_accels_for_action("win.refresh", &["<Primary>r"]);
        application.set_accels_for_action("win.choose-output", &["<Primary>l"]);
        application.set_accels_for_action("win.toggle-sidebar", &["F10"]);
        application.set_accels_for_action("win.cancel", &["Escape"]);
        application.set_accels_for_action("win.zoom-in", &["<Primary>plus"]);
        application.set_accels_for_action("win.zoom-out", &["<Primary>minus"]);
        application.set_accels_for_action("win.zoom-fit", &["<Primary>0"]);
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
    ui.review_before_save.set_sensitive(splitting);
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
        .title(tr("Select output folder"))
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
        .title(tr("Open scan file"))
        .modal(true)
        .build();
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(&tr("Images")));
    filter.add_mime_type("image/png");
    filter.add_mime_type("image/jpeg");
    filter.add_mime_type("image/tiff");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    dialog.set_filters(Some(&filters));
    let import_ui = Rc::clone(ui);
    dialog.open_multiple(Some(&ui.window), None::<&gio::Cancellable>, move |result| {
        if let Ok(files) = result {
            let paths = (0..files.n_items())
                .filter_map(|index| files.item(index))
                .filter_map(|item| item.downcast::<gio::File>().ok())
                .filter_map(|file| file.path())
                .collect::<Vec<_>>();
            if !paths.is_empty() {
                start_import(&import_ui, paths);
            }
        }
    });
}

fn collect_config(ui: &Ui, scanned: bool) -> Result<SplitConfig> {
    let capture_date = match NaiveDate::parse_from_str(ui.date_entry.text().trim(), "%d.%m.%Y") {
        Ok(date) => {
            set_date_entry_validity(&ui.date_entry, true);
            date
        }
        Err(error) => {
            set_date_entry_validity(&ui.date_entry, false);
            ui.date_entry.grab_focus();
            return Err(error).context(tr("Capture date must use the DD.MM.YYYY format"));
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

fn connect_date_picker(entry: &adw::EntryRow, calendar: &gtk::Calendar, popover: &gtk::Popover) {
    let entry_for_validation = entry.clone();
    entry.connect_changed(move |_| {
        let valid =
            NaiveDate::parse_from_str(entry_for_validation.text().trim(), "%d.%m.%Y").is_ok();
        set_date_entry_validity(&entry_for_validation, valid);
    });
    set_date_entry_validity(
        entry,
        NaiveDate::parse_from_str(entry.text().trim(), "%d.%m.%Y").is_ok(),
    );

    let selecting_date = Rc::new(Cell::new(false));
    let entry_for_open = entry.clone();
    let calendar_for_open = calendar.clone();
    let selecting_for_open = Rc::clone(&selecting_date);
    popover.connect_map(move |_| {
        let Ok(date) = NaiveDate::parse_from_str(entry_for_open.text().trim(), "%d.%m.%Y") else {
            return;
        };
        if let Ok(date_time) = glib::DateTime::from_local(
            date.year(),
            date.month() as i32,
            date.day() as i32,
            12,
            0,
            0.0,
        ) {
            selecting_for_open.set(true);
            calendar_for_open.set_date(&date_time);
            selecting_for_open.set(false);
        }
    });

    let entry_for_selection = entry.clone();
    let popover_for_selection = popover.clone();
    calendar.connect_day_selected(move |calendar| {
        if selecting_date.get() {
            return;
        }
        let date = calendar.date();
        entry_for_selection.set_text(&format!(
            "{:02}.{:02}.{:04}",
            date.day_of_month(),
            date.month(),
            date.year()
        ));
        popover_for_selection.popdown();
    });
}

fn set_date_entry_validity(entry: &adw::EntryRow, valid: bool) {
    entry.update_state(&[gtk::accessible::State::Invalid(if valid {
        gtk::AccessibleInvalidState::False
    } else {
        gtk::AccessibleInvalidState::True
    })]);
    if valid {
        entry.remove_css_class("error");
    } else {
        entry.add_css_class("error");
    }
}

fn start_scan(ui: &Ui) {
    if ui.busy.get() {
        return;
    }
    let selected = ui.device_dropdown.selected() as usize;
    let Some(device) = ui.devices.borrow().get(selected).cloned() else {
        show_error(ui, &tr("No scanner is selected."));
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
    let review = !full_scan && ui.review_before_save.is_active();
    let scan_mode = if full_scan {
        ScanMode::Full
    } else if review {
        ScanMode::Review
    } else {
        ScanMode::Direct
    };
    let dpi = config.dpi.unwrap_or(600);
    let output = ui.output_directory.borrow().clone();
    let sender = ui.sender.clone();
    if drop_review(ui) {
        show_previous_preview(ui);
    }
    let (operation_id, cancellation) = begin_operation(ui);
    set_busy(
        ui,
        true,
        &tr_args("Scanning at {dpi} dpi…", &[("dpi", dpi.to_string())]),
    );
    ui.spinner.stop();
    ui.progress_bar.set_fraction(0.0);
    ui.progress_bar.set_visible(true);
    thread::spawn(move || {
        let progress_sender = sender.clone();
        let progress = Box::new(move |percent| {
            let _ = progress_sender.try_send(Message::Progress {
                operation_id,
                percent,
            });
        });
        let phase_sender = sender.clone();
        let phase_text = status_after_scan(scan_mode);
        let scan_complete = Box::new(move || {
            let _ = phase_sender.try_send(Message::ScanComplete {
                operation_id,
                text: phase_text.clone(),
            });
        });
        let result = run_worker(|| {
            scan_work(
                &device,
                dpi,
                &output,
                &config,
                scan_mode,
                &cancellation,
                ScanCallbacks {
                    progress: Some(progress),
                    complete: Some(scan_complete),
                },
            )
            .map_err(|error| {
                if cancellation.is_cancelled() {
                    cancelled_message()
                } else {
                    format!("{error:#}")
                }
            })
        });
        let message = match result {
            Ok(ScanOutcome::Work(result)) => Message::Work {
                operation_id,
                result: Ok(result),
            },
            Ok(ScanOutcome::Review(review)) => Message::ReviewReady {
                operation_id,
                result: Ok(review),
            },
            Err(error) => Message::Work {
                operation_id,
                result: Err(error),
            },
        };
        let _ = sender.send_blocking(message);
    });
}

fn scan_work(
    device: &ScannerDevice,
    dpi: u32,
    output: &Path,
    config: &SplitConfig,
    mode: ScanMode,
    cancellation: &ScannerCancellation,
    callbacks: ScanCallbacks,
) -> Result<ScanOutcome> {
    let ScanCallbacks { progress, complete } = callbacks;
    ensure_not_cancelled(cancellation)?;
    let temporary = TempDir::with_prefix("photoscanner-")?;
    let source = temporary.path().join("scan.png");
    scan_to_file_with_progress(
        &source,
        Some(&device.name),
        dpi,
        Duration::from_secs(600),
        cancellation,
        progress,
    )?;
    ensure_not_cancelled(cancellation)?;
    if let Some(complete) = complete {
        complete();
    }
    if matches!(mode, ScanMode::Full) {
        let path = save_full_scan(&source, output, config, None)?;
        ensure_not_cancelled(cancellation)?;
        let (preview, preview_directory) = bounded_preview(&path)?;
        return Ok(ScanOutcome::Work(WorkResult {
            title: tr("Complete scan saved"),
            detail: path.display().to_string(),
            preview,
            preview_directory,
            capture_date: config
                .capture_date
                .unwrap_or_else(|| Local::now().date_naive()),
        }));
    }
    if matches!(mode, ScanMode::Review) {
        return prepare_review(
            std::slice::from_ref(&source),
            output,
            config,
            cancellation,
            None,
        )
        .map(ScanOutcome::Review);
    }
    let result = split_scan(&source, output, config, None, true)?;
    ensure_not_cancelled(cancellation)?;
    let preview_source = result
        .preview
        .clone()
        .or_else(|| result.files.first().cloned())
        .ok_or_else(|| anyhow!(tr("No preview file was created")))?;
    let (preview, preview_directory) = bounded_preview(&preview_source)?;
    Ok(ScanOutcome::Work(WorkResult {
        title: trn(
            "{count} photo saved",
            "{count} photos saved",
            result.files.len(),
        ),
        detail: tr_args(
            "{output} · Threshold {threshold}",
            &[
                ("output", output.display().to_string()),
                ("threshold", format!("{:.1}", result.threshold_used)),
            ],
        ),
        preview,
        preview_directory,
        capture_date: config
            .capture_date
            .unwrap_or_else(|| Local::now().date_naive()),
    }))
}

fn start_import(ui: &Ui, sources: Vec<PathBuf>) {
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
    let review = !full_scan && ui.review_before_save.is_active();
    let output = ui.output_directory.borrow().clone();
    let sender = ui.sender.clone();
    if drop_review(ui) {
        show_previous_preview(ui);
    }
    let (operation_id, cancellation) = begin_operation(ui);
    set_busy(ui, true, &tr("Analyzing scan file…"));
    thread::spawn(move || {
        if review {
            let status_sender = sender.clone();
            let result = run_worker(|| {
                prepare_review(
                    &sources,
                    &output,
                    &config,
                    &cancellation,
                    Some(Box::new(move |index, total| {
                        let _ = status_sender.try_send(Message::Status {
                            operation_id,
                            text: tr_args(
                                "Analyzing file {index} of {total}…",
                                &[("index", index.to_string()), ("total", total.to_string())],
                            ),
                        });
                    })),
                )
                .map_err(|error| {
                    if cancellation.is_cancelled() {
                        cancelled_message()
                    } else {
                        format!("{error:#}")
                    }
                })
            });
            let _ = sender.send_blocking(Message::ReviewReady {
                operation_id,
                result,
            });
            return;
        }
        let result = run_worker(|| {
            (|| -> Result<WorkResult> {
                let file_count = sources.len();
                let mut photo_count = 0usize;
                let mut failures = Vec::new();
                let mut last_preview = None;
                for (index, source) in sources.iter().enumerate() {
                    ensure_not_cancelled(&cancellation)?;
                    let _ = sender.try_send(Message::Status {
                        operation_id,
                        text: tr_args(
                            "Analyzing file {index} of {total}…",
                            &[
                                ("index", (index + 1).to_string()),
                                ("total", file_count.to_string()),
                            ],
                        ),
                    });
                    let processed = (|| -> Result<(usize, PathBuf, TempDir)> {
                        if !source.is_file() {
                            bail!(tr_args(
                                "The scan file no longer exists: {path}",
                                &[("path", source.display().to_string())],
                            ));
                        }
                        if full_scan {
                            let path = save_full_scan(source, &output, &config, None)?;
                            ensure_not_cancelled(&cancellation)?;
                            let (preview, preview_directory) = bounded_preview(&path)?;
                            return Ok((1, preview, preview_directory));
                        }
                        let result = split_scan(source, &output, &config, None, true)?;
                        ensure_not_cancelled(&cancellation)?;
                        let preview_source = result
                            .preview
                            .clone()
                            .or_else(|| result.files.first().cloned())
                            .ok_or_else(|| anyhow!(tr("No preview file was created")))?;
                        let (preview, preview_directory) = bounded_preview(&preview_source)?;
                        Ok((result.files.len(), preview, preview_directory))
                    })();
                    match processed {
                        Ok((count, preview, preview_directory)) => {
                            photo_count += count;
                            last_preview = Some((preview, preview_directory));
                        }
                        Err(error) => {
                            if cancellation.is_cancelled() {
                                return Err(error);
                            }
                            let name = source
                                .file_name()
                                .and_then(|name| name.to_str())
                                .map(str::to_string)
                                .unwrap_or_else(|| tr("Unknown file"));
                            failures
                                .push(format!("{name}: {}", error_summary(&format!("{error:#}"))));
                        }
                    }
                }
                ensure_not_cancelled(&cancellation)?;
                let Some((preview, preview_directory)) = last_preview else {
                    bail!(tr_args(
                        "No file could be processed:\n{errors}",
                        &[("errors", failures.join("\n"))],
                    ));
                };
                let mut detail = output.display().to_string();
                if !failures.is_empty() {
                    detail.push_str(&tr("\nErrors:\n"));
                    detail.push_str(&failures.join("\n"));
                }
                Ok(WorkResult {
                    title: tr_args(
                        "Saved {photos} photo(s) from {files} file(s)",
                        &[
                            ("photos", photo_count.to_string()),
                            ("files", file_count.to_string()),
                        ],
                    ),
                    detail,
                    preview,
                    preview_directory,
                    capture_date: config
                        .capture_date
                        .unwrap_or_else(|| Local::now().date_naive()),
                })
            })()
            .map_err(|error| {
                if cancellation.is_cancelled() {
                    cancelled_message()
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

fn prepare_review(
    sources: &[PathBuf],
    output: &Path,
    config: &SplitConfig,
    cancellation: &ScannerCancellation,
    status: Option<Box<dyn Fn(usize, usize) + Send>>,
) -> Result<ReviewData> {
    let staging = TempDir::with_prefix("photoscanner-review-")?;
    let mut photos = Vec::new();
    let mut groups = Vec::new();
    let mut failures = Vec::new();
    let mut overviews = Vec::new();
    for (source_index, source) in sources.iter().enumerate() {
        ensure_not_cancelled(cancellation)?;
        if let Some(status) = status.as_deref() {
            status(source_index + 1, sources.len());
        }
        let group_index = groups.len();
        let prepared = (|| -> Result<(AnalyzedScan, Vec<ReviewPhotoData>, PathBuf)> {
            let analyzed = analyze_scan(source, config)?;
            let mut group_photos = Vec::new();
            let mut group_regions = Vec::new();
            for (region_index, region) in analyzed.regions.iter().enumerate() {
                ensure_not_cancelled(cancellation)?;
                let photo = warp_detected_photo(&analyzed, region)?;
                if photo.rows().min(photo.cols()) < 10 {
                    continue;
                }
                let full_path = staging
                    .path()
                    .join(format!("photo_{source_index:03}_{region_index:03}.png"));
                if !imgcodecs::imwrite_def(&full_path, &photo)? {
                    bail!(tr("Could not stage the review photo"));
                }
                let thumbnail_path = staging
                    .path()
                    .join(format!("thumb_{source_index:03}_{region_index:03}.jpg"));
                write_thumbnail(&photo, &thumbnail_path)?;
                group_regions.push(region.clone());
                group_photos.push(ReviewPhotoData {
                    full_path,
                    thumbnail_path,
                    group_index,
                    region: region.clone(),
                });
            }
            if group_photos.is_empty() {
                bail!(tr("No sufficiently large photos were detected"));
            }
            let overview_path = staging
                .path()
                .join(format!("overview_{source_index:03}.jpg"));
            save_detection_preview(
                &analyzed,
                &group_regions,
                &overview_path,
                config.capture_date,
            )?;
            Ok((analyzed, group_photos, overview_path))
        })();
        match prepared {
            Ok((analyzed, mut group_photos, overview_path)) => {
                photos.append(&mut group_photos);
                groups.push(ReviewGroup { analyzed });
                overviews.push(overview_path);
            }
            Err(error) => {
                if cancellation.is_cancelled() {
                    return Err(error);
                }
                let name = source
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| tr("Unknown file"));
                failures.push(format!("{name}: {}", error_summary(&format!("{error:#}"))));
            }
        }
    }
    ensure_not_cancelled(cancellation)?;
    if overviews.is_empty() {
        bail!(tr_args(
            "No file could be prepared for review:\n{errors}",
            &[("errors", failures.join("\n"))],
        ));
    }
    let overview = compose_image_sheet(&overviews, staging.path())?;
    Ok(ReviewData {
        staging,
        photos,
        groups,
        overview,
        config: config.clone(),
        output: output.to_path_buf(),
        failures,
    })
}

fn compose_image_sheet(images: &[PathBuf], directory: &Path) -> Result<PathBuf> {
    const WIDTH: i32 = 1200;
    const HEIGHT: i32 = 900;
    const CELL_PADDING: i32 = 16;

    match images {
        [] => bail!(tr("No preview file was created")),
        [image] => return Ok(image.clone()),
        _ => {}
    }

    let columns = (images.len() as f64).sqrt().ceil() as i32;
    let rows = images.len().div_ceil(columns as usize) as i32;
    let cell_width = WIDTH / columns;
    let cell_height = HEIGHT / rows;
    let mut sheet = Mat::new_rows_cols_with_default(
        HEIGHT,
        WIDTH,
        core::CV_8UC3,
        core::Scalar::new(24.0, 24.0, 24.0, 0.0),
    )?;

    for (index, path) in images.iter().enumerate() {
        let image = imgcodecs::imread(path, imgcodecs::IMREAD_COLOR).with_context(|| {
            tr_args(
                "Could not read preview: {path}",
                &[("path", path.display().to_string())],
            )
        })?;
        if image.empty() {
            bail!(tr_args(
                "Preview contains no image data: {path}",
                &[("path", path.display().to_string())],
            ));
        }
        let available_width = (cell_width - 2 * CELL_PADDING).max(1);
        let available_height = (cell_height - 2 * CELL_PADDING).max(1);
        let scale = (f64::from(available_width) / f64::from(image.cols()))
            .min(f64::from(available_height) / f64::from(image.rows()));
        let target_width = (f64::from(image.cols()) * scale).round().max(1.0) as i32;
        let target_height = (f64::from(image.rows()) * scale).round().max(1.0) as i32;
        let mut thumbnail = Mat::default();
        imgproc::resize(
            &image,
            &mut thumbnail,
            Size::new(target_width, target_height),
            0.0,
            0.0,
            imgproc::INTER_AREA,
        )?;

        let column = index as i32 % columns;
        let row = index as i32 / columns;
        let x = column * cell_width + (cell_width - target_width) / 2;
        let y = row * cell_height + (cell_height - target_height) / 2;
        let mut destination = Mat::roi_mut(
            &mut sheet,
            core::Rect::new(x, y, target_width, target_height),
        )?;
        thumbnail.copy_to(&mut destination)?;
    }

    let path = directory.join("overview_batch.jpg");
    let parameters = Vector::from_slice(&[imgcodecs::IMWRITE_JPEG_QUALITY, 88]);
    if !imgcodecs::imwrite(&path, &sheet, &parameters)? {
        bail!(tr("Could not save the preview file"));
    }
    Ok(path)
}

fn write_thumbnail(photo: &Mat, path: &Path) -> Result<()> {
    const THUMBNAIL_MAX_EDGE: i32 = 360;
    let largest_edge = photo.cols().max(photo.rows());
    let scale = (f64::from(THUMBNAIL_MAX_EDGE) / f64::from(largest_edge)).min(1.0);
    let mut thumbnail = Mat::default();
    if scale < 1.0 {
        imgproc::resize(
            photo,
            &mut thumbnail,
            Size::new(
                (f64::from(photo.cols()) * scale).round().max(1.0) as i32,
                (f64::from(photo.rows()) * scale).round().max(1.0) as i32,
            ),
            0.0,
            0.0,
            imgproc::INTER_AREA,
        )?;
    } else {
        thumbnail = photo.clone();
    }
    let parameters = Vector::from_slice(&[imgcodecs::IMWRITE_JPEG_QUALITY, 86]);
    if !imgcodecs::imwrite(path, &thumbnail, &parameters)? {
        bail!(tr("Could not save the thumbnail"));
    }
    Ok(())
}

fn request_devices(ui: &Ui) {
    if ui.busy.get() {
        return;
    }
    ui.discovery_pending.set(true);
    let (operation_id, cancellation) = begin_operation(ui);
    ui.refresh_action.set_enabled(false);
    ui.scan_action.set_enabled(false);
    ui.cancel_action.set_enabled(true);
    ui.cancel_button.set_visible(true);
    set_status(
        ui,
        &tr("Searching for scanners…"),
        gtk::AccessibleAnnouncementPriority::Low,
    );
    ui.spinner.start();
    let sender = ui.sender.clone();
    thread::spawn(move || {
        let result = run_worker(|| {
            list_devices_cancellable(DEVICE_DISCOVERY_TIMEOUT, &cancellation).map_err(|error| {
                if cancellation.is_cancelled() {
                    cancelled_message()
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
    let work_message = matches!(&message, Message::Work { .. } | Message::ReviewReady { .. });
    let message_operation_id = match &message {
        Message::Progress { operation_id, .. }
        | Message::Status { operation_id, .. }
        | Message::ScanComplete { operation_id, .. }
        | Message::Devices { operation_id, .. }
        | Message::ReviewReady { operation_id, .. }
        | Message::Work { operation_id, .. } => *operation_id,
    };
    if message_operation_id != ui.operation_id.get() {
        return;
    }
    if let Message::Progress { percent, .. } = &message {
        if !ui.closing.get() {
            ui.progress_bar.set_fraction(*percent / 100.0);
            set_status(
                ui,
                &tr_args(
                    "Scanning… {percent}%",
                    &[("percent", format!("{percent:.0}"))],
                ),
                gtk::AccessibleAnnouncementPriority::Low,
            );
        }
        return;
    }
    if let Message::Status { text, .. } = &message {
        if !ui.closing.get() {
            set_status(ui, text, gtk::AccessibleAnnouncementPriority::Low);
        }
        return;
    }
    if let Message::ScanComplete { text, .. } = &message {
        if !ui.closing.get() {
            ui.progress_bar.set_visible(false);
            ui.progress_bar.set_fraction(0.0);
            ui.spinner.start();
            set_status(ui, text, gtk::AccessibleAnnouncementPriority::Low);
        }
        return;
    }
    if matches!(&message, Message::Devices { .. }) {
        ui.discovery_pending.set(false);
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
        Message::Progress { .. } => {
            unreachable!("progress is handled before terminal messages")
        }
        Message::Status { .. } => {
            unreachable!("status is handled before terminal messages")
        }
        Message::ScanComplete { .. } => {
            unreachable!("scan completion is handled before terminal messages")
        }
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
                ui.device_model.append(&tr("No scanner detected"));
                ui.scan_action.set_enabled(false);
                set_status(
                    ui,
                    &tr("No scanner detected"),
                    gtk::AccessibleAnnouncementPriority::Medium,
                );
                ui.import_button.grab_focus();
            } else {
                ui.scan_action.set_enabled(true);
                set_status(
                    ui,
                    &tr("Scanner ready"),
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
            if error != cancelled_message() {
                show_error(ui, &error);
                ui.refresh_button.grab_focus();
            }
        }
        Message::ReviewReady {
            result: Ok(review), ..
        } => {
            let count = review.photos.len();
            set_busy(
                ui,
                false,
                &tr_args(
                    "{count} photo(s) detected – please review",
                    &[("count", count.to_string())],
                ),
            );
            show_review(ui, review);
        }
        Message::ReviewReady {
            result: Err(error), ..
        } => {
            if error.starts_with(&cancelled_message()) {
                set_busy(ui, false, &tr("Operation cancelled"));
            } else {
                set_busy(ui, false, &tr("Review could not be prepared"));
                show_error(ui, &error);
            }
        }
        Message::Work {
            result: Ok(result), ..
        } => {
            drop_review(ui);
            ui.last_capture_date.set(Some(result.capture_date));
            save_settings(ui);
            set_busy(ui, false, &result.title);
            set_status(
                ui,
                &format!("{}\n{}", result.title, result.detail),
                gtk::AccessibleAnnouncementPriority::Medium,
            );
            ui.picture
                .set_file(Some(&gio::File::for_path(&result.preview)));
            set_preview_zoom(ui, 0.0);
            set_zoom_actions_enabled(ui, true);
            ui.picture.update_property(&[
                gtk::accessible::Property::Label(&tr("Scan preview")),
                gtk::accessible::Property::Description(&result.title),
            ]);
            *ui.preview_directory.borrow_mut() = Some(result.preview_directory);
            ui.preview_stack.set_visible_child_name("picture");
            ui.window.set_default_widget(Some(&ui.scan_button));
            ui.toast_overlay.add_toast(adw::Toast::new(&result.title));
        }
        Message::Work {
            result: Err(error), ..
        } => {
            drop_review(ui);
            show_previous_preview(ui);
            ui.window.set_default_widget(Some(&ui.scan_button));
            if error.starts_with(&cancelled_message()) {
                set_busy(ui, false, &tr("Operation cancelled"));
            } else {
                set_busy(ui, false, &tr("Operation failed"));
                show_error(ui, &error);
            }
        }
    }
    if work_message && ui.discovery_pending.get() {
        request_devices(ui);
    }
}

fn show_review(ui: &Ui, review: ReviewData) {
    drop_review(ui);
    ui.review_overview
        .set_file(Some(&gio::File::for_path(&review.overview)));
    let photo_count = review.photos.len();
    let included_count = Rc::new(Cell::new(photo_count));
    ui.review_detected_label.set_label(&trn(
        "{count} photo detected",
        "{count} photos detected",
        photo_count,
    ));
    update_review_selection_label(&ui.review_selection_label, photo_count, photo_count);
    ui.review_save_button.set_label(&trn(
        "Save {count} photo",
        "Save {count} photos",
        photo_count,
    ));
    let mut selections = Vec::with_capacity(review.photos.len());
    for (index, photo) in review.photos.iter().enumerate() {
        let include = Rc::new(Cell::new(true));
        let quarter_turns = Rc::new(Cell::new(0));
        let (card, image) = build_review_card(
            index,
            photo,
            &include,
            &quarter_turns,
            ReviewCardControls {
                included_count: &included_count,
                total_count: photo_count,
                selection_label: &ui.review_selection_label,
                save_button: &ui.review_save_button,
                save_action: &ui.save_review_action,
            },
        );
        ui.review_flow.insert(&card, -1);
        ui.review_images.borrow_mut().push(image);
        selections.push(ReviewSelection {
            include,
            quarter_turns,
        });
    }
    *ui.review_state.borrow_mut() = Some(StoredReview {
        data: review,
        selections,
    });
    ui.save_review_action.set_enabled(true);
    ui.discard_review_action.set_enabled(true);
    ui.preview_stack.set_visible_child_name("review");
    set_review_zoom(ui, 1.0);
    ui.scan_button.remove_css_class("suggested-action");
    ui.window.set_default_widget(Some(&ui.review_save_button));
}

struct ReviewImage {
    frame: gtk::Box,
    width: adw::Clamp,
    height: adw::Clamp,
}

impl ReviewImage {
    fn set_zoom(&self, zoom: f64) {
        let width = (224.0 * zoom).round() as i32;
        let height = (176.0 * zoom).round() as i32;
        self.frame
            .set_size_request((184.0 * zoom).round() as i32, height);
        self.width.set_maximum_size(width);
        self.width.set_tightening_threshold(width);
        self.height.set_maximum_size(height);
        self.height.set_tightening_threshold(height);
    }
}

struct ReviewCardControls<'a> {
    included_count: &'a Rc<Cell<usize>>,
    total_count: usize,
    selection_label: &'a gtk::Label,
    save_button: &'a gtk::Button,
    save_action: &'a gio::SimpleAction,
}

fn build_review_card(
    index: usize,
    photo: &ReviewPhotoData,
    include: &Rc<Cell<bool>>,
    quarter_turns: &Rc<Cell<u8>>,
    context: ReviewCardControls<'_>,
) -> (gtk::Box, ReviewImage) {
    let picture = gtk::Picture::builder()
        .file(&gio::File::for_path(&photo.thumbnail_path))
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    picture.add_css_class("review-photo");
    let picture_frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    picture_frame.set_overflow(gtk::Overflow::Hidden);
    picture_frame.add_css_class("review-photo-frame");
    // A stable proofing area keeps portrait and landscape photos equally readable.
    picture_frame.set_size_request(184, 176);
    let picture_height = adw::Clamp::builder()
        .orientation(gtk::Orientation::Vertical)
        .maximum_size(176)
        .tightening_threshold(176)
        .child(&picture)
        .build();
    let picture_size = adw::Clamp::builder()
        .maximum_size(224)
        .tightening_threshold(224)
        .child(&picture_height)
        .build();
    picture_frame.append(&picture_size);
    let check = gtk::CheckButton::builder()
        .label(tr_args(
            "Photo {number}",
            &[("number", (index + 1).to_string())],
        ))
        .active(true)
        .hexpand(true)
        .build();
    check.update_property(&[gtk::accessible::Property::Description(&tr_args(
        "Include photo {number} in the export",
        &[("number", (index + 1).to_string())],
    ))]);
    let rotate = gtk::Button::builder()
        .icon_name("object-rotate-right-symbolic")
        .tooltip_text(tr_args(
            "Rotate photo {number} 90° clockwise",
            &[("number", (index + 1).to_string())],
        ))
        .valign(gtk::Align::Center)
        .build();
    rotate.add_css_class("flat");
    rotate.add_css_class("icon-action");
    rotate.update_property(&[gtk::accessible::Property::Label(&tr_args(
        "Rotate photo {number} 90° clockwise",
        &[("number", (index + 1).to_string())],
    ))]);
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    controls.add_css_class("review-card-controls");
    controls.append(&check);
    controls.append(&rotate);
    let card = gtk::Box::new(gtk::Orientation::Vertical, 8);
    card.set_overflow(gtk::Overflow::Hidden);
    card.add_css_class("review-card");
    card.add_css_class("included");
    card.append(&picture_frame);
    card.append(&controls);

    let include_state = Rc::clone(include);
    let count = Rc::clone(context.included_count);
    let save_button = context.save_button.clone();
    let save_action = context.save_action.clone();
    let selection_label = context.selection_label.clone();
    let total_count = context.total_count;
    let card_state = card.clone();
    check.connect_toggled(move |check| {
        let active = check.is_active();
        if include_state.replace(active) != active {
            let next = if active {
                count.get().saturating_add(1)
            } else {
                count.get().saturating_sub(1)
            };
            count.set(next);
            if active {
                card_state.remove_css_class("excluded");
                card_state.add_css_class("included");
            } else {
                card_state.remove_css_class("included");
                card_state.add_css_class("excluded");
            }
            save_button.set_label(&trn("Save {count} photo", "Save {count} photos", next));
            save_action.set_enabled(next > 0);
            update_review_selection_label(&selection_label, next, total_count);
        }
    });

    if let Ok(original) = Pixbuf::from_file(&photo.thumbnail_path) {
        let turns = Rc::clone(quarter_turns);
        rotate.connect_clicked(move |_| {
            let next = (turns.get() + 1) % 4;
            turns.set(next);
            let rotation = match next {
                1 => PixbufRotation::Clockwise,
                2 => PixbufRotation::Upsidedown,
                3 => PixbufRotation::Counterclockwise,
                _ => PixbufRotation::None,
            };
            if let Some(rotated) = original.rotate_simple(rotation)
                && let Ok(encoded) = rotated.save_to_bufferv("png", &[])
                && let Ok(texture) =
                    gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(encoded))
            {
                picture.set_paintable(Some(&texture));
            }
        });
    } else {
        rotate.set_sensitive(false);
    }
    (
        card,
        ReviewImage {
            frame: picture_frame,
            width: picture_size,
            height: picture_height,
        },
    )
}

fn update_review_selection_label(label: &gtk::Label, selected: usize, total: usize) {
    let text = tr_args(
        "{selected} of {total} selected",
        &[
            ("selected", selected.to_string()),
            ("total", total.to_string()),
        ],
    );
    label.set_label(&text);
    label.announce(&text, gtk::AccessibleAnnouncementPriority::Low);
}

fn drop_review(ui: &Ui) -> bool {
    ui.review_images.borrow_mut().clear();
    ui.scan_button.add_css_class("suggested-action");
    let had_review = ui.review_state.borrow_mut().take().is_some();
    while let Some(child) = ui.review_flow.first_child() {
        let Ok(child) = child.downcast::<gtk::FlowBoxChild>() else {
            break;
        };
        ui.review_flow.remove(&child);
    }
    ui.review_overview
        .set_paintable(None::<&gtk::gdk::Paintable>);
    ui.review_detected_label.set_label("");
    ui.review_selection_label.set_label("");
    ui.save_review_action.set_enabled(false);
    ui.discard_review_action.set_enabled(false);
    had_review
}

fn discard_review(ui: &Ui) {
    if !drop_review(ui) {
        return;
    }
    show_previous_preview(ui);
    ui.window.set_default_widget(Some(&ui.scan_button));
    set_status(
        ui,
        &tr("Review discarded"),
        gtk::AccessibleAnnouncementPriority::Medium,
    );
}

fn show_previous_preview(ui: &Ui) {
    let has_preview = ui.picture.paintable().is_some();
    ui.preview_stack
        .set_visible_child_name(if has_preview { "picture" } else { "empty" });
    set_zoom_actions_enabled(ui, has_preview);
}

fn save_review(ui: &Ui) {
    if ui.busy.get() {
        return;
    }
    let included = ui
        .review_state
        .borrow()
        .as_ref()
        .map(|review| {
            review
                .selections
                .iter()
                .filter(|selection| selection.include.get())
                .count()
        })
        .unwrap_or(0);
    if included == 0 {
        show_error(ui, &tr("Select at least one photo to save."));
        return;
    }
    let Some(review) = ui.review_state.borrow_mut().take() else {
        return;
    };
    let selections = review
        .selections
        .iter()
        .map(|selection| (selection.include.get(), selection.quarter_turns.get()))
        .collect::<Vec<_>>();
    ui.save_review_action.set_enabled(false);
    ui.discard_review_action.set_enabled(false);
    let sender = ui.sender.clone();
    let (operation_id, cancellation) = begin_operation(ui);
    set_busy(
        ui,
        true,
        &tr_args(
            "Saving {count} photo(s)…",
            &[("count", included.to_string())],
        ),
    );
    thread::spawn(move || {
        let result = run_worker(|| {
            save_review_work(review.data, &selections, &cancellation).map_err(|error| {
                if cancellation.is_cancelled() {
                    cancelled_message()
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

fn save_review_work(
    review: ReviewData,
    selections: &[(bool, u8)],
    cancellation: &ScannerCancellation,
) -> Result<WorkResult> {
    let mut file_count = 0usize;
    let mut saved_files = Vec::new();
    for (group_index, group) in review.groups.iter().enumerate() {
        ensure_not_cancelled(cancellation)?;
        let mut photos = Vec::new();
        let mut regions = Vec::new();
        for (index, photo) in review.photos.iter().enumerate() {
            if photo.group_index != group_index || !selections[index].0 {
                continue;
            }
            let mut image = imgcodecs::imread(&photo.full_path, imgcodecs::IMREAD_COLOR)?;
            if image.empty() {
                bail!(tr_args(
                    "Review photo contains no image data: {path}",
                    &[("path", photo.full_path.display().to_string())],
                ));
            }
            image = rotate_quarter_turns(&image, selections[index].1)?;
            photos.push(image);
            regions.push(photo.region.clone());
        }
        if photos.is_empty() {
            continue;
        }
        let result = export_photos(
            &photos,
            &group.analyzed,
            &review.output,
            &review.config,
            None,
            Some(&regions),
        )?;
        file_count += result.files.len();
        saved_files.extend(result.files);
    }
    ensure_not_cancelled(cancellation)?;
    let preview_source = compose_image_sheet(&saved_files, review.staging.path())?;
    let (preview, preview_directory) = bounded_preview(&preview_source)?;
    let mut detail = review.output.display().to_string();
    if !review.failures.is_empty() {
        detail.push_str(&tr("\nErrors:\n"));
        detail.push_str(&review.failures.join("\n"));
    }
    Ok(WorkResult {
        title: trn("{count} photo saved", "{count} photos saved", file_count),
        detail,
        preview,
        preview_directory,
        capture_date: review
            .config
            .capture_date
            .unwrap_or_else(|| Local::now().date_naive()),
    })
}

fn rotate_quarter_turns(image: &Mat, turns: u8) -> Result<Mat> {
    let code = match turns % 4 {
        0 => return Ok(image.clone()),
        1 => core::ROTATE_90_CLOCKWISE,
        2 => core::ROTATE_180,
        _ => core::ROTATE_90_COUNTERCLOCKWISE,
    };
    let mut rotated = Mat::default();
    core::rotate(image, &mut rotated, code)?;
    Ok(rotated)
}

fn set_zoom_actions_enabled(ui: &Ui, enabled: bool) {
    ui.zoom_in_action.set_enabled(enabled);
    ui.zoom_out_action.set_enabled(enabled);
    ui.zoom_fit_action.set_enabled(enabled);
}

fn set_review_zoom(ui: &Ui, zoom: f64) {
    let zoom = zoom.clamp(0.5, 2.0);
    ui.review_zoom.set(zoom);
    for image in ui.review_images.borrow().iter() {
        image.set_zoom(zoom);
    }
    ui.zoom_in_action.set_enabled(zoom < 2.0);
    ui.zoom_out_action.set_enabled(zoom > 0.5);
    ui.zoom_fit_action.set_enabled(true);
}

fn zoom_preview(ui: &Ui, factor: f64) {
    if ui.preview_stack.visible_child_name().as_deref() == Some("review") {
        set_review_zoom(ui, ui.review_zoom.get() * factor);
        return;
    }
    let Some(paintable) = ui.picture.paintable() else {
        return;
    };
    set_preview_zoom(
        ui,
        next_picture_zoom(
            ui.zoom.get(),
            factor,
            (paintable.intrinsic_width(), paintable.intrinsic_height()),
            (ui.picture.width(), ui.picture.height()),
        ),
    );
}

fn next_picture_zoom(current: f64, factor: f64, image: (i32, i32), viewport: (i32, i32)) -> f64 {
    let base = if current > 0.0 {
        current
    } else if image.0 > 0 && image.1 > 0 && viewport.0 > 0 && viewport.1 > 0 {
        (f64::from(viewport.0) / f64::from(image.0)).min(f64::from(viewport.1) / f64::from(image.1))
    } else {
        1.0
    };
    (base * factor).clamp(0.02, 4.0)
}

fn set_preview_zoom(ui: &Ui, zoom: f64) {
    ui.zoom.set(zoom);
    ui.zoom_in_action.set_enabled(zoom < 4.0);
    ui.zoom_out_action.set_enabled(zoom == 0.0 || zoom > 0.02);
    if zoom == 0.0 {
        ui.picture.set_size_request(-1, -1);
        ui.picture.set_content_fit(gtk::ContentFit::Contain);
        ui.picture.set_can_shrink(true);
        ui.picture.set_hexpand(true);
        ui.picture.set_vexpand(true);
        ui.picture.set_halign(gtk::Align::Fill);
        ui.picture.set_valign(gtk::Align::Fill);
        return;
    }
    let Some(paintable) = ui.picture.paintable() else {
        return;
    };
    let width = paintable.intrinsic_width();
    let height = paintable.intrinsic_height();
    if width <= 0 || height <= 0 {
        return;
    }

    let horizontal = ui.preview_scroller.hadjustment();
    let vertical = ui.preview_scroller.vadjustment();
    let horizontal_center = adjustment_center(&horizontal);
    let vertical_center = adjustment_center(&vertical);
    ui.picture.set_hexpand(false);
    ui.picture.set_vexpand(false);
    ui.picture.set_halign(gtk::Align::Center);
    ui.picture.set_valign(gtk::Align::Center);
    // The explicit request defines the zoom size, including sizes below 100%.
    ui.picture.set_can_shrink(true);
    ui.picture.set_content_fit(gtk::ContentFit::Contain);
    ui.picture.set_size_request(
        (f64::from(width) * zoom).round().max(1.0) as i32,
        (f64::from(height) * zoom).round().max(1.0) as i32,
    );
    glib::idle_add_local_once(move || {
        restore_adjustment_center(&horizontal, horizontal_center);
        restore_adjustment_center(&vertical, vertical_center);
    });
}

fn adjustment_center(adjustment: &gtk::Adjustment) -> f64 {
    let span = adjustment.upper() - adjustment.lower();
    if span <= 0.0 {
        0.5
    } else {
        ((adjustment.value() + adjustment.page_size() / 2.0 - adjustment.lower()) / span)
            .clamp(0.0, 1.0)
    }
}

fn restore_adjustment_center(adjustment: &gtk::Adjustment, center: f64) {
    let span = adjustment.upper() - adjustment.lower();
    adjustment.set_value(adjustment.lower() + center * span - adjustment.page_size() / 2.0);
}

fn save_settings(ui: &Ui) {
    let settings = PersistedSettings {
        output_directory: ui.output_directory.borrow().clone(),
        dpi_index: ui.dpi_dropdown.selected(),
        format_index: ui.format_dropdown.selected(),
        mode_index: ui.mode_dropdown.selected(),
        quality: ui.quality.value(),
        min_area: ui.min_area.value(),
        padding: ui.padding.value(),
        auto_threshold: ui.auto_threshold.is_active(),
        threshold: ui.threshold.value(),
        review_before_save: ui.review_before_save.is_active(),
        capture_date: ui.last_capture_date.get(),
    };
    if let Err(error) = settings.save(&ui.settings_path) {
        eprintln!(
            "{}",
            tr_args(
                "Could not save settings: {error}",
                &[("error", format!("{error:#}"))],
            )
        );
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
    ui.scan_button.set_visible(!busy);
    ui.import_button.set_visible(!busy);
    ui.progress_bar.set_visible(false);
    ui.progress_bar.set_fraction(0.0);
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
    *ui.last_error.borrow_mut() = Some(message.to_string());
    let summary = error_summary(message);
    ui.status_label.set_label(&summary);
    ui.status_label.set_tooltip_text(Some(message));
    ui.status_label
        .announce(message, gtk::AccessibleAnnouncementPriority::High);
    let mut toast = adw::Toast::builder().title(&summary).timeout(6);
    if summary != message {
        toast = toast
            .button_label(tr("Details"))
            .action_name("win.error-details");
    }
    ui.toast_overlay.add_toast(toast.build());
}

fn error_summary(message: &str) -> String {
    const MAX_CHARS: usize = 120;
    let first_line = message.lines().next().unwrap_or_default();
    if first_line.chars().count() <= MAX_CHARS {
        return first_line.to_string();
    }
    let mut summary = first_line.chars().take(MAX_CHARS - 1).collect::<String>();
    summary.push('…');
    summary
}

fn show_error_details(ui: &Ui) {
    let Some(message) = ui.last_error.borrow().clone() else {
        return;
    };
    let label = gtk::Label::builder()
        .label(message)
        .selectable(true)
        .wrap(true)
        .xalign(0.0)
        .yalign(0.0)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .max_content_height(360)
        .propagate_natural_height(true)
        .child(&label)
        .build();
    let dialog = adw::AlertDialog::new(Some(&tr("Error details")), None);
    dialog.set_extra_child(Some(&scroll));
    dialog.add_response("close", &tr("Close"));
    dialog.set_close_response("close");
    dialog.set_default_response(Some("close"));
    dialog.present(Some(&ui.window));
}

fn set_status(ui: &Ui, message: &str, priority: gtk::AccessibleAnnouncementPriority) {
    ui.status_label.set_label(message);
    ui.status_label.set_tooltip_text(Some(message));
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
            &tr("Cancellation requested…"),
            gtk::AccessibleAnnouncementPriority::Medium,
        );
    }
}

fn ensure_not_cancelled(cancellation: &ScannerCancellation) -> Result<()> {
    if cancellation.is_cancelled() {
        bail!(cancelled_message());
    }
    Ok(())
}

fn run_worker<T>(work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|_| Err(worker_panic_message()))
}

fn bounded_preview(source: &Path) -> Result<(PathBuf, TempDir)> {
    let image = imgcodecs::imread(source, imgcodecs::IMREAD_COLOR).with_context(|| {
        tr_args(
            "Could not read preview: {path}",
            &[("path", source.display().to_string())],
        )
    })?;
    if image.empty() {
        bail!(tr_args(
            "Preview contains no image data: {path}",
            &[("path", source.display().to_string())],
        ));
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
        bail!(tr("Could not save the preview file"));
    }
    Ok((path, directory))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencv::core::{CV_8UC3, Scalar};

    #[test]
    #[ignore = "requires a GTK display; run with --ignored --test-threads=1"]
    fn zoom_buttons_resize_the_visible_review_and_preview() {
        adw::init().unwrap();
        install_theme();
        let application = adw::Application::builder()
            .application_id("de.martin.PhotoScanner.ZoomTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(None::<&gio::Cancellable>).unwrap();
        let ui = build_ui(&application);
        gtk::prelude::WidgetExt::realize(&ui.window);
        let directory = TempDir::new().unwrap();
        let source_path = directory.path().join("scan.png");
        let mut source =
            Mat::new_rows_cols_with_default(800, 1200, CV_8UC3, Scalar::all(255.0)).unwrap();
        imgproc::rectangle(
            &mut source,
            core::Rect::new(100, 100, 600, 450),
            Scalar::new(40.0, 80.0, 140.0, 0.0),
            -1,
            imgproc::LINE_8,
            0,
        )
        .unwrap();
        assert!(imgcodecs::imwrite_def(&source_path, &source).unwrap());
        let review = prepare_review(
            std::slice::from_ref(&source_path),
            directory.path(),
            &SplitConfig::default(),
            &ScannerCancellation::new(),
            None,
        )
        .unwrap();
        show_review(&ui, review);
        assert!(ui.zoom_in_action.is_enabled(), "review must support zoom");
        let card = ui.review_flow.first_child().unwrap();
        let original = card.measure(gtk::Orientation::Vertical, -1).0;
        ui.zoom_in_action.activate(None);
        assert!(card.measure(gtk::Orientation::Vertical, -1).0 > original);
        ui.zoom_out_action.activate(None);
        assert_eq!(card.measure(gtk::Orientation::Vertical, -1).0, original);
        ui.zoom_in_action.activate(None);
        ui.zoom_fit_action.activate(None);
        assert_eq!(card.measure(gtk::Orientation::Vertical, -1).0, original);
        for _ in 0..12 {
            ui.zoom_in_action.activate(None);
        }
        assert!(!ui.zoom_in_action.is_enabled());
        for _ in 0..20 {
            ui.zoom_out_action.activate(None);
        }
        assert!(!ui.zoom_out_action.is_enabled());
        ui.zoom_fit_action.activate(None);
        assert_eq!(card.measure(gtk::Orientation::Vertical, -1).0, original);
        assert_eq!(
            ui.review_state.borrow().as_ref().unwrap().selections.len(),
            1
        );

        drop_review(&ui);
        ui.picture
            .set_file(Some(&gio::File::for_path(&source_path)));
        show_previous_preview(&ui);
        set_preview_zoom(&ui, 0.5);
        assert_eq!(ui.picture.measure(gtk::Orientation::Horizontal, -1).0, 600);
        assert_eq!(ui.picture.measure(gtk::Orientation::Vertical, -1).0, 400);
        ui.zoom_out_action.activate(None);
        assert_eq!(ui.picture.measure(gtk::Orientation::Horizontal, -1).0, 480);
        ui.zoom_in_action.activate(None);
        assert_eq!(ui.picture.measure(gtk::Orientation::Horizontal, -1).0, 600);
        ui.zoom_fit_action.activate(None);
        assert_eq!(ui.picture.width_request(), -1);
        assert_eq!(ui.picture.height_request(), -1);
        ui.sender.close();
        ui.window.destroy();
    }

    #[test]
    fn picture_zoom_starts_from_the_fitted_size() {
        let image = (3200, 2400);
        let viewport = (640, 480);
        assert!((next_picture_zoom(0.0, 1.25, image, viewport) - 0.25).abs() < 1e-9);
        assert!((next_picture_zoom(0.0, 0.8, image, viewport) - 0.16).abs() < 1e-9);
        assert!((next_picture_zoom(0.5, 1.25, image, viewport) - 0.625).abs() < 1e-9);
    }

    #[test]
    fn bounded_preview_limits_the_largest_edge() {
        let source_directory = TempDir::new().unwrap();
        let source = source_directory.path().join("large.png");
        let image = Mat::new_rows_cols_with_default(
            2000,
            4000,
            CV_8UC3,
            Scalar::new(40.0, 80.0, 120.0, 0.0),
        )
        .unwrap();
        assert!(imgcodecs::imwrite_def(&source, &image).unwrap());

        let (preview, owner) = bounded_preview(&source).unwrap();
        assert!(preview.starts_with(owner.path()));
        let image = imgcodecs::imread(&preview, imgcodecs::IMREAD_COLOR).unwrap();
        assert_eq!(image.cols().max(image.rows()), PREVIEW_MAX_EDGE);
        assert_eq!((image.cols(), image.rows()), (3200, 1600));
    }

    #[test]
    fn error_summary_uses_first_line_and_limits_length() {
        assert_eq!(error_summary("Short\nTechnical details"), "Short");
        let long = "x".repeat(140);
        let summary = error_summary(&long);
        assert_eq!(summary.chars().count(), 120);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn quarter_turn_rotation_changes_dimensions() {
        let image =
            Mat::new_rows_cols_with_default(40, 90, CV_8UC3, Scalar::new(20.0, 80.0, 160.0, 0.0))
                .unwrap();

        let clockwise = rotate_quarter_turns(&image, 1).unwrap();
        let upside_down = rotate_quarter_turns(&image, 2).unwrap();

        assert_eq!((clockwise.cols(), clockwise.rows()), (40, 90));
        assert_eq!((upside_down.cols(), upside_down.rows()), (90, 40));
    }

    #[test]
    fn scan_status_switches_to_the_follow_up_phase() {
        assert_eq!(status_after_scan(ScanMode::Full), tr("Saving scan…"));
        assert_eq!(
            status_after_scan(ScanMode::Direct),
            tr("Analyzing scan file…")
        );
        assert_eq!(
            status_after_scan(ScanMode::Review),
            tr("Analyzing scan file…")
        );
    }

    #[test]
    fn saved_review_preview_uses_the_exported_photo() {
        let output = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let photo_path = staging.path().join("photo.png");
        let photo =
            Mat::new_rows_cols_with_default(80, 120, CV_8UC3, Scalar::new(0.0, 0.0, 255.0, 0.0))
                .unwrap();
        assert!(imgcodecs::imwrite_def(&photo_path, &photo).unwrap());

        let source =
            Mat::new_rows_cols_with_default(160, 200, CV_8UC3, Scalar::new(80.0, 80.0, 80.0, 0.0))
                .unwrap();
        let region = DetectedRegion {
            center: core::Point2f::new(100.0, 80.0),
            source_box: [
                core::Point2f::new(20.0, 20.0),
                core::Point2f::new(180.0, 20.0),
                core::Point2f::new(180.0, 140.0),
                core::Point2f::new(20.0, 140.0),
            ],
            area_percent: 60.0,
        };
        let review = ReviewData {
            staging,
            photos: vec![ReviewPhotoData {
                full_path: photo_path.clone(),
                thumbnail_path: photo_path,
                group_index: 0,
                region: region.clone(),
            }],
            groups: vec![ReviewGroup {
                analyzed: AnalyzedScan {
                    image: source,
                    regions: vec![region],
                    threshold: 12.0,
                    embedded_dpi: None,
                },
            }],
            overview: PathBuf::new(),
            config: SplitConfig {
                output_format: OutputFormat::Png,
                capture_date: NaiveDate::from_ymd_opt(1995, 9, 1),
                ..SplitConfig::default()
            },
            output: output.path().join("saved"),
            failures: Vec::new(),
        };

        let result = save_review_work(review, &[(true, 0)], &ScannerCancellation::new()).unwrap();
        let preview = imgcodecs::imread(&result.preview, imgcodecs::IMREAD_COLOR).unwrap();
        let center = preview
            .at_2d::<core::Vec3b>(preview.rows() / 2, preview.cols() / 2)
            .unwrap();

        assert!(center[2] > 220 && center[1] < 30 && center[0] < 30);
    }

    #[test]
    fn review_overview_contains_every_source() {
        let directory = TempDir::new().unwrap();
        let red_path = directory.path().join("red.png");
        let green_path = directory.path().join("green.png");
        let red =
            Mat::new_rows_cols_with_default(90, 160, CV_8UC3, Scalar::new(0.0, 0.0, 255.0, 0.0))
                .unwrap();
        let green =
            Mat::new_rows_cols_with_default(90, 160, CV_8UC3, Scalar::new(0.0, 255.0, 0.0, 0.0))
                .unwrap();
        assert!(imgcodecs::imwrite_def(&red_path, &red).unwrap());
        assert!(imgcodecs::imwrite_def(&green_path, &green).unwrap());

        let overview = compose_image_sheet(&[red_path, green_path], directory.path()).unwrap();
        let image = imgcodecs::imread(&overview, imgcodecs::IMREAD_COLOR).unwrap();
        let left = image.at_2d::<core::Vec3b>(450, 300).unwrap();
        let right = image.at_2d::<core::Vec3b>(450, 900).unwrap();

        assert_eq!((image.cols(), image.rows()), (1200, 900));
        assert!(left[2] > 220 && left[1] < 30);
        assert!(right[1] > 220 && right[2] < 30);
    }
}
