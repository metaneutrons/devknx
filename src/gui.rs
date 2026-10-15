// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use devknx::control::RestStatus;
use devknx::ets::{CsvEncoding, EtsFormat};
use devknx::ipc::WireState;
use devknx::paths;
use devknx::storage::CaptureSide;
use eframe::egui;
use knx_rs_ip::discovery::GatewayInfo;

use crate::color::{self, Tone};
use crate::ets_import::{self, EtsImportOutcome, EtsImportPreview};
use crate::interface::{
    self, ConnectionMode, ConnectionSettings, DisplayCapture, Follower, MonitorModel,
};
use crate::platform::{self, MenuAction};

const fn sent_color(dark_mode: bool) -> egui::Color32 {
    if dark_mode {
        egui::Color32::from_rgb(105, 205, 240)
    } else {
        egui::Color32::from_rgb(0, 95, 150)
    }
}

const fn warning_color(dark_mode: bool) -> egui::Color32 {
    if dark_mode {
        egui::Color32::from_rgb(255, 200, 90)
    } else {
        egui::Color32::from_rgb(145, 90, 0)
    }
}

const fn loss_color(dark_mode: bool) -> egui::Color32 {
    if dark_mode {
        egui::Color32::from_rgb(255, 110, 110)
    } else {
        egui::Color32::from_rgb(175, 35, 35)
    }
}

pub fn run(
    database: Option<PathBuf>,
    smoke: bool,
    smoke_live: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let smoke_result = (smoke || smoke_live).then(|| Arc::new(AtomicBool::new(false)));
    let app_smoke_result = smoke_result.clone();
    let icon =
        image::load_from_memory(include_bytes!("../resources/png/devknx-256.png"))?.to_rgba8();
    let (width, height) = icon.dimensions();
    let options = eframe::NativeOptions {
        #[cfg(target_os = "windows")]
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_app_id("devknx")
            .with_title("devknx")
            .with_icon(egui::IconData {
                rgba: icon.into_raw(),
                width,
                height,
            })
            .with_inner_size([1_100.0, 700.0])
            .with_min_inner_size([650.0, 420.0]),
        ..Default::default()
    };
    // Not in the automated smoke runs, which nobody answers.
    let skill_offer = if smoke {
        None
    } else {
        devknx::agent_skill::Offer::for_this_user(&<crate::Cli as clap::CommandFactory>::command())
    };
    // Held while the window is open, so the daemon does not stop under it.
    let _lease = devknx::lifecycle::LeaseKeeper::start(devknx::lifecycle::LeaseKind::Gui);
    eframe::run_native(
        "devknx",
        options,
        Box::new(move |_cc| {
            platform::init_app();
            let mut app = MonitorApp::new(database, app_smoke_result, smoke_live);
            app.skill_offer = skill_offer;
            Ok(Box::new(app))
        }),
    )?;
    if let Some(result) = smoke_result
        && !result.load(Ordering::Acquire)
    {
        return Err("GUI smoke: native window or live-capture qualification failed".into());
    }
    Ok(())
}

/// The same connection editor is shown on the opening screen and in Settings.
fn render_connection_form(
    ui: &mut egui::Ui,
    settings: &mut ConnectionSettings,
    id_salt: &str,
) -> bool {
    ui.push_id(id_salt, |ui| {
        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.label(egui::RichText::new("Connection mode").strong());
            egui::ComboBox::from_id_salt("connection-mode")
                .selected_text(match settings.mode {
                    ConnectionMode::Tunnel => "Tunneling",
                    ConnectionMode::Routing => "Routing (multicast)",
                })
                .width(245.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut settings.mode, ConnectionMode::Tunnel, "Tunneling");
                    ui.selectable_value(
                        &mut settings.mode,
                        ConnectionMode::Routing,
                        "Routing (multicast)",
                    );
                });
            ui.add_space(8.0);

            let (address_label, address_hint, help) = match settings.mode {
                ConnectionMode::Tunnel => (
                    "Gateway IP address",
                    "Enter gateway IP address",
                    "Enter a unicast address (for example, 192.168.1.10). Discovery is optional.",
                ),
                ConnectionMode::Routing => (
                    "Multicast group",
                    "Enter multicast group",
                    "Enter a reachable multicast group (usually 224.0.23.12).",
                ),
            };
            ui.label(egui::RichText::new(address_label).strong());
            ui.add(
                egui::TextEdit::singleline(&mut settings.address)
                    .hint_text(address_hint)
                    .desired_width(300.0),
            );
            ui.add_space(8.0);
            ui.label(egui::RichText::new("UDP port").strong());
            ui.add(egui::DragValue::new(&mut settings.port).range(1..=u16::MAX));
            ui.small(help);
            ui.add_space(8.0);
            if settings.mode == ConnectionMode::Tunnel {
                let discover = ui.button("Discover gateways…").clicked();
                ui.small("Discovery uses multicast; manual tunneling does not require it.");
                discover
            } else {
                false
            }
        })
        .inner
    })
    .inner
}

/// Whether Cmd+F (Ctrl+F off macOS) was pressed, without Shift or Alt.
fn filter_shortcut_pressed(input: &egui::InputState) -> bool {
    input.modifiers.command
        && !input.modifiers.shift
        && !input.modifiers.alt
        && input.key_pressed(egui::Key::F)
}

/// Give a text field the cursor and select what is in it, so that typing
/// replaces the old text, as in the find field of other applications.
fn focus_text_field(ctx: &egui::Context, id: egui::Id, text: &str) {
    ctx.memory_mut(|memory| memory.request_focus(id));
    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
        let end = text.chars().count();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(end),
            )));
        state.store(ctx, id);
    }
}

fn rest_indicator(status: Option<&RestStatus>, error: Option<&str>) -> String {
    if error.is_some() {
        return "REST unavailable".into();
    }
    match status {
        Some(RestStatus {
            enabled: true,
            bind: Some(bind),
            ..
        }) => format!("REST :{}", bind.port()),
        Some(RestStatus { enabled: true, .. }) => "REST on".into(),
        Some(_) => "REST off".into(),
        None => "REST checking…".into(),
    }
}

struct SmokeRun {
    started: Instant,
    frames: u32,
    result: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug)]
enum LiveSmokePhase {
    Stream { baseline_id: i64 },
    Scroll { first_id: i64 },
    Disconnect { last_id: i64 },
    Reconnect { last_id: i64 },
}

struct LiveSmoke {
    started: Instant,
    last_reported: Instant,
    phase: LiveSmokePhase,
    pin_to_top: bool,
    result: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
enum RestRequestKind {
    Status,
    Change,
}

struct EtsImportDialog {
    open: bool,
    file: String,
    format: EtsFormat,
    encoding: CsvEncoding,
    preview: Option<EtsImportPreview>,
    prepare_receiver: Option<Receiver<Result<EtsImportPreview, String>>>,
    commit_receiver: Option<Receiver<Result<EtsImportOutcome, String>>>,
    error: Option<String>,
}

impl Default for EtsImportDialog {
    fn default() -> Self {
        Self {
            open: false,
            file: String::new(),
            format: EtsFormat::Csv31,
            encoding: CsvEncoding::Utf8,
            preview: None,
            prepare_receiver: None,
            commit_receiver: None,
            error: None,
        }
    }
}

fn poll_import<T>(receiver: &mut Option<Receiver<Result<T, String>>>) -> Option<Result<T, String>> {
    let result = match receiver.as_ref()?.try_recv() {
        Ok(result) => result,
        Err(mpsc::TryRecvError::Empty) => return None,
        Err(mpsc::TryRecvError::Disconnected) => {
            Err("ETS import worker stopped unexpectedly".into())
        }
    };
    *receiver = None;
    Some(result)
}

fn render_ets_options(ui: &mut egui::Ui, dialog: &mut EtsImportDialog, preparing: bool) {
    ui.add_enabled_ui(!preparing, |ui| {
        ui.horizontal(|ui| {
            ui.label("ETS export");
            if ui
                .add(egui::TextEdit::singleline(&mut dialog.file).desired_width(320.0))
                .changed()
            {
                dialog.error = None;
            }
            if ui.button("Choose…").clicked()
                && let Some(file) = rfd::FileDialog::new()
                    .set_title("Choose ETS Group Address Export")
                    .add_filter("ETS exports", &["csv", "xml"])
                    .pick_file()
            {
                if file
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("xml"))
                {
                    dialog.format = EtsFormat::GaXml01;
                } else {
                    dialog.format = EtsFormat::Csv31;
                }
                dialog.file = file.display().to_string();
                dialog.error = None;
            }
        });
        ui.horizontal(|ui| {
            ui.label("Format");
            ui.selectable_value(&mut dialog.format, EtsFormat::Csv31, "ETS CSV (3/1)");
            ui.selectable_value(&mut dialog.format, EtsFormat::GaXml01, "GA Export 01 XML");
        });
        if dialog.format == EtsFormat::Csv31 {
            ui.horizontal(|ui| {
                ui.label("Encoding");
                ui.selectable_value(&mut dialog.encoding, CsvEncoding::Utf8, "UTF-8");
                ui.selectable_value(
                    &mut dialog.encoding,
                    CsvEncoding::Latin1,
                    "Latin-1 (legacy)",
                );
            });
        } else {
            dialog.encoding = CsvEncoding::Utf8;
            ui.small("XML must use UTF-8.");
        }
    });
}

/// The question on quit, with what stopping the daemon would end.
struct QuitQuestion {
    view: interface::DaemonView,
    remember: bool,
}

/// The question before the stored capture is cleared.
struct ClearCaptureQuestion {
    /// The capture it is about, which may no longer be the one on screen when
    /// the answer comes.
    database: PathBuf,
    /// Telegrams and loss reports it holds, or why that could not be read.
    counts: Result<(u64, u64), String>,
    /// Copy it to the archive folder first. On unless turned off.
    archive: bool,
}

/// The question before the telegrams on one side of a telegram are deleted.
struct DeleteAroundQuestion {
    /// The capture it is about, which may no longer be the one on screen when
    /// the answer comes.
    database: PathBuf,
    /// Which telegrams, around which one.
    span: devknx::storage::CaptureSpan,
    /// The telegram that stays, as the row shows it.
    line: String,
    /// Telegrams and loss reports the span holds, or why that could not be read.
    counts: Result<(u64, u64), String>,
    /// Copy the capture to the archive folder first. On unless turned off.
    archive: bool,
}

#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent visible GUI dialogs"
)]
struct MonitorApp {
    /// Talks to the daemon when quitting; `None` asks nothing, as in tests.
    quit_daemon: Option<interface::DaemonControl>,
    /// Where the answer to the quit question is remembered.
    settings_file: Option<PathBuf>,
    quit_question: Option<QuitQuestion>,
    /// Counts and clears a capture through the daemon; `None` in tests.
    capture_control: Option<interface::CaptureControl>,
    clear_capture: Option<ClearCaptureQuestion>,
    delete_around: Option<DeleteAroundQuestion>,
    /// Quitting is decided; the next close goes through.
    quit_resolved: bool,
    /// The agent skill offered at start, until it is answered.
    skill_offer: Option<devknx::agent_skill::Offer>,
    /// What answering the offer did, until it is dismissed.
    skill_result: Option<String>,
    ets_import: EtsImportDialog,
    primary_database: Option<PathBuf>,
    database_override: Option<PathBuf>,
    offline_capture: bool,
    model: Option<MonitorModel>,
    follower: Option<Follower>,
    gateway_receiver: Option<Receiver<Result<Vec<GatewayInfo>, String>>>,
    gateways: Vec<GatewayInfo>,
    action_receiver: Option<Receiver<Result<devknx::ipc::IpcMessage, String>>>,
    connection_receiver: Option<Receiver<Result<String, String>>>,
    rest_receiver: Option<Receiver<Result<RestStatus, String>>>,
    rest_request_kind: Option<RestRequestKind>,
    rest_status: Option<RestStatus>,
    /// Shown in the status bar while the daemon is another build.
    build_warning: Option<String>,
    build_receiver: Option<Receiver<Option<String>>>,
    rest_error: Option<String>,
    rest_action_error: Option<String>,
    rest_last_refresh: Option<Instant>,
    rest_bind: String,
    rest_token: String,
    rest_allow_remote_writes: bool,
    settings: ConnectionSettings,
    settings_draft: ConnectionSettings,
    error: Option<String>,
    show_settings: bool,
    show_rest: bool,
    show_gateways: bool,
    show_export: bool,
    show_read: bool,
    show_write: bool,
    export_path: String,
    read_address: String,
    write_address: String,
    write_dpt: String,
    write_value: String,
    preview: Option<(String, String, String, String)>,
    selected: Option<DisplayCapture>,
    color_enabled: bool,
    window_title: String,
    smoke: Option<SmokeRun>,
    live_smoke: Option<LiveSmoke>,
}

