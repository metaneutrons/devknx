// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use devknx::ipc::WireState;
use eframe::egui;
use knx_rs_ip::discovery::GatewayInfo;

use crate::interface::{self, DisplayCapture, Follower, MonitorModel};
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
    database_input: String,
    model: Option<MonitorModel>,
    follower: Option<Follower>,
    gateway_receiver: Option<Receiver<Result<Vec<GatewayInfo>, String>>>,
    gateways: Vec<GatewayInfo>,
    action_receiver: Option<Receiver<Result<devknx::ipc::IpcMessage, String>>>,
    error: Option<String>,
    show_open: bool,
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
        if let Some(database) = database {
            app.database_input = database.display().to_string();
            app.attach();
        }
        if let (Some(smoke), Some(model)) = (&mut app.live_smoke, &app.model) {
            smoke.phase = LiveSmokePhase::Stream {
                baseline_id: model.rows.last().and_then(|row| row.id).unwrap_or(0),
            };
        }
        app
    }

    fn attach(&mut self) {
        let database = PathBuf::from(self.database_input.trim());
        match MonitorModel::open(database.clone()) {
            Ok(model) => {
                self.follower = Some(Follower::start(database));
                self.model = Some(model);
                self.error = None;
                self.selected = None;
                self.show_open = false;
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn discover(&mut self) {
        let (sender, receiver) = mpsc::channel();
        self.gateway_receiver = Some(receiver);
        std::thread::spawn(move || {
            let _ = sender.send(interface::discover_gateways());
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
                    self.error = Some(error);
                    self.gateway_receiver = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.error = Some("Gateway discovery stopped unexpectedly".into());
                    self.gateway_receiver = None;
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
            self.show_open = true;
        }
        if platform::take(MenuAction::ExportCsv) {
            self.show_export = true;
        }
        if platform::take(MenuAction::Discover) {
            self.discover();
        }
        if platform::take(MenuAction::Read) {
            self.show_read = true;
        }
        if platform::take(MenuAction::Write) {
            self.show_write = true;
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
        if self.show_open {
            let mut open = self.show_open;
            let mut attach = false;
            egui::Window::new("Open Capture Database")
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label("Existing SQLite capture database");
                    ui.text_edit_singleline(&mut self.database_input);
                    attach = ui.button("Attach").clicked();
                });
            self.show_open = open;
            if attach {
                self.attach();
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
                            self.model.is_some() && self.action_receiver.is_none(),
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
                                    self.model.is_some() && self.action_receiver.is_none(),
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
}

impl eframe::App for MonitorApp {
    #[expect(
        clippy::too_many_lines,
        reason = "one egui frame composes the monitor controls"
    )]
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.process_background();
        let focus_filter = self.process_menu();
        let ctx = ui.ctx().clone();
        self.smoke_step(&ctx);
        ctx.request_repaint_after(Duration::from_millis(100));
        ui.heading("devknx · KNXnet/IP monitor");
        ui.horizontal(|ui| {
            if ui.button("Open database…").clicked() {
                self.show_open = true;
            }
            if ui.button("Discover gateways").clicked() {
                self.discover();
            }
            if ui
                .add_enabled(self.model.is_some(), egui::Button::new("Export CSV…"))
                .clicked()
            {
                self.show_export = true;
            }
            if ui
                .add_enabled(self.model.is_some(), egui::Button::new("Read…"))
                .clicked()
            {
                self.show_read = true;
            }
            if ui
                .add_enabled(self.model.is_some(), egui::Button::new("Write…"))
                .clicked()
            {
                self.show_write = true;
            }
        });
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::RED, error);
        }
        let mut scroll_offset = None;
        if let Some(model) = &mut self.model {
            ui.label(format!("Database: {}", model.database.display()));
            ui.strong(format!("Connection: {:?}", model.state));
            ui.horizontal(|ui| {
                ui.label("Filter");
                let id = egui::Id::new("capture-filter");
                if focus_filter {
                    ui.memory_mut(|memory| memory.request_focus(id));
                }
                ui.add(
                    egui::TextEdit::singleline(&mut model.filter)
                        .id(id)
                        .desired_width(250.0),
                );
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
            let visible: Vec<_> = model
                .rows
                .iter()
                .filter(|row| row.matches(&model.filter))
                .collect();
            ui.label(format!(
                "{} visible · {} buffered",
                visible.len(),
                model.rows.len()
            ));
            let mut selected = None;
            let pin_to_top = self
                .live_smoke
                .as_ref()
                .is_some_and(|smoke| smoke.pin_to_top);
            let mut scroll = egui::ScrollArea::vertical()
                .id_salt("captures")
                .stick_to_bottom(!pin_to_top)
                .max_height((ui.available_height() - 190.0).max(120.0));
            if pin_to_top {
                scroll = scroll.vertical_scroll_offset(0.0);
            }
            let output = scroll.show_rows(ui, 23.0, visible.len(), |ui, range| {
                for row in &visible[range] {
                    let text = format!(
                        "{}  {:8}  {:8} → {:9}  {:18}  {}",
                        row.timestamp_ms,
                        row.direction,
                        row.source,
                        row.destination,
                        row.service,
                        row.label.as_deref().unwrap_or("")
                    );
                    if ui.selectable_label(false, text).clicked() {
                        selected = Some((*row).clone());
                    }
                }
            });
            scroll_offset = Some(output.state.offset.y);
            if let Some(row) = selected {
                self.selected = Some(row);
            }
            if let Some(row) = &self.selected {
                ui.separator();
                ui.monospace(format!(
                    "Selected: {} · {} · DPT [{}] · raw cEMI {}",
                    row.destination,
                    row.label.as_deref().unwrap_or("no ETS label"),
                    row.dpts.join(", "),
                    row.raw_cemi
                ));
            }
            if let Some(notice) = model.notices.last() {
                ui.separator();
                ui.label(notice);
            }
        } else {
            ui.label("Open an existing capture database and start `devknx serve` to follow live traffic.");
        }
        if self.gateway_receiver.is_some() {
            ui.spinner();
        }
        for gateway in &self.gateways {
            ui.label(format!("Gateway: {} · {}", gateway.name, gateway.address));
        }
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
                raw_cemi: "2900".into(),
            });
        }
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
