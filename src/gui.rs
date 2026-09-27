// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use devknx::ipc::WireState;
use eframe::egui;
use knx_rs_ip::discovery::GatewayInfo;

use crate::interface::{
    self, ConnectionMode, ConnectionSettings, DisplayCapture, Follower, MonitorModel,
};
use crate::platform::{self, MenuAction};

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

#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent visible GUI dialogs"
)]
struct MonitorApp {
    primary_database: Option<PathBuf>,
    offline_capture: bool,
    model: Option<MonitorModel>,
    follower: Option<Follower>,
    gateway_receiver: Option<Receiver<Result<Vec<GatewayInfo>, String>>>,
    gateways: Vec<GatewayInfo>,
    action_receiver: Option<Receiver<Result<devknx::ipc::IpcMessage, String>>>,
    connection_receiver: Option<Receiver<Result<String, String>>>,
    settings: ConnectionSettings,
    settings_draft: ConnectionSettings,
    error: Option<String>,
    show_settings: bool,
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
        let database = database.or_else(|| {
            if app.smoke.is_some() {
                None
            } else {
                match interface::default_database() {
                    Ok(path) => {
                        if let Err(error) = interface::ensure_database(&path) {
                            app.error = Some(format!("Cannot initialize capture storage: {error}"));
                            None
                        } else {
                            Some(path)
                        }
                    }
                    Err(error) => {
                        app.error = Some(error);
                        None
                    }
                }
            }
        });
        if let Some(database) = database {
            app.primary_database = Some(database.clone());
            app.attach(&database, false);
        }
        if let (Some(smoke), Some(model)) = (&mut app.live_smoke, &app.model) {
            smoke.phase = LiveSmokePhase::Stream {
                baseline_id: model.rows.last().and_then(|row| row.id).unwrap_or(0),
            };
        }
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
        if let Some(parent) = self
            .primary_database
            .as_ref()
            .and_then(|path| path.parent())
        {
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
        let Some(model) = &self.model else {
            self.error = Some("Capture storage is not available".into());
            return;
        };
        if self.connection_receiver.is_some() {
            return;
        }
        let endpoint = match self.settings.endpoint() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        if let Err(error) = interface::save_settings(&model.database, &self.settings) {
            self.error = Some(format!("Could not save connection settings: {error}"));
            return;
        }
        let database = model.database.clone();
        let (sender, receiver) = mpsc::channel();
        self.connection_receiver = Some(receiver);
        self.error = None;
        std::thread::spawn(move || {
            let _ = sender.send(interface::connect_owner(&database, &endpoint));
        });
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

    fn operate(&mut self, request: devknx::operations::OperationRequest) {
        let Some(model) = &self.model else { return };
        let database = model.database.clone();
        let (sender, receiver) = mpsc::channel();
        self.action_receiver = Some(receiver);
        std::thread::spawn(move || {
            let _ = sender.send(interface::operate(&database, &request));
        });
    }

    fn process_background(&mut self) {
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
                && last_id >= previous_id + 50
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
                .default_width(400.0)
                .show(ctx, |ui| {
                    ui.label("KNXnet/IP connection");
                    egui::ComboBox::from_label("Mode")
                        .selected_text(match self.settings_draft.mode {
                            ConnectionMode::Tunnel => "Tunneling",
                            ConnectionMode::Routing => "Routing (multicast)",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.settings_draft.mode,
                                ConnectionMode::Tunnel,
                                "Tunneling",
                            );
                            ui.selectable_value(
                                &mut self.settings_draft.mode,
                                ConnectionMode::Routing,
                                "Routing (multicast)",
                            );
                        });
                    ui.horizontal(|ui| {
                        ui.label("Gateway IP");
                        ui.text_edit_singleline(&mut self.settings_draft.address);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Port");
                        ui.add(
                            egui::DragValue::new(&mut self.settings_draft.port).range(1..=u16::MAX),
                        );
                    });
                    if self.settings_draft.mode == ConnectionMode::Routing {
                        ui.small("Enter the multicast group, for example 224.0.23.12.");
                    } else {
                        ui.small("Enter the gateway's unicast IP; discovery is optional.");
                    }
                    discover = ui.button("Discover gateways…").clicked();
                    ui.separator();
                    if let Some(database) = self.primary_database.as_ref() {
                        ui.label("Capture storage");
                        ui.small("History is stored automatically on this computer:");
                        ui.monospace(database.display().to_string());
                        ui.small("Open… chooses a saved capture with the system file dialog.");
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        save = ui.button("Save settings").clicked();
                        connect = ui
                            .add_enabled(
                                !self.offline_capture
                                    && self
                                        .model
                                        .as_ref()
                                        .is_some_and(|model| !model.owner_available)
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
                let validation = if connect || !self.settings_draft.address.is_empty() {
                    self.settings_draft.endpoint().map(|_| ())
                } else {
                    Ok(())
                };
                match validation {
                    Ok(()) => {
                        self.settings = self.settings_draft.clone();
                        if let Some(database) = &self.primary_database {
                            if let Err(error) = interface::save_settings(database, &self.settings) {
                                self.error =
                                    Some(format!("Could not save connection settings: {error}"));
                            } else {
                                self.error = None;
                                self.show_settings = false;
                                if connect {
                                    self.connect();
                                }
                            }
                        }
                    }
                    Err(error) => self.error = Some(error),
                }
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
                egui::Color32::RED,
                "Storage unavailable",
                "Use Open… to choose an existing capture file".into(),
            );
        };
        if self.offline_capture {
            return (
                egui::Color32::GRAY,
                "Offline capture",
                model.database.display().to_string(),
            );
        }
        if !model.owner_available {
            return (
                egui::Color32::GRAY,
                "Disconnected",
                "No capture service is running".into(),
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
                "Capture service stopped".into(),
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
                if ui.button("Live capture").clicked()
                    && let Some(database) = &self.primary_database
                {
                    self.attach(&database.clone(), false);
                }
            } else if ui
                .add_enabled(
                    self.model.is_some() && self.connection_receiver.is_none(),
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
                .add_enabled(!self.offline_capture, egui::Button::new("Gateways…"))
                .clicked()
            {
                self.discover();
            }
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

    fn render_empty(&mut self, ui: &mut egui::Ui) {
        ui.add_space((ui.available_height() * 0.18).min(110.0));
        ui.vertical_centered(|ui| {
            ui.heading("Connect to KNXnet/IP");
            ui.label("Select a gateway or enter its IP address. Captures are saved automatically.");
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 420.0).max(0.0) / 2.0);
                ui.label("Gateway IP");
                ui.add(egui::TextEdit::singleline(&mut self.settings.address)
                    .hint_text("192.168.2.8").desired_width(170.0));
                if ui.add_enabled(self.model.is_some() && self.connection_receiver.is_none(), egui::Button::new("Connect")).clicked() {
                    self.connect();
                }
            });
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 265.0).max(0.0) / 2.0);
                if ui.button("Discover gateways").clicked() { self.discover(); }
                if ui.button("Connection settings…").clicked() {
                    self.settings_draft = self.settings.clone();
                    self.show_settings = true;
                }
            });
            ui.small("Gateway discovery uses multicast. Manual tunneling works without multicast discovery.");
        });
    }

    #[expect(
        clippy::too_many_lines,
        reason = "virtualized table, compact layout and details share the same capture selection"
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
        if compact {
            ui.monospace(format!(
                "{:<12} {:<9} {:<11} {:<14} {}",
                "Time", "Direction", "Destination", "Value", "ETS group"
            ));
        } else {
            ui.monospace(format!(
                "{:<12} {:<9} {:<9} {:<11} {:<19} {:<14} {}",
                "Time", "Direction", "Source", "Destination", "Service", "Value", "ETS group"
            ));
        }
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
            .auto_shrink([false, false])
            .stick_to_bottom(!pin_to_top)
            .max_height((ui.available_height() - 85.0).max(100.0));
        if pin_to_top {
            scroll = scroll.vertical_scroll_offset(0.0);
        }
        let output = scroll.show_rows(ui, 23.0, visible.len(), |ui, range| {
            for row in &visible[range] {
                let text = if compact {
                    format!(
                        "{:<12} {:<9} {:<11} {:<14} {}",
                        interface::format_time(row.timestamp_ms),
                        row.direction,
                        row.destination,
                        row.value.as_deref().unwrap_or("—"),
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
                        row.value.as_deref().unwrap_or("—"),
                        row.label.as_deref().unwrap_or("")
                    )
                };
                if ui
                    .selectable_label(
                        self.selected
                            .as_ref()
                            .is_some_and(|current| current.id == row.id),
                        egui::RichText::new(text).monospace(),
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
        ui.separator();
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
                        row.value.as_deref().unwrap_or("unknown"),
                        row.raw_cemi
                    ))
                    .monospace(),
                )
                .wrap(),
            );
        } else {
            ui.small(format!(
                "{} visible · Select a capture for raw cEMI and DPT details",
                visible.len()
            ));
        }
        Some(output.state.offset.y)
    }

    fn render_status(&self, ui: &mut egui::Ui) {
        let (_, status, detail) = self.connection_status();
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{status} · {detail}"));
            if let Some(model) = &self.model {
                ui.separator();
                ui.label(format!("{} captures", model.rows.len()));
                ui.separator();
                ui.label(format!(
                    "Router losses (session): {}",
                    model.router_lost_messages
                ));
                ui.separator();
                ui.label(format!("Local lag (session): {}", model.local_lag_events));
                ui.separator();
                ui.label("History saved locally")
                    .on_hover_text(model.database.display().to_string());
            }
        });
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
        let mut scroll_offset = None;
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(error) = &self.error {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
                ui.separator();
            }
            if !self.offline_capture
                && self
                    .model
                    .as_ref()
                    .is_some_and(|model| model.rows.is_empty() && !model.owner_available)
            {
                self.render_empty(ui);
            } else if self.model.is_some() {
                scroll_offset = self.render_captures(ui);
            } else {
                ui.label(
                    "Capture storage could not be opened. Use Open… to choose an existing file.",
                );
            }
            if let Some(notice) = self.model.as_ref().and_then(|model| model.notices.last()) {
                ui.separator();
                ui.label(notice);
            }
        });
        self.live_smoke_step(&ctx, scroll_offset);
        self.dialogs(&ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devknx::storage::CaptureStore;
    use std::num::NonZeroU32;

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
        append_rows(app.model.as_mut().unwrap(), 151, 200);
        app.live_smoke_step(&ctx, Some(0.0));
        assert!(result.load(Ordering::Acquire));
    }
}