impl MonitorApp {
    fn new(
        database: Option<PathBuf>,
        smoke_result: Option<Arc<AtomicBool>>,
        smoke_live: bool,
    ) -> Self {
        let mut app = Self {
            color_enabled: color::ui_enabled(),
            rest_bind: "127.0.0.1:8765".into(),
            smoke: smoke_result
                .clone()
                .filter(|_| !smoke_live)
                .map(|result| SmokeRun {
                    started: Instant::now(),
                    frames: 0,
                    result,
                }),
            live_smoke: smoke_result.filter(|_| smoke_live).map(|result| LiveSmoke {
                started: Instant::now(),
                last_reported: Instant::now(),
                phase: LiveSmokePhase::Stream { baseline_id: 0 },
                pin_to_top: false,
                result,
            }),
            // Tests never reach the daemon of the user running them.
            #[cfg(not(test))]
            quit_daemon: Some(interface::daemon_control()),
            #[cfg(not(test))]
            capture_control: Some(interface::capture_control()),
            #[cfg(not(test))]
            settings_file: devknx::settings::Settings::path().ok(),
            ..Self::default()
        };
        if let Some(database) = database {
            app.database_override = Some(database.clone());
            app.primary_database = Some(database.clone());
            match interface::ensure_database(&database) {
                Ok(()) => app.attach(&database, false),
                Err(error) => app.error = Some(error),
            }
        } else if app.smoke.is_none() {
            match interface::load_recent_settings() {
                Ok(settings) => {
                    app.settings = settings.clone();
                    app.settings_draft = settings.clone();
                    if let Ok(endpoint) = app.settings.endpoint()
                        && let Ok(database) = paths::database_for_endpoint(&endpoint)
                        && database.exists()
                    {
                        app.primary_database = Some(database.clone());
                        app.attach(&database, false);
                        app.settings = settings.clone();
                        app.settings_draft = settings;
                    }
                }
                Err(error) => {
                    app.error = Some(format!("Connection settings could not be loaded: {error}"));
                }
            }
        }
        if let (Some(smoke), Some(model)) = (&mut app.live_smoke, &app.model) {
            smoke.phase = LiveSmokePhase::Stream {
                baseline_id: model.rows.last().and_then(|row| row.id).unwrap_or(0),
            };
        }
        app.refresh_rest_status();
        let (sender, receiver) = mpsc::channel();
        app.build_receiver = Some(receiver);
        std::thread::spawn(move || {
            let _ = sender.send(interface::build_warning());
        });
        app
    }

