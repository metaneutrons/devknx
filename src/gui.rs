// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use devknx::control::RestStatus;
use devknx::ipc::WireState;
use devknx::paths;
use eframe::egui;
use knx_rs_ip::discovery::GatewayInfo;

use crate::color::{self, Tone};
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
    eframe::run_native(
        "devknx",
        options,
        Box::new(move |_cc| {
            platform::init_app();
            Ok(Box::new(MonitorApp::new(
                database,
                app_smoke_result,
                smoke_live,
            )))
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

#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent visible GUI dialogs"
)]
struct MonitorApp {
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
        app
    }

    fn attach(&mut self, database: &Path, offline: bool) {
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
        if self.connection_receiver.is_some() {
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

    fn process_background(&mut self) {
        self.process_rest_background();
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

    fn process_menu(&mut self) -> bool {
        if platform::take(MenuAction::OpenDatabase) {
            self.open_capture_dialog();
        }
        if platform::take(MenuAction::ExportCsv) {
            self.show_export = true;
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

    #[expect(clippy::too_many_lines, reason = "four independent small modal forms")]
    fn dialogs(&mut self, ctx: &egui::Context) {
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
                if ui.button("Live capture").clicked() {
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
                    self.connection_receiver.is_none(),
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
            if ui
                .button("Open…")
                .on_hover_text("Open a saved capture file")
                .clicked()
            {
                self.open_capture_dialog();
            }
            if ui
                .add_enabled(self.model.is_some(), egui::Button::new("Export…"))
                .clicked()
            {
                self.show_export = true;
            }
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
                    ui.memory_mut(|memory| memory.request_focus(id));
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
                "{:<12} {:<9} {:<11} {:<14} {}",
                "Time", "Direction", "Destination", "Value", "ETS group"
            )
        } else {
            format!(
                "{:<12} {:<9} {:<9} {:<11} {:<19} {:<14} {}",
                "Time", "Direction", "Source", "Destination", "Service", "Value", "ETS group"
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
                        "{:<12} {:<9} {:<11} {:<14} {}",
                        interface::format_time(row.timestamp_ms),
                        row.direction,
                        row.destination,
                        row.value_text(),
                        row.label.as_deref().unwrap_or("")
                    )
                } else {
                    format!(
                        "{:<12} {:<9} {:<9} {:<11} {:<19} {:<14} {}",
                        interface::format_time(row.timestamp_ms),
                        row.direction,
                        row.source,
                        row.destination,
                        row.service,
                        row.value_text(),
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
                if ui
                    .add_sized(
                        [ui.available_width(), row_height],
                        egui::Button::selectable(is_selected, (rich, egui::Atom::grow()))
                            .truncate(),
                    )
                    .clicked()
                {
                    selected = Some((*row).clone());
                }
            }
        });
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
        let focus_filter = self.process_menu();
        let ctx = ui.ctx().clone();
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
        platform::update_color_menu_state(self.color_enabled, color::ui_is_locked());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devknx::storage::CaptureStore;
    use std::num::NonZeroU32;

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
}