    fn attach(&mut self, database: &Path, offline: bool) {
        if self.ets_import.commit_receiver.is_some() {
            self.error = Some("Wait for the ETS import to finish before changing captures".into());
            return;
        }
        let same_primary = self.primary_database.as_ref().is_some_and(|primary| {
            primary == database
                || matches!(
                    (primary.canonicalize(), database.canonicalize()),
                    (Ok(left), Ok(right)) if left == right
                )
        });
        let offline = offline && !same_primary;
        match MonitorModel::open(database.to_path_buf()) {
            Ok(model) => {
                self.ets_import = EtsImportDialog::default();
                self.follower = (!offline).then(|| Follower::start(database.to_path_buf()));
                self.model = Some(model);
                self.offline_capture = offline;
                self.error = None;
                self.selected = None;
                if !offline {
                    match interface::load_settings(database) {
                        Ok(settings) => {
                            self.settings_draft = settings.clone();
                            self.settings = settings;
                        }
                        Err(error) => {
                            self.error =
                                Some(format!("Connection settings could not be loaded: {error}"));
                        }
                    }
                }
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn open_capture_dialog(&mut self) {
        if self.ets_import.commit_receiver.is_some() {
            return;
        }
        let mut dialog = rfd::FileDialog::new()
            .set_title("Open KNX Capture")
            .add_filter("SQLite capture", &["sqlite", "sqlite3", "db"]);
        let directory = self
            .primary_database
            .as_ref()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .or_else(|| paths::data_dir().ok().map(|path| path.join("captures")));
        if let Some(parent) = directory {
            dialog = dialog.set_directory(parent);
        }
        if let Some(database) = dialog.pick_file() {
            self.attach(&database, true);
        }
    }

    fn discover(&mut self) {
        self.show_gateways = true;
        let (sender, receiver) = mpsc::channel();
        self.gateway_receiver = Some(receiver);
        std::thread::spawn(move || {
            let _ = sender.send(interface::discover_gateways());
        });
    }

    fn connect(&mut self) {
        if self.offline_capture {
            self.error = Some("Return to Live Capture before connecting to a gateway".into());
            return;
        }
        if self.connection_receiver.is_some() || self.ets_import.commit_receiver.is_some() {
            return;
        }
        let endpoint = match self.settings.endpoint() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.rest_action_error = Some(error);
                return;
            }
        };
        let settings = self.settings.clone();
        let database = match self.select_connection(&settings, &endpoint) {
            Ok(database) => database,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let (sender, receiver) = mpsc::channel();
        self.connection_receiver = Some(receiver);
        self.error = None;
        std::thread::spawn(move || {
            let _ = sender.send(interface::connect_owner(&database, &endpoint));
        });
    }

    fn select_connection(
        &mut self,
        settings: &ConnectionSettings,
        endpoint: &str,
    ) -> Result<PathBuf, String> {
        if self.offline_capture {
            return Err("Return to Live Capture before changing connection settings".into());
        }
        if self.ets_import.commit_receiver.is_some() {
            return Err(
                "Wait for the ETS import to finish before changing connection settings".into(),
            );
        }
        let database = match &self.database_override {
            Some(path) => path.clone(),
            None => paths::database_for_endpoint(endpoint)?,
        };
        if self.connection_receiver.is_some() {
            return Err("Wait for the current connection operation to finish".into());
        }
        if self.action_receiver.is_some() {
            return Err(
                "Wait for the current operation to finish before changing connection settings"
                    .into(),
            );
        }
        if self
            .model
            .as_ref()
            .is_some_and(|model| model.owner_available && settings != &self.settings)
        {
            return Err("Disconnect before changing connection settings".into());
        }
        let replacement = if self.primary_database.as_deref() != Some(database.as_path())
            || self.model.is_none()
        {
            interface::ensure_database(&database)?;
            Some(MonitorModel::open(database.clone())?)
        } else {
            None
        };
        interface::save_settings(&database, settings)?;
        if self.database_override.is_none() {
            interface::save_recent_settings(settings)?;
        }
        if let Some(model) = replacement {
            self.ets_import = EtsImportDialog::default();
            self.follower = Some(Follower::start(database.clone()));
            self.model = Some(model);
            self.primary_database = Some(database.clone());
            self.offline_capture = false;
            self.selected = None;
        }
        self.settings = settings.clone();
        self.settings_draft = settings.clone();
        Ok(database)
    }

    fn disconnect(&mut self) {
        if self.offline_capture {
            return;
        }
        let Some(model) = &self.model else { return };
        if self.connection_receiver.is_some() {
            return;
        }
        let database = model.database.clone();
        let (sender, receiver) = mpsc::channel();
        self.connection_receiver = Some(receiver);
        std::thread::spawn(move || {
            let _ = sender.send(interface::disconnect_owner(&database));
        });
    }

    fn refresh_rest_status(&mut self) {
        if self.rest_receiver.is_some() {
            return;
        }
        self.rest_last_refresh = Some(Instant::now());
        let (sender, receiver) = mpsc::channel();
        self.rest_receiver = Some(receiver);
        self.rest_request_kind = Some(RestRequestKind::Status);
        std::thread::spawn(move || {
            let _ = sender.send(interface::rest_status());
        });
    }

    fn enable_rest(&mut self) {
        if self.rest_receiver.is_some() || self.offline_capture {
            return;
        }
        let endpoint = self.settings.endpoint().ok();
        let bind = self.rest_bind.trim().to_owned();
        let database = endpoint.as_ref().and_then(|_| {
            self.primary_database
                .clone()
                .or_else(|| self.database_override.clone())
        });
        let token_text = std::mem::take(&mut self.rest_token);
        let token = (!token_text.is_empty()).then_some(token_text);
        let allow_remote_writes = self.rest_allow_remote_writes;
        let (sender, receiver) = mpsc::channel();
        self.rest_receiver = Some(receiver);
        self.rest_request_kind = Some(RestRequestKind::Change);
        self.rest_action_error = None;
        std::thread::spawn(move || {
            let _ = sender.send(interface::rest_enable(
                endpoint.as_deref(),
                database,
                &bind,
                token,
                allow_remote_writes,
            ));
        });
    }

    fn disable_rest(&mut self) {
        if self.rest_receiver.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        self.rest_receiver = Some(receiver);
        self.rest_request_kind = Some(RestRequestKind::Change);
        self.rest_action_error = None;
        std::thread::spawn(move || {
            let _ = sender.send(interface::rest_disable());
        });
    }

    fn operate(&mut self, request: devknx::operations::OperationRequest) {
        let Some(model) = &self.model else { return };
        let database = model.database.clone();
        let (sender, receiver) = mpsc::channel();
        self.action_receiver = Some(receiver);
        std::thread::spawn(move || {
            let _ = sender.send(interface::operate(&database, &request));
        });
    }

    fn process_rest_background(&mut self) {
        if let Some(receiver) = &self.rest_receiver {
            match receiver.try_recv() {
                Ok(Ok(status)) => {
                    if status.enabled
                        && let Some(bind) = status.bind
                    {
                        self.rest_bind = bind.to_string();
                    }
                    if status.enabled {
                        self.rest_allow_remote_writes = status.allow_remote_writes;
                    }
                    self.rest_status = Some(status);
                    self.rest_error = None;
                    if matches!(self.rest_request_kind.take(), Some(RestRequestKind::Change)) {
                        self.rest_action_error = None;
                    }
                    self.rest_receiver = None;
                }
                Ok(Err(error)) => {
                    match self.rest_request_kind.take() {
                        Some(RestRequestKind::Change) => self.rest_action_error = Some(error),
                        Some(RestRequestKind::Status) | None => self.rest_error = Some(error),
                    }
                    self.rest_receiver = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    let error = "REST control worker stopped unexpectedly".into();
                    match self.rest_request_kind.take() {
                        Some(RestRequestKind::Change) => self.rest_action_error = Some(error),
                        Some(RestRequestKind::Status) | None => self.rest_error = Some(error),
                    }
                    self.rest_receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.rest_receiver.is_none()
            && self
                .rest_last_refresh
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(3))
        {
            self.refresh_rest_status();
        }
    }

    /// Take the daemon's build comparison once the background check is done.
    fn process_build_warning(&mut self) {
        if let Some(receiver) = &self.build_receiver
            && let Ok(warning) = receiver.try_recv()
        {
            self.build_warning = warning;
            self.build_receiver = None;
        }
    }

    fn process_background(&mut self) {
        self.process_rest_background();
        self.process_build_warning();
        self.process_ets_import();
        if self.model.is_none()
            && !self.offline_capture
            && let Ok(endpoint) = self.settings.endpoint()
            && let Ok(database) = self
                .database_override
                .clone()
                .map_or_else(|| paths::database_for_endpoint(&endpoint), Ok)
            && database.exists()
        {
            match interface::save_settings(&database, &self.settings) {
                Ok(()) => {
                    self.primary_database = Some(database.clone());
                    self.attach(&database, false);
                }
                Err(error) => {
                    self.error = Some(format!("Could not save connection settings: {error}"));
                }
            }
        }
        if let Some(receiver) = &self.gateway_receiver {
            match receiver.try_recv() {
                Ok(Ok(gateways)) => {
                    self.gateways = gateways;
                    self.gateway_receiver = None;
                }
                Ok(Err(error)) => {
                    self.error = Some(
                        if error.contains("No route to host")
                            || error.contains("Network is unreachable")
                        {
                            "Gateway discovery is unavailable on this network. Enter the gateway IP address manually.".into()
                        } else {
                            format!(
                                "Gateway discovery failed: {error}. You can enter an IP address manually."
                            )
                        },
                    );
                    self.gateway_receiver = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.error = Some("Gateway discovery stopped unexpectedly".into());
                    self.gateway_receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(receiver) = &self.connection_receiver {
            match receiver.try_recv() {
                Ok(Ok(notice)) => {
                    if let Some(model) = &mut self.model {
                        model.notice(notice);
                    }
                    self.connection_receiver = None;
                    self.error = None;
                }
                Ok(Err(error)) => {
                    self.error = Some(error);
                    self.connection_receiver = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.error = Some("Connection worker stopped unexpectedly".into());
                    self.connection_receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let (Some(model), Some(follower)) = (&mut self.model, &self.follower) {
            for event in follower.receiver.try_iter().take(500) {
                match event {
                    Ok(message) => model.ingest(message),
                    Err(error) => model.owner_unavailable(&error),
                }
            }
        }
        if let Some(receiver) = &self.action_receiver {
            match receiver.try_recv() {
                Ok(Ok(message)) => {
                    if let Some(model) = &mut self.model {
                        model.ingest(message);
                    }
                    self.action_receiver = None;
                }
                Ok(Err(error)) => {
                    self.error = Some(error);
                    self.action_receiver = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.error = Some("Operation worker stopped unexpectedly".into());
                    self.action_receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    fn prepare_ets_import(&mut self) {
        let Some(model) = &self.model else { return };
        let database = model.database.clone();
        let file = PathBuf::from(&self.ets_import.file);
        let format = self.ets_import.format;
        let encoding = self.ets_import.encoding;
        let (sender, receiver) = mpsc::channel();
        self.ets_import.prepare_receiver = Some(receiver);
        self.ets_import.preview = None;
        self.ets_import.error = None;
        std::thread::spawn(move || {
            let _ = sender.send(ets_import::prepare(database, file, format, encoding));
        });
    }

    fn confirm_ets_import(&mut self) {
        if self.connection_receiver.is_some() || self.action_receiver.is_some() {
            self.ets_import.error = Some("Wait for the current operation to finish".into());
            return;
        }
        let Some(preview) = self.ets_import.preview.as_ref() else {
            return;
        };
        if !self
            .model
            .as_ref()
            .is_some_and(|model| model.database == preview.database && !model.owner_available)
        {
            self.ets_import.error =
                Some("Disconnect the session using this capture before importing".into());
            return;
        }
        let preview = self.ets_import.preview.take().expect("checked preview");
        let (sender, receiver) = mpsc::channel();
        self.ets_import.commit_receiver = Some(receiver);
        self.ets_import.error = None;
        std::thread::spawn(move || {
            let _ = sender.send(ets_import::commit(preview));
        });
    }

    fn process_ets_import(&mut self) {
        if let Some(result) = poll_import(&mut self.ets_import.prepare_receiver) {
            match result {
                Ok(preview)
                    if self.ets_import.open
                        && self
                            .model
                            .as_ref()
                            .is_some_and(|model| model.database == preview.database) =>
                {
                    self.ets_import.preview = Some(preview);
                }
                Ok(_) => {
                    self.ets_import.error =
                        Some("Capture changed; create a new import preview".into());
                }
                Err(error) => self.ets_import.error = Some(error),
            }
        }
        if let Some(result) = poll_import(&mut self.ets_import.commit_receiver) {
            match result {
                Ok(outcome) => {
                    if let Some(model) = &mut self.model
                        && model.database == outcome.database
                    {
                        model.notice(outcome.notice());
                        if let Err(error) = model.refresh_ets() {
                            self.error =
                                Some(format!("Import saved, but display refresh failed: {error}"));
                        }
                        if let Some(selected) = &self.selected {
                            self.selected =
                                model.rows.iter().find(|row| row.id == selected.id).cloned();
                        }
                    }
                    self.ets_import.open = false;
                }
                Err(error) => self.ets_import.error = Some(error),
            }
        }
    }

    fn render_ets_import(&mut self, ctx: &egui::Context) {
        if !self.ets_import.open {
            return;
        }
        let busy = self.ets_import.commit_receiver.is_some();
        let preparing = self.ets_import.prepare_receiver.is_some();
        let mut open = true;
        let mut prepare = false;
        let mut confirm = false;
        let mut disconnect = false;
        let mut cancel = false;
        egui::Window::new("Import ETS Group Addresses")
            .open(&mut open)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .default_width(550.0)
            .default_height(260.0)
            .max_height((ctx.content_rect().height() - 24.0).max(240.0))
            .vscroll(true)
            .show(ctx, |ui| {
                let Some(model) = &self.model else {
                    ui.label("Save connection settings or open a capture first to select the import destination.");
                    return;
                };
                ui.label("Destination capture");
                ui.monospace(model.database.display().to_string());
                ui.small("Replaces the active ETS catalogue only. Raw captures and previous revisions are preserved.");
                ui.separator();
                if busy {
                    ui.spinner();
                    ui.label("Importing ETS group addresses…");
                } else if let Some(preview) = &self.ets_import.preview {
                    ui.label(format!("Source: {}", preview.file.display()));
                    ui.strong(preview.summary());
                    for line in preview.sample_lines() { ui.monospace(line); }
                    ui.small("Only declared, unambiguous DPTs decode values. Standard four-column CSV supplies names but no DPTs.");
                    if model.owner_available {
                        ui.label("Disconnect this capture session before importing. The daemon and REST stay running.");
                        disconnect = ui.add_enabled(self.connection_receiver.is_none(), egui::Button::new("Disconnect session")).clicked();
                    }
                    ui.horizontal(|ui| {
                        confirm = ui.add_enabled(!model.owner_available && self.connection_receiver.is_none() && self.action_receiver.is_none(), egui::Button::new("Replace ETS catalogue")).clicked();
                        if ui.button("Back").clicked() { self.ets_import.preview = None; }
                        cancel = ui.button("Cancel").clicked();
                    });
                } else {
                    render_ets_options(ui, &mut self.ets_import, preparing);
                    ui.horizontal(|ui| {
                        prepare = ui.add_enabled(!preparing && !self.ets_import.file.trim().is_empty(), egui::Button::new("Preview import")).clicked();
                        cancel = ui.button("Cancel").clicked();
                        if preparing { ui.spinner(); ui.label("Validating export…"); }
                    });
                }
                if let Some(error) = &self.ets_import.error { ui.colored_label(loss_color(ui.visuals().dark_mode), error); }
            });
        if (!open || cancel) && !busy {
            self.ets_import = EtsImportDialog::default();
        } else if prepare {
            self.prepare_ets_import();
        } else if confirm {
            self.confirm_ets_import();
        } else if disconnect {
            self.disconnect();
        }
    }

    fn process_menu(&mut self) -> bool {
        if platform::take(MenuAction::OpenDatabase) {
            self.open_capture_dialog();
        }
        if platform::take(MenuAction::ExportCsv) {
            self.show_export = true;
        }
        if platform::take(MenuAction::ImportEts) {
            self.ets_import.open = true;
        }
        if platform::take(MenuAction::Discover) {
            self.discover();
        }
        if platform::take(MenuAction::ConnectionSettings) {
            self.settings_draft = self.settings.clone();
            self.show_settings = true;
        }
        if platform::take(MenuAction::ToggleColor) && !color::ui_is_locked() {
            self.color_enabled = !self.color_enabled;
            self.save_color_preference();
        }
        if platform::take(MenuAction::ToggleConnection) {
            if self
                .model
                .as_ref()
                .is_some_and(|model| model.owner_available)
            {
                self.disconnect();
            } else {
                self.connect();
            }
        }
        if platform::take(MenuAction::Read) {
            self.show_read = !self.offline_capture
                && self
                    .model
                    .as_ref()
                    .is_some_and(|model| matches!(model.state, WireState::Connected { .. }));
        }
        if platform::take(MenuAction::Write) {
            self.show_write = !self.offline_capture
                && self
                    .model
                    .as_ref()
                    .is_some_and(|model| matches!(model.state, WireState::Connected { .. }));
        }
        platform::take(MenuAction::FocusFilter)
    }

    fn smoke_step(&mut self, ctx: &egui::Context) {
        let Some(smoke) = &mut self.smoke else { return };
        if smoke.result.load(Ordering::Acquire) {
            return;
        }
        smoke.frames += 1;
        if smoke.frames == 1 {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(800.0, 520.0)));
        } else {
            let size = ctx.input(|input| input.content_rect().size());
            if smoke.frames >= 2
                && (size.x - 800.0).abs() <= 30.0
                && (size.y - 520.0).abs() <= 30.0
                && platform::menu_installed()
            {
                eprintln!("GUI smoke: window, resize and native menu verified");
                smoke.result.store(true, Ordering::Release);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        if smoke.started.elapsed() > Duration::from_secs(12) {
            let size = ctx.input(|input| input.content_rect().size());
            eprintln!(
                "GUI smoke: timed out after {} frames (size={size:?}, menu={})",
                smoke.frames,
                platform::menu_installed()
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn live_smoke_step(&mut self, ctx: &egui::Context, scroll_offset: Option<f32>) {
        let Some(smoke) = &mut self.live_smoke else {
            return;
        };
        if smoke.result.load(Ordering::Acquire) {
            return;
        }
        if smoke.started.elapsed() > Duration::from_secs(45) {
            eprintln!("GUI live smoke: timed out");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let Some(model) = &self.model else {
            return;
        };
        let ordered = model.rows.iter().all(|row| row.id.is_some())
            && model.rows.windows(2).all(|pair| pair[0].id < pair[1].id);
        if !ordered {
            eprintln!("GUI live smoke: capture IDs are missing, repeated or unordered");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let last_id = model.rows.last().and_then(|row| row.id).unwrap_or(0);
        if smoke.last_reported.elapsed() >= Duration::from_secs(5) {
            eprintln!(
                "GUI live diagnostic: phase={:?} state={:?} rows={} last_id={last_id} scroll={scroll_offset:?}",
                smoke.phase,
                model.state,
                model.rows.len()
            );
            smoke.last_reported = Instant::now();
        }
        match smoke.phase {
            LiveSmokePhase::Stream { baseline_id }
                if matches!(model.state, WireState::Connected { .. })
                    && last_id >= baseline_id + 100
                    && scroll_offset.is_some_and(|offset| offset > 100.0) =>
            {
                smoke.pin_to_top = true;
                smoke.phase = LiveSmokePhase::Scroll { first_id: last_id };
            }
            LiveSmokePhase::Scroll { first_id }
                if matches!(model.state, WireState::Connected { .. })
                    && last_id >= first_id + 50
                    && scroll_offset.is_some_and(|offset| offset <= 2.0) =>
            {
                eprintln!("GUI live smoke: streaming");
                smoke.phase = LiveSmokePhase::Disconnect { last_id };
            }
            LiveSmokePhase::Disconnect {
                last_id: previous_id,
            } if matches!(
                &model.state,
                WireState::WaitingRetry { reason, .. }
                    if reason == "capture owner unavailable"
            ) && last_id >= previous_id
                && scroll_offset.is_some_and(|offset| offset <= 2.0) =>
            {
                eprintln!("GUI live smoke: disconnected");
                smoke.phase = LiveSmokePhase::Reconnect {
                    last_id: previous_id,
                };
            }
            LiveSmokePhase::Reconnect {
                last_id: previous_id,
            } if matches!(model.state, WireState::Connected { .. })
                // The loopback tunnel server still retries ACKs to the forcibly
                // killed first owner, so recovery checks several new frames,
                // not the pre-disconnect throughput threshold.
                && last_id >= previous_id + 5
                && scroll_offset.is_some_and(|offset| offset <= 2.0) =>
            {
                eprintln!("GUI live smoke: recovered");
                smoke.result.store(true, Ordering::Release);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            _ => {}
        }
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    /// Answer the agent skill offer: install (`Some(true)`), never offer these
    /// places again (`Some(false)`), or ask next time (`None`).
    fn answer_skill_offer(&mut self, answer: Option<bool>) {
        let Some(offer) = self.skill_offer.take() else {
            return;
        };
        self.skill_result = match answer {
            Some(true) => Some(offer.accept().join("\n")),
            Some(false) => Some(offer.decline().map_or_else(
                |error| format!("Could not save the answer: {error}"),
                |()| "Not offered again. `devknx skill --install` installs it later.".to_owned(),
            )),
            None => None,
        };
    }

    /// The agent skill offer, and then what answering it did.
    /// Closing the window or quitting from the menu: decide what happens to
    /// the daemon first. The smoke runs close themselves and nobody answers
    /// there, so they are let through.
    fn process_quit(&mut self, ctx: &egui::Context) {
        let close = ctx.input(|input| input.viewport().close_requested());
        let menu_quit = platform::take(MenuAction::Quit);
        if self.quit_resolved || self.smoke.is_some() || self.live_smoke.is_some() {
            if menu_quit {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            return;
        }
        if !close && !menu_quit {
            return;
        }
        if close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self.quit_question.is_none() && self.begin_quit() {
            self.quit_resolved = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Returns whether the window may close now; otherwise the question is up.
    fn begin_quit(&mut self) -> bool {
        let Some(control) = &self.quit_daemon else {
            return true;
        };
        match interface::plan_quit(control, self.settings_file.as_deref()) {
            interface::QuitPlan::Quit => true,
            interface::QuitPlan::Ask(view) => {
                self.quit_question = Some(QuitQuestion {
                    view,
                    remember: false,
                });
                false
            }
        }
    }

    /// Act on the answer; `None` cancels the quit.
    fn answer_quit(&mut self, ctx: &egui::Context, stop: Option<bool>) {
        let Some(question) = self.quit_question.take() else {
            return;
        };
        let Some(stop) = stop else {
            return;
        };
        if let Some(control) = &self.quit_daemon
            && let Err(error) = interface::answer_quit(
                control,
                self.settings_file.as_deref(),
                stop,
                question.remember,
            )
        {
            eprintln!("{error}");
        }
        self.quit_resolved = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn quit_window(&mut self, ctx: &egui::Context) {
        let Some(question) = &mut self.quit_question else {
            return;
        };
        let mut answer = None;
        egui::Window::new("Quit devknx")
            .resizable(false)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Stop the daemon too?").strong());
                ui.add_space(6.0);
                for line in question
                    .view
                    .what_stopping_ends(devknx::lifecycle::LeaseKind::Gui)
                {
                    ui.label(line);
                }
                ui.add_space(8.0);
                ui.checkbox(&mut question.remember, "Remember my answer");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Stop the daemon").clicked() {
                        answer = Some(Some(true));
                    }
                    if ui.button("Leave it running").clicked() {
                        answer = Some(Some(false));
                    }
                    if ui.button("Cancel").clicked() {
                        answer = Some(None);
                    }
                });
            });
        if let Some(answer) = answer {
            self.answer_quit(ctx, answer);
        }
    }

    fn skill_offer_window(&mut self, ctx: &egui::Context) {
        if let Some(result) = &self.skill_result {
            let mut dismissed = false;
            egui::Window::new("AI tools")
                .resizable(false)
                .collapsible(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(result);
                    ui.add_space(8.0);
                    dismissed = ui.button("OK").clicked();
                });
            if dismissed {
                self.skill_result = None;
            }
            return;
        }
        let Some(offer) = &self.skill_offer else {
            return;
        };
        let mut answer = None;
        egui::Window::new("AI tools")
            .resizable(false)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(egui::RichText::new(devknx::agent_skill::Offer::QUESTION).strong());
                ui.add_space(6.0);
                for line in offer.lines() {
                    ui.label(line);
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Install").clicked() {
                        answer = Some(Some(true));
                    }
                    if ui.button("Not here").clicked() {
                        answer = Some(Some(false));
                    }
                    if ui.button("Later").clicked() {
                        answer = Some(None);
                    }
                });
            });
        if let Some(answer) = answer {
            self.answer_skill_offer(answer);
        }
    }

    #[expect(clippy::too_many_lines, reason = "four independent small modal forms")]
    fn dialogs(&mut self, ctx: &egui::Context) {
        self.skill_offer_window(ctx);
        self.quit_window(ctx);
        self.clear_capture_window(ctx);
        self.delete_around_window(ctx);
        if self.show_settings {
            let mut open = self.show_settings;
            let mut save = false;
            let mut connect = false;
            let mut discover = false;
            egui::Window::new("Connection Settings")
                .open(&mut open)
                .resizable(false)
                .max_height((ctx.content_rect().height() - 24.0).max(240.0))
                .vscroll(true)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .default_width(400.0)
                .show(ctx, |ui| {
                    discover = render_connection_form(ui, &mut self.settings_draft, "settings");
                    ui.separator();
                    ui.label("Capture storage");
                    ui.small("History is saved automatically for this connection.");
                    ui.small("Use Open… to choose another capture in the system file dialog.");
                    ui.separator();
                    ui.horizontal(|ui| {
                        save = ui.button("Save settings").clicked();
                        connect = ui
                            .add_enabled(
                                !self.offline_capture
                                    && !self
                                        .model
                                        .as_ref()
                                        .is_some_and(|model| model.owner_available)
                                    && self.connection_receiver.is_none(),
                                egui::Button::new("Save and connect"),
                            )
                            .clicked();
                    });
                });
            self.show_settings = open;
            if discover {
                self.discover();
            }
            if save || connect {
                match self.settings_draft.endpoint() {
                    Ok(endpoint) => {
                        match self.select_connection(&self.settings_draft.clone(), &endpoint) {
                            Ok(_) => {
                                self.error = None;
                                self.show_settings = false;
                                if connect {
                                    self.connect();
                                }
                            }
                            Err(error) => {
                                self.error =
                                    Some(format!("Could not save connection settings: {error}"));
                            }
                        }
                    }
                    Err(error) => self.error = Some(error),
                }
            }
        }
        if self.show_rest {
            let mut open = self.show_rest;
            let mut refresh = false;
            let mut enable = false;
            let mut disable = false;
            egui::Window::new("REST API")
                .open(&mut open)
                .resizable(false)
                .max_height((ctx.content_rect().height() - 24.0).max(240.0))
                .vscroll(true)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .default_width(400.0)
                .show(ctx, |ui| {
                    if let Some(error) = &self.rest_action_error {
                        ui.colored_label(egui::Color32::LIGHT_RED, format!("REST request failed: {error}"));
                    }
                    if let Some(error) = &self.rest_error {
                        ui.colored_label(egui::Color32::LIGHT_RED, format!("Status unavailable: {error}"));
                    } else if let Some(status) = &self.rest_status {
                        if status.enabled {
                            ui.label(format!(
                                "Listening at http://{}/v1",
                                status.bind.map_or_else(
                                    || "unknown address".into(),
                                    |bind| bind.to_string()
                                )
                            ));
                            ui.label(format!(
                                "Selected endpoint: {}",
                                status.endpoint.as_deref().unwrap_or("none; supply endpoint per request")
                            ));
                            ui.small(if status.allow_remote_writes {
                                "Remote typed writes enabled"
                            } else {
                                "Remote typed writes disabled"
                            });
                        } else {
                            ui.label("REST is off");
                        }
                    } else {
                        ui.label("Checking REST status…");
                    }
                    ui.small("REST starts without a KNX connection. Use /v1/sessions to list, connect, and disconnect endpoints.");
                    ui.small("If an endpoint is selected here, /v1/connection and endpoint-free data requests use it as the default.");
                    let target_endpoint = self.settings.endpoint();
                    if let Ok(endpoint) = &target_endpoint {
                        ui.small(format!("Selected endpoint: {endpoint}"));
                    }
                    ui.separator();
                    let active = self.rest_status.as_ref().is_some_and(|status| status.enabled);
                    ui.add_enabled_ui(!active, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Listen address");
                            ui.text_edit_singleline(&mut self.rest_bind);
                        });
                        ui.horizontal(|ui| {
                            ui.label("Bearer token");
                            ui.add(egui::TextEdit::singleline(&mut self.rest_token).password(true));
                        });
                        ui.checkbox(&mut self.rest_allow_remote_writes, "Allow remote typed writes");
                    });
                    ui.small("Required for non-loopback binding (at least 32 printable ASCII characters). Not saved.");
                    if active {
                        ui.small("Stop REST before changing its settings.");
                    }
                    ui.small("REST uses plain HTTP. Expose it only on a trusted network or behind a TLS proxy.");
                    let status_known = self.rest_status.is_some() && self.rest_error.is_none();
                    if self.offline_capture && !active {
                        ui.small("Return to Live Capture before starting REST.");
                    }
                    ui.horizontal(|ui| {
                        enable = ui.add_enabled(
                            !self.offline_capture && status_known && !active && self.rest_receiver.is_none(),
                            egui::Button::new("Start REST"),
                        ).clicked();
                        disable = ui.add_enabled(
                            status_known && active && self.rest_receiver.is_none(),
                            egui::Button::new("Stop REST"),
                        ).clicked();
                        refresh = ui.add_enabled(
                            self.rest_receiver.is_none(),
                            egui::Button::new("Refresh status"),
                        ).clicked();
                    });
                });
            self.show_rest = open;
            if refresh {
                self.refresh_rest_status();
            }
            if enable {
                self.enable_rest();
            }
            if disable {
                self.disable_rest();
            }
        }
        if self.show_gateways {
            let mut open = self.show_gateways;
            let mut chosen = None;
            let mut refresh = false;
            egui::Window::new("KNXnet/IP Gateways")
                .open(&mut open)
                .default_width(420.0)
                .show(ctx, |ui| {
                    if self.gateway_receiver.is_some() {
                        ui.horizontal(|ui| { ui.spinner(); ui.label("Searching the local network…"); });
                    } else if self.gateways.is_empty() {
                        ui.label("No gateways found. You can enter the IP address manually in Connection Settings.");
                    }
                    for gateway in &self.gateways {
                        if ui.button(format!("{}  ·  {}", gateway.name, gateway.address)).clicked() {
                            chosen = Some(gateway.address);
                        }
                    }
                    refresh = ui.button("Search again").clicked();
                });
            self.show_gateways = open;
            if refresh {
                self.discover();
            }
            if let Some(address) = chosen {
                self.settings_draft.mode = ConnectionMode::Tunnel;
                self.settings_draft.address = address.ip().to_string();
                self.settings_draft.port = address.port();
                self.show_gateways = false;
                self.show_settings = true;
            }
        }
        if self.show_export {
            let mut open = self.show_export;
            let mut export = false;
            egui::Window::new("Export CSV")
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label("New file path; an existing file is never replaced");
                    ui.text_edit_singleline(&mut self.export_path);
                    export = ui
                        .add_enabled(
                            self.model.is_some(),
                            egui::Button::new("Export full history"),
                        )
                        .clicked();
                });
            self.show_export = open;
            if export && let Some(model) = &mut self.model {
                match interface::export_csv(
                    &model.database,
                    &PathBuf::from(self.export_path.trim()),
                ) {
                    Ok(count) => {
                        model.notice(format!("Exported {count} captures to {}", self.export_path));
                        self.show_export = false;
                    }
                    Err(error) => self.error = Some(error),
                }
            }
        }
        if self.show_read {
            let mut open = self.show_read;
            let mut read = false;
            egui::Window::new("Read Group Value")
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label("Group address");
                    ui.text_edit_singleline(&mut self.read_address);
                    read = ui
                        .add_enabled(
                            !self.offline_capture
                                && self.model.as_ref().is_some_and(|model| {
                                    matches!(model.state, WireState::Connected { .. })
                                })
                                && self.action_receiver.is_none(),
                            egui::Button::new("Send read request"),
                        )
                        .clicked();
                });
            self.show_read = open;
            if read {
                match interface::read_request(&self.read_address) {
                    Ok(request) => self.operate(request),
                    Err(error) => self.error = Some(error),
                }
            }
        }
        if self.show_write {
            let mut open = self.show_write;
            let mut preview = false;
            let mut send = false;
            egui::Window::new("Prepared Group Write")
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label("Group address");
                    ui.text_edit_singleline(&mut self.write_address);
                    if let Some(model) = &self.model
                        && let Ok(Some((name, dpts))) = model.ets_for_address(&self.write_address)
                    {
                        ui.label(format!("ETS: {name} · {}", dpts.join(", ")));
                    }
                    ui.label("DPT (empty: use unambiguous ETS declaration)");
                    ui.text_edit_singleline(&mut self.write_dpt);
                    ui.label("Typed value");
                    ui.text_edit_singleline(&mut self.write_value);
                    preview = ui.button("Prepare and preview").clicked();
                    if let Some((address, dpt, value, frame)) = &self.preview {
                        if *address == self.write_address
                            && *dpt == self.write_dpt
                            && *value == self.write_value
                        {
                            ui.monospace(frame);
                            send = ui
                                .add_enabled(
                                    !self.offline_capture
                                        && self.model.as_ref().is_some_and(|model| {
                                            matches!(model.state, WireState::Connected { .. })
                                        })
                                        && self.action_receiver.is_none(),
                                    egui::Button::new("Transmit prepared typed write"),
                                )
                                .clicked();
                        } else {
                            ui.label("Inputs changed; prepare again before sending.");
                        }
                    }
                });
            self.show_write = open;
            if preview && let Some(model) = &self.model {
                match model.preview_write(&self.write_address, &self.write_dpt, &self.write_value) {
                    Ok(frame) => {
                        self.preview = Some((
                            self.write_address.clone(),
                            self.write_dpt.clone(),
                            self.write_value.clone(),
                            frame,
                        ));
                        self.error = None;
                    }
                    Err(error) => {
                        self.preview = None;
                        self.error = Some(error);
                    }
                }
            }
            if send {
                match interface::write_request(
                    &self.write_address,
                    &self.write_dpt,
                    &self.write_value,
                ) {
                    Ok(request) => {
                        self.operate(request);
                        self.preview = None;
                    }
                    Err(error) => self.error = Some(error),
                }
            }
        }
    }

    fn connection_status(&self) -> (egui::Color32, &'static str, String) {
        let Some(model) = &self.model else {
            return (
                egui::Color32::GRAY,
                "Disconnected",
                "Choose a KNXnet/IP connection to begin capture".into(),
            );
        };
        if self.offline_capture {
            return (
                egui::Color32::GRAY,
                "Offline capture",
                model.database.file_name().map_or_else(
                    || model.database.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                ),
            );
        }
        if !model.owner_available {
            return (
                egui::Color32::GRAY,
                "Disconnected",
                "No capture connection is active".into(),
            );
        }
        match &model.state {
            WireState::Idle => (
                egui::Color32::YELLOW,
                "Starting",
                "Preparing KNXnet/IP connection".into(),
            ),
            WireState::Connecting { .. } => (
                egui::Color32::YELLOW,
                "Connecting",
                "Opening KNXnet/IP connection".into(),
            ),
            WireState::Connected { endpoint } => {
                (egui::Color32::GREEN, "Connected", endpoint.clone())
            }
            WireState::WaitingRetry { reason, .. } => {
                (egui::Color32::YELLOW, "Retrying", reason.clone())
            }
            WireState::Stopped => (
                egui::Color32::GRAY,
                "Disconnected",
                "KNXnet/IP connection disconnected".into(),
            ),
            WireState::StorageFailed { reason } => {
                (egui::Color32::RED, "Storage error", reason.clone())
            }
        }
    }

    fn render_toolbar(&mut self, ui: &mut egui::Ui, focus_filter: bool) {
        let (color, status, detail) = self.connection_status();
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(color, format!("● {status}"))
                .on_hover_text(detail);
            let owner_available = self
                .model
                .as_ref()
                .is_some_and(|model| model.owner_available);
            if self.offline_capture {
                if ui
                    .add_enabled(
                        self.ets_import.commit_receiver.is_none(),
                        egui::Button::new("Live capture"),
                    )
                    .clicked()
                {
                    if let Some(database) = self.primary_database.clone() {
                        self.attach(&database, false);
                    } else {
                        self.model = None;
                        self.follower = None;
                        self.offline_capture = false;
                    }
                }
            } else if ui
                .add_enabled(
                    self.connection_receiver.is_none() && self.ets_import.commit_receiver.is_none(),
                    egui::Button::new(if owner_available {
                        "Disconnect"
                    } else {
                        "Connect"
                    }),
                )
                .clicked()
            {
                if owner_available {
                    self.disconnect();
                } else {
                    self.connect();
                }
            }
            ui.separator();
            if ui
                .add_enabled(!self.offline_capture, egui::Button::new("Settings…"))
                .clicked()
            {
                self.settings_draft = self.settings.clone();
                self.show_settings = true;
            }
            ui.separator();
            let connected = !self.offline_capture
                && self
                    .model
                    .as_ref()
                    .is_some_and(|model| matches!(model.state, WireState::Connected { .. }));
            if ui
                .add_enabled(connected, egui::Button::new("Read…"))
                .clicked()
            {
                self.show_read = true;
            }
            if ui
                .add_enabled(connected, egui::Button::new("Write…"))
                .clicked()
            {
                self.show_write = true;
            }
            self.render_capture_actions(ui);
            ui.separator();
            self.render_color_toggle(ui);
            ui.separator();
            // Keep the label and editor together when the toolbar wraps.
            if ui.available_size_before_wrap().x < 240.0 {
                ui.end_row();
            }
            ui.label("Filter");
            if let Some(model) = &mut self.model {
                let id = egui::Id::new("capture-filter");
                if focus_filter {
                    focus_text_field(ui.ctx(), id, &model.filter);
                }
                ui.add(
                    egui::TextEdit::singleline(&mut model.filter)
                        .id(id)
                        .desired_width(170.0),
                );
            }
        });
    }

    fn render_color_toggle(&mut self, ui: &mut egui::Ui) {
        let toggle = ui.add_enabled(
            !color::ui_is_locked(),
            egui::Checkbox::new(&mut self.color_enabled, "Color"),
        );
        if toggle.changed() {
            self.save_color_preference();
        }
        toggle.on_hover_text("Color changes display only, not captures or exports");
    }

    fn render_capture_actions(&mut self, ui: &mut egui::Ui) {
        let busy = self.ets_import.commit_receiver.is_some();
        if ui
            .add_enabled(!busy, egui::Button::new("Open…"))
            .on_hover_text("Open a saved capture file")
            .clicked()
        {
            self.open_capture_dialog();
        }
        if ui
            .add_enabled(
                self.model.is_some() && !busy,
                egui::Button::new("Import ETS…"),
            )
            .on_hover_text("Preview and import ETS group-address CSV or XML")
            .clicked()
        {
            self.ets_import.open = true;
        }
        if ui
            .add_enabled(self.model.is_some(), egui::Button::new("Export…"))
            .clicked()
        {
            self.show_export = true;
        }
        let recorded = !self.offline_capture
            && self
                .model
                .as_ref()
                .is_some_and(|model| model.owner_available);
        if ui
            .add_enabled(recorded, egui::Button::new("Clear…"))
            .on_hover_text("Delete the stored capture, after asking")
            .clicked()
        {
            self.open_clear_capture();
        }
    }

    /// Count what the capture holds and open the question before it is cleared.
    fn open_clear_capture(&mut self) {
        use interface::{CaptureAnswer, CaptureRequest};
        let Some(database) = self.model.as_ref().map(|model| model.database.clone()) else {
            return;
        };
        let Some(control) = self.capture_control.clone() else {
            self.error = Some("No daemon is behind this window, so nothing can be cleared".into());
            return;
        };
        let counts = match control(&database, CaptureRequest::Counts) {
            Ok(CaptureAnswer::Counts { events, losses }) => Ok((events, losses)),
            Ok(_) => Err("unexpected answer from the daemon".into()),
            Err(error) => Err(error),
        };
        self.clear_capture = Some(ClearCaptureQuestion {
            database,
            counts,
            archive: true,
        });
    }

    /// Count the telegrams on one side of a telegram and open the question
    /// before they are deleted.
    fn open_delete_around(&mut self, side: CaptureSide, id: i64, destination: &str) {
        use interface::{CaptureAnswer, CaptureRequest};
        let Some(model) = self.model.as_ref() else {
            return;
        };
        let database = model.database.clone();
        // A deletion below a telegram stops at the newest one this window has
        // seen, so telegrams that arrive while the question is open are not
        // taken.
        let span = devknx::storage::CaptureSpan {
            side,
            line: id,
            through: (side == CaptureSide::Below).then(|| model.newest_id()),
        };
        let Some(control) = self.capture_control.clone() else {
            self.error = Some("No daemon is behind this window, so nothing can be deleted".into());
            return;
        };
        let counts = match control(&database, CaptureRequest::CountAround { span }) {
            Ok(CaptureAnswer::SpanCounts { events, losses }) => Ok((events, losses)),
            Ok(_) => Err("unexpected answer from the daemon".into()),
            Err(error) => Err(error),
        };
        self.delete_around = Some(DeleteAroundQuestion {
            database,
            span,
            line: format!("{id} ({destination})"),
            counts,
            archive: true,
        });
    }

    /// Close the question without deleting anything.
    fn cancel_delete_around(&mut self) {
        self.delete_around = None;
    }

    /// Delete the telegrams and say what happened. A failure is shown and what
    /// is loaded stays.
    fn confirm_delete_around(&mut self) {
        use interface::{CaptureAnswer, CaptureRequest};
        let Some(question) = self.delete_around.take() else {
            return;
        };
        let Some(control) = self.capture_control.clone() else {
            return;
        };
        match control(
            &question.database,
            CaptureRequest::DeleteAround {
                span: question.span,
                archive: question.archive,
            },
        ) {
            Ok(CaptureAnswer::Deleted {
                events,
                losses,
                archive,
            }) => {
                let copy = archive
                    .as_ref()
                    .map_or_else(String::new, |path| format!(", copy in {}", path.display()));
                let side = match question.span.side {
                    CaptureSide::Above => "above",
                    CaptureSide::Below => "below",
                };
                let message = format!(
                    "Deleted {events} telegrams and {losses} loss reports {side} telegram {}{copy}",
                    question.span.line
                );
                if let Some(model) = self
                    .model
                    .as_mut()
                    .filter(|model| model.database == question.database)
                {
                    model.deleted_around(question.span, message);
                }
            }
            Ok(_) => {
                self.error = Some("Deleting telegrams: unexpected answer from the daemon".into());
            }
            Err(error) => self.error = Some(format!("Deleting telegrams failed: {error}")),
        }
    }

    /// The question before telegrams are deleted around a telegram. Cancel has
    /// the focus, so Enter deletes nothing; deleting is a click on its own
    /// button.
    fn delete_around_window(&mut self, ctx: &egui::Context) {
        let Some(question) = self.delete_around.as_mut() else {
            return;
        };
        let mut confirm = false;
        let mut cancel = ctx.input(|input| input.key_pressed(egui::Key::Escape));
        let deletable = matches!(question.counts, Ok((events, losses)) if events + losses > 0);
        let side = match question.span.side {
            CaptureSide::Above => "above",
            CaptureSide::Below => "below",
        };
        egui::Window::new("Delete telegrams")
            .resizable(false)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                match &question.counts {
                    Ok((0, 0)) => {
                        ui.label(format!("There is nothing {side} this telegram."));
                    }
                    Ok((events, losses)) => {
                        ui.label(
                            egui::RichText::new(format!(
                                "Delete {events} telegrams and {losses} loss reports {side} telegram {}?",
                                question.line
                            ))
                            .strong(),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            "The telegram itself stays. The capture is the record of what \
                             the bus carried; once deleted it can only come back from the \
                             copy. The imported group addresses and the log of what was \
                             sent from here stay, including the entry for a telegram that \
                             goes.",
                        );
                    }
                    Err(error) => {
                        ui.colored_label(
                            egui::Color32::LIGHT_RED,
                            format!("The telegrams could not be counted: {error}"),
                        );
                    }
                }
                ui.add_space(8.0);
                ui.add_enabled(
                    deletable,
                    egui::Checkbox::new(
                        &mut question.archive,
                        "Copy the capture to the archive folder first (recommended)",
                    ),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let cancel_button = ui.button("Cancel");
                    if !ui.memory(|memory| memory.focused().is_some()) {
                        cancel_button.request_focus();
                    }
                    if cancel_button.clicked() {
                        cancel = true;
                    }
                    let delete = egui::Button::new(
                        egui::RichText::new("Delete telegrams").color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(180, 50, 50));
                    if ui.add_enabled(deletable, delete).clicked() {
                        confirm = true;
                    }
                });
            });
        if cancel {
            self.cancel_delete_around();
        } else if confirm {
            self.confirm_delete_around();
        }
    }

    /// Close the question without deleting anything.
    fn cancel_clear_capture(&mut self) {
        self.clear_capture = None;
    }

    /// Clear the capture and say what happened. A failure is shown and what is
    /// loaded stays.
    fn confirm_clear_capture(&mut self) {
        use interface::{CaptureAnswer, CaptureRequest};
        let Some(question) = self.clear_capture.take() else {
            return;
        };
        let Some(control) = self.capture_control.clone() else {
            return;
        };
        match control(
            &question.database,
            CaptureRequest::Clear {
                archive: question.archive,
            },
        ) {
            Ok(CaptureAnswer::Cleared {
                events,
                losses,
                archive,
            }) => {
                let copy = archive
                    .as_ref()
                    .map_or_else(String::new, |path| format!(", copy in {}", path.display()));
                let message = format!("Cleared {events} telegrams and {losses} loss reports{copy}");
                if let Some(model) = self
                    .model
                    .as_mut()
                    .filter(|model| model.database == question.database)
                {
                    model.cleared(message);
                }
            }
            Ok(_) => {
                self.error = Some("Clearing the capture: unexpected answer from the daemon".into());
            }
            Err(error) => self.error = Some(format!("Clearing the capture failed: {error}")),
        }
    }

    /// The question before the capture is cleared. Cancel has the focus, so
    /// Enter deletes nothing; deleting is a click on its own button.
    fn clear_capture_window(&mut self, ctx: &egui::Context) {
        let Some(question) = self.clear_capture.as_mut() else {
            return;
        };
        let mut confirm = false;
        let mut cancel = ctx.input(|input| input.key_pressed(egui::Key::Escape));
        let deletable = matches!(question.counts, Ok((events, losses)) if events + losses > 0);
        egui::Window::new("Clear capture")
            .resizable(false)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                match &question.counts {
                    Ok((0, 0)) => {
                        ui.label("The capture is empty.");
                    }
                    Ok((events, losses)) => {
                        ui.label(
                            egui::RichText::new(format!(
                                "Delete {events} telegrams and {losses} loss reports?"
                            ))
                            .strong(),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            "The capture is the record of what the bus carried. Once \
                             deleted it can only come back from the copy. The imported \
                             group addresses and the log of what was sent from here stay.",
                        );
                    }
                    Err(error) => {
                        ui.colored_label(
                            egui::Color32::LIGHT_RED,
                            format!("The capture could not be read: {error}"),
                        );
                    }
                }
                ui.add_space(8.0);
                ui.add_enabled(
                    deletable,
                    egui::Checkbox::new(
                        &mut question.archive,
                        "Copy it to the archive folder first (recommended)",
                    ),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let cancel_button = ui.button("Cancel");
                    if !ui.memory(|memory| memory.focused().is_some()) {
                        cancel_button.request_focus();
                    }
                    if cancel_button.clicked() {
                        cancel = true;
                    }
                    let delete = egui::Button::new(
                        egui::RichText::new("Clear capture").color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(180, 50, 50));
                    if ui.add_enabled(deletable, delete).clicked() {
                        confirm = true;
                    }
                });
            });
        if cancel {
            self.cancel_clear_capture();
        } else if confirm {
            self.confirm_clear_capture();
        }
    }

    fn save_color_preference(&mut self) {
        if let Err(error) = color::save_preference(self.color_enabled) {
            self.error = Some(format!("Could not save color preference: {error}"));
        }
    }

    fn render_empty(&mut self, ui: &mut egui::Ui) {
        ui.add_space((ui.available_height() * 0.18).min(110.0));
        ui.vertical_centered(|ui| {
            ui.heading("Connect to KNXnet/IP");
            ui.label("Choose tunneling or multicast routing. Captures are saved automatically.");
            ui.add_space(12.0);
            let mut discover = false;
            let mut connect = false;
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 420.0).max(0.0) / 2.0);
                egui::Frame::group(ui.style())
                    .inner_margin(16.0)
                    .show(ui, |ui| {
                        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                            ui.set_width(380.0);
                            discover = render_connection_form(ui, &mut self.settings, "start");
                            ui.add_space(8.0);
                            connect = ui
                                .add_enabled(
                                    self.connection_receiver.is_none(),
                                    egui::Button::new("Connect"),
                                )
                                .clicked();
                        });
                    });
            });
            if discover {
                self.discover();
            }
            if connect {
                self.connect();
            }
            if let Ok(previous) = paths::legacy_database()
                && previous.exists()
                && self.primary_database.as_deref() != Some(previous.as_path())
                && ui.button("Open previous capture…").clicked()
            {
                self.attach(&previous, true);
            }
        });
    }

    #[expect(
        clippy::too_many_lines,
        reason = "virtualized table and compact layout share the same capture selection"
    )]
    fn render_captures(&mut self, ui: &mut egui::Ui) -> Option<f32> {
        let Some(model) = &mut self.model else {
            return None;
        };
        ui.horizontal(|ui| {
            ui.label(format!("{} buffered captures", model.rows.len()));
            if ui.button("Reload history").clicked()
                && let Err(error) = model.reload_history()
            {
                self.error = Some(error);
            }
            if ui.button("Older history").clicked() {
                match model.load_older() {
                    Ok(0) => model.notice("No older retained captures".into()),
                    Ok(count) => model.notice(format!("Loaded {count} older captures")),
                    Err(error) => self.error = Some(error),
                }
            }
        });
        ui.separator();
        let compact = ui.available_width() < 950.0;
        let heading = if compact {
            format!(
                "{:<12} {:<9} {:<11} {:<14} {:<14} {}",
                "Time", "Direction", "Destination", "Value", "DPT", "ETS group"
            )
        } else {
            format!(
                "{:<12} {:<9} {:<9} {:<11} {:<19} {:<14} {:<14} {}",
                "Time",
                "Direction",
                "Source",
                "Destination",
                "Service",
                "Value",
                "DPT",
                "ETS group"
            )
        };
        ui.horizontal(|ui| {
            ui.add_space(ui.spacing().button_padding.x);
            ui.monospace(heading);
        });
        ui.separator();
        let visible: Vec<_> = model
            .rows
            .iter()
            .filter(|row| row.matches(&model.filter))
            .collect();
        let mut selected = None;
        let mut delete_around: Option<(CaptureSide, i64, String)> = None;
        let can_delete = !self.offline_capture && model.owner_available;
        let pin_to_top = self
            .live_smoke
            .as_ref()
            .is_some_and(|smoke| smoke.pin_to_top);
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt("captures")
            .auto_shrink([false, true])
            .stick_to_bottom(!pin_to_top);
        if pin_to_top {
            scroll = scroll.vertical_scroll_offset(0.0);
        }
        // The virtualizer and every rendered widget must use the same height.
        // Truncation prevents long ETS labels from creating taller rows.
        let row_height = ui.spacing().interact_size.y.max(2.0_f32.mul_add(
            ui.spacing().button_padding.y,
            ui.text_style_height(&egui::TextStyle::Monospace),
        ));
        let output = scroll.show_rows(ui, row_height, visible.len(), |ui, range| {
            for row in &visible[range] {
                let text = if compact {
                    format!(
                        "{:<12} {:<9} {:<11} {:<14} {:<14} {}",
                        interface::format_time(row.timestamp_ms),
                        row.direction,
                        row.destination,
                        row.value_text(),
                        row.dpt_text(),
                        row.label.as_deref().unwrap_or("")
                    )
                } else {
                    format!(
                        "{:<12} {:<9} {:<9} {:<11} {:<19} {:<14} {:<14} {}",
                        interface::format_time(row.timestamp_ms),
                        row.direction,
                        row.source,
                        row.destination,
                        row.service,
                        row.value_text(),
                        row.dpt_text(),
                        row.label.as_deref().unwrap_or("")
                    )
                };
                let is_selected = self
                    .selected
                    .as_ref()
                    .is_some_and(|current| current.id == row.id);
                let rich = egui::RichText::new(text).monospace();
                let rich = if self.color_enabled
                    && !is_selected
                    && color::direction_tone(&row.direction) == Tone::Sent
                {
                    rich.color(sent_color(ui.visuals().dark_mode))
                } else {
                    rich
                };
                let response = ui.add_sized(
                    [ui.available_width(), row_height],
                    egui::Button::selectable(is_selected, (rich, egui::Atom::grow())).truncate(),
                );
                let payload_hint = if row.value.is_some() {
                    "Value interpreted using the declared DPT, or a read request without a payload."
                } else {
                    "Undecoded payload shown as hexadecimal bytes; no DPT was guessed."
                };
                let response = response.on_hover_text(format!(
                    "ETS DPT declarations: {}\n{payload_hint}",
                    if row.dpts.is_empty() {
                        "none".into()
                    } else {
                        row.dpts.join(", ")
                    }
                ));
                if response.clicked() {
                    selected = Some((*row).clone());
                }
                // Only a telegram that is in the capture has an id to delete
                // around, and only a capture a daemon records can be changed.
                if let (Some(id), true) = (row.id, can_delete) {
                    response.context_menu(|ui| {
                        if ui.button("Delete Telegrams Above…").clicked() {
                            delete_around = Some((CaptureSide::Above, id, row.destination.clone()));
                            ui.close();
                        }
                        if ui.button("Delete Telegrams Below…").clicked() {
                            delete_around = Some((CaptureSide::Below, id, row.destination.clone()));
                            ui.close();
                        }
                    });
                }
            }
        });
        if let Some((side, id, destination)) = delete_around {
            self.open_delete_around(side, id, &destination);
        }
        if let Some(row) = selected {
            self.selected = Some(row);
        }
        Some(output.state.offset.y)
    }

    fn render_capture_details(&self, ui: &mut egui::Ui) {
        if let Some(row) = &self.selected {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "{} · {} · {} · {} · {} · DPT [{}] · value {} · raw cEMI {}",
                        row.destination,
                        row.source,
                        row.direction,
                        row.service,
                        row.label.as_deref().unwrap_or("no ETS label"),
                        row.dpts.join(", "),
                        row.value_text(),
                        row.raw_cemi
                    ))
                    .monospace(),
                )
                .wrap(),
            );
        } else {
            ui.small(format!(
                "{} visible · Select a capture for raw cEMI and DPT details",
                self.model.as_ref().map_or(0, |model| model
                    .rows
                    .iter()
                    .filter(|row| row.matches(&model.filter))
                    .count())
            ));
        }
        if let Some(notice) = self.model.as_ref().and_then(|model| model.notices.last()) {
            ui.separator();
            ui.label(notice);
        }
    }

    fn render_status(&mut self, ui: &mut egui::Ui) {
        let (_, status, detail) = self.connection_status();
        let rest_label = rest_indicator(self.rest_status.as_ref(), self.rest_error.as_deref());
        let rest_color = if self.rest_error.is_some() {
            egui::Color32::from_rgb(255, 180, 60)
        } else if self
            .rest_status
            .as_ref()
            .is_some_and(|status| status.enabled)
        {
            egui::Color32::from_rgb(80, 200, 120)
        } else {
            egui::Color32::from_rgb(160, 160, 160)
        };
        let rest_tip = self.rest_error.as_ref().map_or_else(
            || "Show and change the daemon's REST API".to_owned(),
            |error| format!("REST status unavailable: {error}. Click to inspect."),
        );
        let mut open_rest = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{status} · {detail}"));
            if let Some(model) = &self.model {
                ui.separator();
                ui.label(format!("{} captures", model.rows.len()));
                ui.separator();
                let router_label =
                    format!("Router losses (session): {}", model.router_lost_messages);
                if self.color_enabled && model.router_lost_messages > 0 {
                    ui.colored_label(loss_color(ui.visuals().dark_mode), router_label);
                } else {
                    ui.label(router_label);
                }
                ui.separator();
                let lag_label = format!("Local lag (session): {}", model.local_lag_events);
                if self.color_enabled && model.local_lag_events > 0 {
                    ui.colored_label(warning_color(ui.visuals().dark_mode), lag_label);
                } else {
                    ui.label(lag_label);
                }
                ui.separator();
                ui.label("History saved locally")
                    .on_hover_text(model.database.display().to_string());
            }
            if let Some(warning) = &self.build_warning {
                ui.separator();
                ui.colored_label(
                    warning_color(ui.visuals().dark_mode),
                    "Daemon is another build",
                )
                .on_hover_text(warning);
            }
            ui.separator();
            open_rest = ui
                .add(egui::Button::new(egui::RichText::new(rest_label).color(rest_color)).small())
                .on_hover_text(rest_tip)
                .clicked();
        });
        if open_rest {
            if let Some(bind) = self.rest_status.as_ref().and_then(|status| status.bind) {
                self.rest_bind = bind.to_string();
            }
            self.show_rest = true;
            self.refresh_rest_status();
        }
    }
}

impl eframe::App for MonitorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.process_background();
        // Cmd+F: on macOS the menu item gets the key and reports it; off
        // macOS, where there is no menu bar, the shortcut is read here.
        let focus_filter = self.process_menu() || ui.input(filter_shortcut_pressed);
        let ctx = ui.ctx().clone();
        self.process_quit(&ctx);
        self.smoke_step(&ctx);
        ctx.request_repaint_after(Duration::from_millis(100));
        let (_, status, detail) = self.connection_status();
        let title = if status == "Connected" {
            format!("devknx — {detail}")
        } else {
            format!("devknx — {status}")
        };
        if self.window_title != title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.window_title = title;
        }
        egui::Panel::top("capture-toolbar")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_gray(43))
                    .inner_margin(8.0),
            )
            .show(ui, |ui| self.render_toolbar(ui, focus_filter));
        egui::Panel::bottom("capture-status")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_gray(38))
                    .inner_margin(5.0),
            )
            .show(ui, |ui| self.render_status(ui));
        let show_captures = self.model.as_ref().is_some_and(|model| {
            self.offline_capture || !model.rows.is_empty() || model.owner_available
        });
        if show_captures {
            egui::Panel::bottom("capture-details").show(ui, |ui| self.render_capture_details(ui));
        }
        let mut scroll_offset = None;
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(error) = &self.error {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
                ui.separator();
            }
            if show_captures {
                scroll_offset = self.render_captures(ui);
            } else if !self.offline_capture {
                self.render_empty(ui);
            } else {
                ui.label(
                    "Capture storage could not be opened. Use Open… to choose an existing file.",
                );
            }
            if !show_captures
                && let Some(notice) = self.model.as_ref().and_then(|model| model.notices.last())
            {
                ui.separator();
                ui.label(notice);
            }
        });
        self.live_smoke_step(&ctx, scroll_offset);
        self.dialogs(&ctx);
        self.render_ets_import(&ctx);
        platform::update_color_menu_state(self.color_enabled, color::ui_is_locked());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devknx::storage::CaptureStore;
    use std::num::NonZeroU32;

    /// Closing the window with a daemon running asks first; the answer stops
    /// it or leaves it, cancelling keeps the window, and without a daemon the
    /// window closes at once.
    #[test]
    fn quitting_asks_about_the_daemon() {
        use crate::interface::DaemonRequest::{Stop, View};
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("settings.json");
        let view = crate::interface::DaemonView {
            sessions: Vec::new(),
            leases: vec![devknx::lifecycle::LeaseKind::Gui],
            rest: RestStatus {
                enabled: false,
                endpoint: None,
                bind: None,
                allow_remote_writes: false,
            },
        };
        let (control, asked) = crate::interface::fake_daemon_control(Some(view));
        let ctx = egui::Context::default();
        let mut app = MonitorApp {
            quit_daemon: Some(control),
            settings_file: Some(settings.clone()),
            ..MonitorApp::default()
        };

        assert!(!app.begin_quit(), "asking must not close yet");
        let lines = app
            .quit_question
            .as_ref()
            .unwrap()
            .view
            .what_stopping_ends(devknx::lifecycle::LeaseKind::Gui);
        assert_eq!(lines, vec!["No KNX session is configured.".to_owned()]);
        app.answer_quit(&ctx, None);
        assert!(
            app.quit_question.is_none() && !app.quit_resolved,
            "cancel keeps the window"
        );

        assert!(!app.begin_quit());
        app.quit_question.as_mut().unwrap().remember = true;
        app.answer_quit(&ctx, Some(true));
        assert!(app.quit_resolved);
        assert_eq!(*asked.lock().unwrap(), vec![View, View, Stop]);
        assert_eq!(
            devknx::settings::Settings::load_from(&settings)
                .unwrap()
                .on_quit,
            devknx::settings::OnQuit::Stop
        );

        let (control, asked) = crate::interface::fake_daemon_control(None);
        let mut app = MonitorApp {
            quit_daemon: Some(control),
            ..MonitorApp::default()
        };
        assert!(app.begin_quit(), "no daemon, nothing to ask");
        assert_eq!(*asked.lock().unwrap(), vec![View]);
    }

    /// The agent skill offer installs, remembers a no, or asks again next
    /// time.
    #[test]
    fn the_skill_offer_installs_remembers_or_waits() {
        use devknx::agent_skill::{Offer, sandbox_agent};
        let directory = tempfile::tempdir().unwrap();
        let cli = <crate::Cli as clap::CommandFactory>::command();
        let dir = directory.path();
        let mut app = MonitorApp {
            skill_offer: Offer::for_agent(sandbox_agent(&cli, dir, &[".claude"])),
            ..MonitorApp::default()
        };
        app.answer_skill_offer(None);
        assert!(app.skill_offer.is_none() && app.skill_result.is_none());
        let again = Offer::for_agent(sandbox_agent(&cli, dir, &[]));
        assert!(again.is_some(), "later did not ask again");

        app.skill_offer = again;
        app.answer_skill_offer(Some(false));
        assert!(
            app.skill_result
                .as_deref()
                .is_some_and(|text| text.contains("Not offered again")),
            "{:?}",
            app.skill_result
        );
        assert!(Offer::for_agent(sandbox_agent(&cli, dir, &[])).is_none());

        app.skill_offer = Offer::for_agent(sandbox_agent(&cli, dir, &[".codex"]));
        app.answer_skill_offer(Some(true));
        assert!(dir.join("home/.agents/skills/devknx/SKILL.md").exists());
    }

    fn import_app() -> (tempfile::TempDir, MonitorApp, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        drop(CaptureStore::open_for_ets_import(&database).unwrap());
        let file = directory.path().join("groups.csv");
        std::fs::write(&file, "Main;Middle;Sub;Address\nHome;Lights;Desk;1/2/4\n").unwrap();
        let app = MonitorApp {
            model: Some(MonitorModel::open(database).unwrap()),
            ..Default::default()
        };
        (directory, app, file)
    }

    #[test]
    fn canceled_or_wrong_target_preview_cannot_be_activated() {
        let (_directory, mut app, file) = import_app();
        let database = app.model.as_ref().unwrap().database.clone();
        let (sender, receiver) = mpsc::channel();
        app.ets_import.prepare_receiver = Some(receiver);
        sender
            .send(ets_import::prepare(
                database.clone(),
                file.clone(),
                EtsFormat::Csv31,
                CsvEncoding::Utf8,
            ))
            .unwrap();
        // Closing the dialog while its parser runs must not resurrect a preview.
        app.process_ets_import();
        assert!(app.ets_import.preview.is_none());
        assert_eq!(
            CaptureStore::open_existing(&database)
                .unwrap()
                .ets_revision()
                .unwrap(),
            None
        );
        app.ets_import.open = true;
        let other = database.with_file_name("other.sqlite");
        drop(CaptureStore::open_for_ets_import(&other).unwrap());
        let (sender, receiver) = mpsc::channel();
        app.ets_import.prepare_receiver = Some(receiver);
        sender
            .send(ets_import::prepare(
                other,
                file,
                EtsFormat::Csv31,
                CsvEncoding::Utf8,
            ))
            .unwrap();
        app.process_ets_import();
        assert!(app.ets_import.preview.is_none());
        assert!(
            app.ets_import
                .error
                .as_deref()
                .unwrap()
                .contains("Capture changed")
        );
    }

    #[test]
    fn active_connection_and_pending_operations_block_import_confirmation() {
        let (_directory, mut app, file) = import_app();
        let database = app.model.as_ref().unwrap().database.clone();
        app.ets_import.preview = Some(
            ets_import::prepare(database.clone(), file, EtsFormat::Csv31, CsvEncoding::Utf8)
                .unwrap(),
        );
        app.model.as_mut().unwrap().owner_available = true;
        app.confirm_ets_import();
        assert!(app.ets_import.preview.is_some());
        assert!(app.ets_import.commit_receiver.is_none());
        app.model.as_mut().unwrap().owner_available = false;
        let (_sender, receiver) = mpsc::channel();
        app.connection_receiver = Some(receiver);
        app.confirm_ets_import();
        assert!(app.ets_import.preview.is_some());
        assert!(app.ets_import.commit_receiver.is_none());
        assert_eq!(
            CaptureStore::open_existing(&database)
                .unwrap()
                .ets_revision()
                .unwrap(),
            None
        );
    }

    #[test]
    fn import_commit_blocks_capture_switch_and_connect() {
        let (_directory, mut app, _file) = import_app();
        let original = app.model.as_ref().unwrap().database.clone();
        let other = original.with_file_name("other.sqlite");
        drop(CaptureStore::open_for_ets_import(&other).unwrap());
        let (_sender, receiver) = mpsc::channel();
        app.ets_import.commit_receiver = Some(receiver);
        app.attach(&other, true);
        assert_eq!(app.model.as_ref().unwrap().database, original);
        assert!(
            app.error
                .as_deref()
                .unwrap()
                .contains("ETS import to finish")
        );
        app.connect();
        assert!(app.connection_receiver.is_none());
        let settings = ConnectionSettings::from_endpoint("tunnel://192.0.2.8:3671").unwrap();
        assert!(
            app.select_connection(&settings, "tunnel://192.0.2.8:3671")
                .unwrap_err()
                .contains("ETS import to finish")
        );
    }

    #[test]
    fn completed_import_refreshes_selected_details_without_changing_filter() {
        let (_directory, mut app, file) = import_app();
        let model = app.model.as_mut().unwrap();
        append_rows(model, 1, 1);
        model.rows[0].destination = "1/2/4".into();
        model.filter = "Desk".into();
        app.selected = Some(model.rows[0].clone());
        let outcome = ets_import::commit(
            ets_import::prepare(
                model.database.clone(),
                file,
                EtsFormat::Csv31,
                CsvEncoding::Utf8,
            )
            .unwrap(),
        )
        .unwrap();
        let (sender, receiver) = mpsc::channel();
        app.ets_import.commit_receiver = Some(receiver);
        app.ets_import.open = true;
        sender.send(Ok(outcome)).unwrap();
        app.process_ets_import();
        assert_eq!(app.model.as_ref().unwrap().filter, "Desk");
        assert_eq!(
            app.selected.as_ref().unwrap().label.as_deref(),
            Some("Desk")
        );
        assert!(!app.ets_import.open);
        assert!(
            app.model
                .as_ref()
                .unwrap()
                .notices
                .last()
                .unwrap()
                .contains("revision 1")
        );
    }

    #[test]
    fn rest_indicator_distinguishes_live_off_and_unavailable() {
        let status = RestStatus {
            enabled: true,
            endpoint: Some("tunnel://192.0.2.10:3671".into()),
            bind: Some("127.0.0.1:8765".parse().unwrap()),
            allow_remote_writes: false,
        };
        assert_eq!(rest_indicator(None, None), "REST checking…");
        assert_eq!(rest_indicator(Some(&status), None), "REST :8765");
        assert_eq!(
            rest_indicator(Some(&status), Some("IPC failed")),
            "REST unavailable"
        );
        assert_eq!(
            rest_indicator(
                Some(&RestStatus {
                    enabled: false,
                    ..status
                }),
                None
            ),
            "REST off"
        );
    }

    #[test]
    fn rest_status_updates_when_another_surface_changes_the_daemon() {
        let mut app = MonitorApp {
            rest_last_refresh: Some(Instant::now()),
            ..MonitorApp::default()
        };
        let (sender, receiver) = mpsc::channel();
        app.rest_receiver = Some(receiver);
        app.rest_request_kind = Some(RestRequestKind::Status);
        app.rest_action_error = Some("Invalid listen address".into());
        sender
            .send(Ok(RestStatus {
                enabled: true,
                endpoint: Some("tunnel://192.0.2.10:3671".into()),
                bind: Some("127.0.0.1:8765".parse().unwrap()),
                allow_remote_writes: false,
            }))
            .unwrap();
        app.process_rest_background();
        assert_eq!(rest_indicator(app.rest_status.as_ref(), None), "REST :8765");

        let (sender, receiver) = mpsc::channel();
        app.rest_receiver = Some(receiver);
        app.rest_request_kind = Some(RestRequestKind::Status);
        sender
            .send(Ok(RestStatus {
                enabled: false,
                endpoint: None,
                bind: None,
                allow_remote_writes: false,
            }))
            .unwrap();
        app.process_rest_background();
        assert_eq!(rest_indicator(app.rest_status.as_ref(), None), "REST off");
        assert_eq!(
            app.rest_action_error.as_deref(),
            Some("Invalid listen address")
        );
    }

    fn append_rows(model: &mut MonitorModel, start: i64, end: i64) {
        for id in start..=end {
            model.rows.push(DisplayCapture {
                id: Some(id),
                timestamp_ms: u64::try_from(id).unwrap(),
                direction: "in".into(),
                source: "1.1.1".into(),
                destination: "1/1/1".into(),
                service: "GroupValueWrite".into(),
                label: None,
                dpts: Vec::new(),
                value: None,
                raw_cemi: "2900".into(),
            });
        }
    }

    #[test]
    fn virtualized_captures_fill_the_viewport_after_scroll_and_resize() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(2_000).unwrap()).unwrap());
        let mut model = MonitorModel::open(database).unwrap();
        append_rows(&mut model, 1, 1_004);
        for row in &mut model.rows {
            row.dpts = vec!["DPT-1-1".into()];
            if row.id.unwrap() % 2 == 1 {
                row.label = Some("Long ETS room and lighting circuit description ".repeat(10));
            }
        }
        let mut app = MonitorApp {
            model: Some(model),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut time = 0.0;
        for size in [
            egui::vec2(1_300.0, 900.0),
            egui::vec2(650.0, 420.0),
            egui::vec2(1_100.0, 700.0),
        ] {
            for frame in 0..8 {
                time += 0.1;
                let mut input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    time: Some(time),
                    ..Default::default()
                };
                if frame == 1 || frame == 4 {
                    input.events = vec![
                        egui::Event::PointerMoved(egui::pos2(200.0, 200.0)),
                        egui::Event::MouseWheel {
                            unit: egui::MouseWheelUnit::Point,
                            delta: egui::vec2(0.0, if frame == 1 { 240.0 } else { -240.0 }),
                            modifiers: egui::Modifiers::NONE,
                            phase: egui::TouchPhase::Move,
                        },
                    ];
                }
                let output = ctx.run_ui(input, |ui| {
                    app.render_captures(ui);
                });
                if frame == 3 || frame == 7 {
                    let (last_row_bottom, viewport_bottom) = output
                        .shapes
                        .iter()
                        .filter_map(|clipped| {
                            let egui::Shape::Text(text) = &clipped.shape else {
                                return None;
                            };
                            if !text.galley.text().contains("1/1/1")
                                || text.pos.y > clipped.clip_rect.bottom()
                            {
                                return None;
                            }
                            assert_eq!(text.galley.rows.len(), 1, "capture rows must not wrap");
                            assert!(text.pos.x <= 8.0, "capture text must be left-aligned");
                            assert!(
                                text.galley.text().contains("1.001"),
                                "DPT must remain visible"
                            );
                            Some((
                                text.pos.y + text.galley.rect.bottom(),
                                clipped.clip_rect.bottom(),
                            ))
                        })
                        .max_by(|left, right| left.0.total_cmp(&right.0))
                        .expect("capture rows were painted");
                    assert!(
                        viewport_bottom - last_row_bottom <= 30.0,
                        "empty capture rows at {size:?}: last row ends at {last_row_bottom}, viewport ends at {viewport_bottom}"
                    );
                    assert!(
                        size.y - viewport_bottom <= 8.0,
                        "table must use available height"
                    );
                }
                output.drop_without_applying_deltas();
            }
        }
    }

    #[test]
    fn offline_capture_does_not_follow_or_offer_a_knx_connection() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap());
        let mut app = MonitorApp::new(Some(database.clone()), None, false);
        assert!(app.follower.is_some());
        assert!(!app.offline_capture);
        let archive = directory.path().join("archive.sqlite");
        drop(CaptureStore::open(&archive, NonZeroU32::new(10).unwrap()).unwrap());
        app.attach(&archive, true);
        assert!(app.follower.is_none());
        assert_eq!(app.connection_status().1, "Offline capture");
        assert_eq!(app.connection_status().2, "archive.sqlite");
        app.connect();
        assert!(
            app.error
                .as_deref()
                .unwrap()
                .contains("Return to Live Capture")
        );
        app.attach(&database, false);
        assert!(app.follower.is_some());
        assert!(!app.offline_capture);
        app.attach(&database, true);
        assert!(!app.offline_capture);
    }

    #[test]
    fn explicit_database_override_stays_fixed_when_connection_changes() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("manual.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap());
        let mut app = MonitorApp::new(Some(database.clone()), None, false);
        let settings = ConnectionSettings::from_endpoint("tunnel://192.0.2.8:3671").unwrap();
        assert_eq!(
            app.select_connection(&settings, &settings.endpoint().unwrap())
                .unwrap(),
            database
        );
        assert_eq!(app.primary_database.as_deref(), Some(database.as_path()));
        assert_eq!(interface::load_settings(&database).unwrap(), settings);

        app.model.as_mut().unwrap().owner_available = true;
        let other = ConnectionSettings::from_endpoint("tunnel://192.0.2.9:3671").unwrap();
        assert!(
            app.select_connection(&other, &other.endpoint().unwrap())
                .unwrap_err()
                .contains("Disconnect")
        );
        assert_eq!(interface::load_settings(&database).unwrap(), settings);
    }

    #[test]
    fn live_gate_rejects_missing_scroll_disconnect_and_reconnect_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(1_000).unwrap()).unwrap());
        let mut model = MonitorModel::open(database).unwrap();
        model.state = WireState::Connected {
            endpoint: "tunnel://127.0.0.1:3671".into(),
        };
        append_rows(&mut model, 1, 100);
        let result = Arc::new(AtomicBool::new(false));
        let mut app = MonitorApp {
            model: Some(model),
            live_smoke: Some(LiveSmoke {
                started: Instant::now(),
                last_reported: Instant::now(),
                phase: LiveSmokePhase::Stream { baseline_id: 0 },
                pin_to_top: false,
                result: Arc::clone(&result),
            }),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        app.live_smoke_step(&ctx, Some(0.0));
        assert!(matches!(
            app.live_smoke.as_ref().unwrap().phase,
            LiveSmokePhase::Stream { .. }
        ));
        app.live_smoke_step(&ctx, Some(200.0));
        assert!(matches!(
            app.live_smoke.as_ref().unwrap().phase,
            LiveSmokePhase::Scroll { .. }
        ));
        append_rows(app.model.as_mut().unwrap(), 101, 150);
        app.live_smoke_step(&ctx, Some(200.0));
        assert!(matches!(
            app.live_smoke.as_ref().unwrap().phase,
            LiveSmokePhase::Scroll { .. }
        ));
        app.live_smoke_step(&ctx, Some(0.0));
        assert!(matches!(
            app.live_smoke.as_ref().unwrap().phase,
            LiveSmokePhase::Disconnect { .. }
        ));
        app.model.as_mut().unwrap().state = WireState::WaitingRetry {
            reason: "transport retry".into(),
            delay_ms: 2_000,
        };
        app.live_smoke_step(&ctx, Some(0.0));
        assert!(matches!(
            app.live_smoke.as_ref().unwrap().phase,
            LiveSmokePhase::Disconnect { .. }
        ));
        app.model.as_mut().unwrap().state = WireState::WaitingRetry {
            reason: "capture owner unavailable".into(),
            delay_ms: 2_000,
        };
        app.live_smoke_step(&ctx, Some(0.0));
        assert!(matches!(
            app.live_smoke.as_ref().unwrap().phase,
            LiveSmokePhase::Reconnect { .. }
        ));
        app.model.as_mut().unwrap().state = WireState::Connected {
            endpoint: "tunnel://127.0.0.1:3671".into(),
        };
        app.live_smoke_step(&ctx, Some(0.0));
        assert!(!result.load(Ordering::Acquire));
        append_rows(app.model.as_mut().unwrap(), 151, 155);
        app.live_smoke_step(&ctx, Some(0.0));
        assert!(result.load(Ordering::Acquire));
    }

    /// Whether the shortcut sees this key press, in a frame of its own.
    fn pressed(modifiers: egui::Modifiers, key: egui::Key) -> bool {
        let mut raw = egui::RawInput::default();
        // The frame's modifiers come from this event, not from the key's.
        raw.events.push(egui::Event::ModifiersChanged(modifiers));
        raw.events.push(egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        });
        let mut seen = false;
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(raw, |ui| {
            seen = ui.input(filter_shortcut_pressed);
        });
        // Nothing paints here, so the frame's textures are not wanted.
        output.textures_delta.clear();
        seen
    }

    #[test]
    fn cmd_f_is_the_shortcut_and_nothing_near_it() {
        let command = egui::Modifiers::COMMAND;
        assert!(pressed(command, egui::Key::F));
        // Counter-probes: F alone, another key with Cmd, and Cmd with Shift or
        // Alt (other shortcuts) are not it.
        assert!(!pressed(egui::Modifiers::NONE, egui::Key::F));
        assert!(!pressed(command, egui::Key::G));
        assert!(!pressed(command | egui::Modifiers::SHIFT, egui::Key::F));
        assert!(!pressed(command | egui::Modifiers::ALT, egui::Key::F));
    }

    /// The field takes the cursor and its text is selected, so that typing
    /// replaces it. A field that was not asked is left alone.
    #[test]
    fn the_filter_field_takes_the_cursor_with_its_text_selected() {
        let id = egui::Id::new("filter-under-test");
        let other = egui::Id::new("another-field");
        let ctx = egui::Context::default();
        let mut text = "0/1/2".to_string();
        let mut other_text = String::new();
        let frame = |text: &mut String, other_text: &mut String| {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.add(egui::TextEdit::singleline(text).id(id));
                ui.add(egui::TextEdit::singleline(other_text).id(other));
            });
            output.textures_delta.clear();
        };
        // The first frames create the fields and their state.
        frame(&mut text, &mut other_text);
        frame(&mut text, &mut other_text);
        assert!(!ctx.memory(|memory| memory.has_focus(id)));

        focus_text_field(&ctx, id, &text);
        frame(&mut text, &mut other_text);
        frame(&mut text, &mut other_text);
        assert!(ctx.memory(|memory| memory.has_focus(id)), "no focus");
        assert!(!ctx.memory(|memory| memory.has_focus(other)));
        let range = egui::TextEdit::load_state(&ctx, id)
            .and_then(|state| state.cursor.char_range())
            .expect("a cursor");
        assert_eq!(
            range,
            egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(5)),
            "the text should be selected from start to end"
        );
    }

    /// An app showing a capture with `rows` rows, owned by a daemon that is
    /// asked through a fake control.
    fn app_with_capture(
        rows: u64,
        fail: bool,
    ) -> (
        MonitorApp,
        crate::interface::CaptureLog,
        PathBuf,
        tempfile::TempDir,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(2_000).unwrap()).unwrap());
        let mut model = MonitorModel::open(database.clone()).unwrap();
        append_rows(&mut model, 1, i64::try_from(rows).unwrap());
        model.owner_available = true;
        let (control, asked) = crate::interface::fake_capture_control(rows, fail);
        let app = MonitorApp {
            model: Some(model),
            capture_control: Some(control),
            ..Default::default()
        };
        (app, asked, database, directory)
    }

    #[test]
    fn the_question_counts_the_capture_and_proposes_a_copy() {
        use crate::interface::CaptureRequest;
        let (mut app, asked, database, _directory) = app_with_capture(5, false);
        app.open_clear_capture();
        let question = app.clear_capture.as_ref().expect("a question");
        assert!(
            matches!(question.counts, Ok((5, 2))),
            "{:?}",
            question.counts
        );
        assert!(
            question.archive,
            "the copy is on unless the user turns it off"
        );
        assert_eq!(
            *asked.lock().unwrap(),
            vec![(database, CaptureRequest::Counts)]
        );
        // Cancelling deletes nothing and keeps what is loaded.
        app.cancel_clear_capture();
        assert!(app.clear_capture.is_none());
        assert_eq!(asked.lock().unwrap().len(), 1);
        assert_eq!(app.model.as_ref().unwrap().rows.len(), 5);
    }

    #[test]
    fn confirming_clears_with_the_copy_the_box_says_and_empties_the_view() {
        use crate::interface::CaptureRequest;
        for archive in [true, false] {
            let (mut app, asked, database, _directory) = app_with_capture(5, false);
            app.open_clear_capture();
            app.clear_capture.as_mut().unwrap().archive = archive;
            app.confirm_clear_capture();
            assert_eq!(
                asked.lock().unwrap().last().cloned(),
                Some((database, CaptureRequest::Clear { archive }))
            );
            assert!(app.clear_capture.is_none());
            let model = app.model.as_ref().unwrap();
            assert!(model.rows.is_empty(), "the loaded rows are dropped too");
            let notice = model.notices.last().expect("a notice");
            assert!(
                notice.starts_with("Cleared 5 telegrams and 2 loss reports"),
                "{notice}"
            );
            assert_eq!(notice.contains("copy in"), archive, "{notice}");
        }
    }

    #[test]
    fn a_refusal_is_reported_and_the_view_stays() {
        let (mut app, _, _, _directory) = app_with_capture(5, true);
        app.open_clear_capture();
        app.confirm_clear_capture();
        let error = app.error.clone().expect("an error");
        assert!(error.contains("failed"), "{error}");
        assert_eq!(app.model.as_ref().unwrap().rows.len(), 5);
    }

    /// An answer that comes after the window moved to another capture does
    /// not empty the one on screen.
    #[test]
    fn the_answer_does_not_empty_another_capture() {
        let (mut app, _, _, _directory) = app_with_capture(5, false);
        app.open_clear_capture();
        app.clear_capture.as_mut().unwrap().database = PathBuf::from("/elsewhere/other.sqlite");
        app.confirm_clear_capture();
        assert_eq!(app.model.as_ref().unwrap().rows.len(), 5);
    }

    #[test]
    fn without_a_control_no_question_opens() {
        let (mut app, asked, _, _directory) = app_with_capture(5, false);
        app.capture_control = None;
        app.open_clear_capture();
        assert!(app.clear_capture.is_none());
        assert!(app.error.is_some());
        assert!(asked.lock().unwrap().is_empty());
    }

    #[test]
    fn the_question_draws_for_a_full_an_empty_and_an_unreadable_capture() {
        for counts in [Ok((5, 2)), Ok((0, 0)), Err("no answer".to_string())] {
            let (mut app, asked, database, _directory) = app_with_capture(5, false);
            app.clear_capture = Some(ClearCaptureQuestion {
                database,
                counts,
                archive: true,
            });
            let ctx = egui::Context::default();
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                app.clear_capture_window(ui.ctx());
            });
            output.textures_delta.clear();
            assert!(app.clear_capture.is_some(), "drawing closed the question");
            assert!(asked.lock().unwrap().is_empty(), "drawing asked the daemon");
        }
    }

    #[test]
    fn the_question_counts_around_a_telegram_and_proposes_a_copy() {
        use crate::interface::CaptureRequest;
        use devknx::storage::{CaptureSide, CaptureSpan};
        let (mut app, asked, database, _directory) = app_with_capture(6, false);
        app.open_delete_around(CaptureSide::Below, 2, "1/1/1");
        let question = app.delete_around.as_ref().expect("a question");
        assert!(
            matches!(question.counts, Ok((3, 1))),
            "{:?}",
            question.counts
        );
        assert!(question.archive, "the copy is on unless it is turned off");
        // Below a telegram the deletion ends at the newest one this window has
        // seen, so telegrams that arrive meanwhile are not taken.
        let span = CaptureSpan {
            side: CaptureSide::Below,
            line: 2,
            through: Some(6),
        };
        assert_eq!(
            *asked.lock().unwrap(),
            vec![(database, CaptureRequest::CountAround { span })]
        );
        // Cancelling deletes nothing and keeps what is loaded.
        app.cancel_delete_around();
        assert!(app.delete_around.is_none());
        assert_eq!(asked.lock().unwrap().len(), 1);
        assert_eq!(app.model.as_ref().unwrap().rows.len(), 6);
    }

    #[test]
    fn above_a_telegram_has_no_end_to_stop_at() {
        use crate::interface::CaptureRequest;
        use devknx::storage::{CaptureSide, CaptureSpan};
        let (mut app, asked, database, _directory) = app_with_capture(6, false);
        app.open_delete_around(CaptureSide::Above, 4, "1/1/1");
        assert_eq!(
            *asked.lock().unwrap(),
            vec![(
                database,
                CaptureRequest::CountAround {
                    span: CaptureSpan {
                        side: CaptureSide::Above,
                        line: 4,
                        through: None,
                    }
                }
            )]
        );
    }

    #[test]
    fn confirming_deletes_with_the_copy_the_box_says_and_trims_the_view() {
        use crate::interface::CaptureRequest;
        use devknx::storage::{CaptureSide, CaptureSpan};
        for archive in [true, false] {
            let (mut app, asked, database, _directory) = app_with_capture(6, false);
            app.open_delete_around(CaptureSide::Below, 2, "1/1/1");
            app.delete_around.as_mut().unwrap().archive = archive;
            app.confirm_delete_around();
            let span = CaptureSpan {
                side: CaptureSide::Below,
                line: 2,
                through: Some(6),
            };
            assert_eq!(
                asked.lock().unwrap().last().cloned(),
                Some((database, CaptureRequest::DeleteAround { span, archive }))
            );
            assert!(app.delete_around.is_none());
            let model = app.model.as_ref().unwrap();
            let ids: Vec<Option<i64>> = model.rows.iter().map(|row| row.id).collect();
            assert_eq!(ids, [Some(1), Some(2)], "telegram 2 itself stays");
            let notice = model.notices.last().expect("a notice");
            assert!(
                notice.starts_with("Deleted 3 telegrams and 1 loss reports below telegram 2"),
                "{notice}"
            );
            assert_eq!(notice.contains("copy in"), archive, "{notice}");
        }
    }

    #[test]
    fn a_refusal_to_delete_is_reported_and_the_view_stays() {
        use devknx::storage::CaptureSide;
        let (mut app, _, _, _directory) = app_with_capture(6, true);
        app.open_delete_around(CaptureSide::Above, 4, "1/1/1");
        app.confirm_delete_around();
        let error = app.error.clone().expect("an error");
        assert!(error.contains("failed"), "{error}");
        assert_eq!(app.model.as_ref().unwrap().rows.len(), 6);
    }

    /// An answer that comes after the window moved to another capture does not
    /// trim the one on screen.
    #[test]
    fn the_answer_does_not_trim_another_capture() {
        use devknx::storage::CaptureSide;
        let (mut app, _, _, _directory) = app_with_capture(6, false);
        app.open_delete_around(CaptureSide::Above, 4, "1/1/1");
        app.delete_around.as_mut().unwrap().database = PathBuf::from("/elsewhere/other.sqlite");
        app.confirm_delete_around();
        assert_eq!(app.model.as_ref().unwrap().rows.len(), 6);
    }

    #[test]
    fn without_a_control_no_deletion_question_opens() {
        use devknx::storage::CaptureSide;
        let (mut app, asked, _, _directory) = app_with_capture(6, false);
        app.capture_control = None;
        app.open_delete_around(CaptureSide::Above, 4, "1/1/1");
        assert!(app.delete_around.is_none());
        assert!(app.error.is_some());
        assert!(asked.lock().unwrap().is_empty());
    }

    #[test]
    fn the_deletion_question_draws_for_a_count_none_and_an_error() {
        use devknx::storage::{CaptureSide, CaptureSpan};
        for counts in [Ok((3, 1)), Ok((0, 0)), Err("no answer".to_string())] {
            let (mut app, asked, database, _directory) = app_with_capture(6, false);
            app.delete_around = Some(DeleteAroundQuestion {
                database,
                span: CaptureSpan {
                    side: CaptureSide::Above,
                    line: 4,
                    through: None,
                },
                line: "4 (1/1/1)".into(),
                counts,
                archive: true,
            });
            let ctx = egui::Context::default();
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                app.delete_around_window(ui.ctx());
            });
            output.textures_delta.clear();
            assert!(app.delete_around.is_some(), "drawing closed the question");
            assert!(asked.lock().unwrap().is_empty(), "drawing asked the daemon");
        }
    }
}
