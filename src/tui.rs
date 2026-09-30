// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Terminal monitor using the same history, ETS and operation model as the GUI.

use std::cell::Cell;
use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use crossterm::ExecutableCommand as _;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::buffer::CellWidth;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::color::{self, Tone};
use crate::interface::{self, ConnectionSettings, Follower, MonitorModel};
use devknx::control::RestStatus;
use devknx::ets::{CsvEncoding, EtsFormat};
use devknx::paths;

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let old = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = io::stdout().execute(LeaveAlternateScreen);
            old(info);
        }));
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    Filter,
    Read,
    WriteAddress,
    WriteDpt,
    WriteValue,
    ConfirmWrite,
    ConfirmRest,
    Export,
    ConnectionEndpoint,
    EtsImportPath,
    EtsImportFormat,
    EtsImportEncoding,
    EtsImportPreparing,
    ConfirmEtsImport,
    EtsImportCommitting,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent terminal view and connection state"
)]
struct App {
    model: MonitorModel,
    follower: Follower,
    fixed_database: bool,
    action_receiver: Option<Receiver<Result<devknx::ipc::IpcMessage, String>>>,
    discovery_receiver: Option<Receiver<Result<Vec<knx_rs_ip::discovery::GatewayInfo>, String>>>,
    connection_receiver: Option<Receiver<Result<String, String>>>,
    rest_receiver: Option<Receiver<Result<RestStatus, String>>>,
    rest_status: Option<RestStatus>,
    last_rest_refresh: Instant,
    rest_action_pending: bool,
    settings: ConnectionSettings,
    mode: Mode,
    input: String,
    write_address: String,
    write_dpt: String,
    write_value: String,
    preview: String,
    ets_import_receiver: Option<Receiver<Result<crate::ets_import::EtsImportPreview, String>>>,
    ets_import_preview: Option<crate::ets_import::EtsImportPreview>,
    ets_import_commit_receiver:
        Option<Receiver<Result<crate::ets_import::EtsImportOutcome, String>>>,
    ets_import_file: Option<PathBuf>,
    ets_import_target: Option<PathBuf>,
    ets_import_scroll: Cell<u16>,
    ets_import_scroll_max: Cell<u16>,
    ets_import_notice: Option<String>,
    selected: usize,
    follow_tail: bool,
    color_enabled: bool,
}

impl App {
    fn new(
        database: PathBuf,
        fixed_database: bool,
        selected_endpoint: Option<&str>,
    ) -> Result<Self, String> {
        let mut model = MonitorModel::open(database.clone())?;
        let settings = if let Some(endpoint) = selected_endpoint {
            ConnectionSettings::from_endpoint(endpoint)?
        } else {
            match interface::load_settings(&database) {
                Ok(settings) => settings,
                Err(error) => {
                    model.notice(format!("Connection settings could not be loaded: {error}"));
                    ConnectionSettings::default()
                }
            }
        };
        let selected = model.rows.len().saturating_sub(1);
        Ok(Self {
            model,
            follower: Follower::start(database),
            fixed_database,
            action_receiver: None,
            discovery_receiver: None,
            connection_receiver: None,
            rest_receiver: None,
            rest_status: None,
            last_rest_refresh: Instant::now()
                .checked_sub(Duration::from_secs(5))
                .unwrap_or_else(Instant::now),
            rest_action_pending: false,
            settings,
            mode: Mode::Normal,
            input: String::new(),
            write_address: String::new(),
            write_dpt: String::new(),
            write_value: String::new(),
            preview: String::new(),
            ets_import_receiver: None,
            ets_import_preview: None,
            ets_import_commit_receiver: None,
            ets_import_file: None,
            ets_import_target: None,
            ets_import_scroll: Cell::new(0),
            ets_import_scroll_max: Cell::new(0),
            ets_import_notice: None,
            selected,
            follow_tail: true,
            color_enabled: color::ui_enabled(),
        })
    }

    fn poll(&mut self) {
        self.poll_ets_import();
        self.poll_rest();
        for result in self.follower.receiver.try_iter().take(500) {
            match result {
                Ok(message) => self.model.ingest(message),
                Err(error) => self.model.owner_unavailable(&error),
            }
        }
        if self.follow_tail {
            self.selected = self.visible_count().saturating_sub(1);
        }
        if let Some(receiver) = &self.action_receiver {
            match receiver.try_recv() {
                Ok(Ok(message)) => {
                    self.model.ingest(message);
                    if self.mode == Mode::ConfirmEtsImport {
                        self.ets_import_notice =
                            Some("Pending KNX operation completed; preview retained.".into());
                    }
                    self.action_receiver = None;
                }
                Ok(Err(error)) => {
                    if self.mode == Mode::ConfirmEtsImport {
                        self.ets_import_notice =
                            Some(format!("Pending KNX operation failed: {error}"));
                    }
                    self.model.notice(error);
                    self.action_receiver = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    if self.mode == Mode::ConfirmEtsImport {
                        self.ets_import_notice =
                            Some("Pending KNX operation stopped unexpectedly.".into());
                    }
                    self.model
                        .notice("Operation worker stopped unexpectedly".into());
                    self.action_receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        self.poll_discovery_and_connection();
    }

    fn poll_ets_import(&mut self) {
        if let Some(receiver) = &self.ets_import_receiver {
            match receiver.try_recv() {
                Ok(Ok(preview)) => {
                    self.ets_import_receiver = None;
                    if preview.database == self.model.database {
                        self.ets_import_file = Some(preview.file.clone());
                        self.ets_import_target = Some(preview.database.clone());
                        self.ets_import_preview = Some(preview);
                        self.ets_import_notice = Some("Preview ready; no data has changed.".into());
                        self.mode = Mode::ConfirmEtsImport;
                    } else {
                        self.ets_import_file = None;
                        self.model.notice(
                            "ETS preview discarded because the capture database changed".into(),
                        );
                        self.mode = Mode::Normal;
                    }
                }
                Ok(Err(error)) => {
                    self.model
                        .notice(format!("Could not preview ETS import: {error}"));
                    self.ets_import_receiver = None;
                    self.ets_import_file = None;
                    self.mode = Mode::Normal;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.model
                        .notice("ETS preview worker stopped unexpectedly".into());
                    self.ets_import_receiver = None;
                    self.ets_import_file = None;
                    self.mode = Mode::Normal;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        if let Some(receiver) = &self.ets_import_commit_receiver {
            match receiver.try_recv() {
                Ok(Ok(outcome)) => {
                    if outcome.database == self.model.database {
                        let notice = outcome.notice();
                        if let Err(error) = self.model.refresh_ets() {
                            self.model.notice(format!(
                                "{notice}; capture display refresh failed: {error}"
                            ));
                        } else {
                            self.model.notice(notice);
                        }
                    } else {
                        self.model.notice(format!(
                            "ETS import committed to {}, but the selected database changed",
                            outcome.database.display()
                        ));
                    }
                    self.finish_ets_import_commit();
                }
                Ok(Err(error)) => {
                    self.model.notice(format!("ETS import failed: {error}"));
                    self.finish_ets_import_commit();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.model
                        .notice("ETS import worker stopped unexpectedly".into());
                    self.finish_ets_import_commit();
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    fn finish_ets_import_commit(&mut self) {
        self.ets_import_commit_receiver = None;
        self.ets_import_target = None;
        self.ets_import_file = None;
        self.ets_import_notice = None;
        self.ets_import_scroll.set(0);
        self.ets_import_scroll_max.set(0);
        self.mode = Mode::Normal;
    }

    const fn import_commit_pending(&self) -> bool {
        self.ets_import_commit_receiver.is_some()
    }

    fn clear_ets_import(&mut self) {
        self.ets_import_receiver = None;
        self.ets_import_preview = None;
        self.ets_import_file = None;
        self.ets_import_target = None;
        self.ets_import_scroll.set(0);
        self.ets_import_scroll_max.set(0);
        self.ets_import_notice = None;
        if matches!(
            self.mode,
            Mode::EtsImportPath
                | Mode::EtsImportFormat
                | Mode::EtsImportEncoding
                | Mode::EtsImportPreparing
                | Mode::ConfirmEtsImport
        ) {
            self.mode = Mode::Normal;
            self.input.clear();
        }
    }

    fn cancel_ets_import(&mut self) {
        self.clear_ets_import();
        self.model
            .notice("ETS import cancelled; capture data was not changed".into());
    }

    fn start_ets_import_prepare(&mut self, format: EtsFormat, encoding: CsvEncoding) {
        let Some(file) = self.ets_import_file.clone() else {
            self.model.notice("Choose an ETS export file first".into());
            self.mode = Mode::Normal;
            return;
        };
        let database = self.model.database.clone();
        let (sender, receiver) = mpsc::channel();
        self.ets_import_receiver = Some(receiver);
        self.mode = Mode::EtsImportPreparing;
        std::thread::spawn(move || {
            let _ = sender.send(crate::ets_import::prepare(database, file, format, encoding));
        });
    }

    fn confirm_ets_import(&mut self) {
        if self.model.owner_available {
            let notice = "Owner active; press c to disconnect before importing. Preview retained.";
            self.ets_import_notice = Some(notice.into());
            self.model.notice(notice.into());
            return;
        }
        let busy_notice = if self.connection_receiver.is_some() {
            Some("Connection operation pending; wait before importing.")
        } else if self.action_receiver.is_some() {
            Some("KNX operation pending; wait before importing.")
        } else if self.rest_action_pending {
            Some("REST action pending; wait before importing.")
        } else {
            None
        };
        if let Some(notice) = busy_notice {
            self.ets_import_notice = Some(notice.into());
            self.model.notice(notice.into());
            return;
        }
        let Some(preview) = self.ets_import_preview.take() else {
            self.model
                .notice("ETS preview is no longer available".into());
            self.mode = Mode::Normal;
            return;
        };
        if preview.database != self.model.database {
            self.ets_import_preview = Some(preview);
            self.clear_ets_import();
            self.model
                .notice("ETS preview discarded because the capture database changed".into());
            return;
        }
        self.ets_import_target = Some(preview.database.clone());
        self.ets_import_file = Some(preview.file.clone());
        self.ets_import_scroll.set(0);
        self.ets_import_scroll_max.set(0);
        let (sender, receiver) = mpsc::channel();
        self.ets_import_commit_receiver = Some(receiver);
        self.mode = Mode::EtsImportCommitting;
        std::thread::spawn(move || {
            let _ = sender.send(crate::ets_import::commit(preview));
        });
    }

    fn poll_rest(&mut self) {
        if self.rest_receiver.is_none()
            && self.last_rest_refresh.elapsed() >= Duration::from_secs(5)
        {
            let (sender, receiver) = mpsc::channel();
            self.rest_receiver = Some(receiver);
            self.rest_action_pending = false;
            self.last_rest_refresh = Instant::now();
            std::thread::spawn(move || {
                let _ = sender.send(interface::rest_status());
            });
        }
        if let Some(receiver) = &self.rest_receiver {
            match receiver.try_recv() {
                Ok(Ok(status)) => {
                    if self.rest_action_pending {
                        let notice = if status.enabled {
                            format!(
                                "REST enabled at http://{}/v1 for {}",
                                status.bind.map_or_else(
                                    || "unknown address".into(),
                                    |bind| bind.to_string()
                                ),
                                status.endpoint.as_deref().unwrap_or("no default endpoint")
                            )
                        } else {
                            "REST disabled".into()
                        };
                        if self.mode == Mode::ConfirmEtsImport {
                            self.ets_import_notice = Some(notice.clone());
                        }
                        self.model.notice(notice);
                    }
                    self.rest_status = Some(status);
                    self.rest_receiver = None;
                    self.rest_action_pending = false;
                }
                Ok(Err(error)) => {
                    if self.mode == Mode::ConfirmEtsImport && self.rest_action_pending {
                        self.ets_import_notice = Some(format!("REST control failed: {error}"));
                    }
                    self.model.notice(format!("REST control failed: {error}"));
                    self.rest_receiver = None;
                    self.rest_action_pending = false;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    if self.mode == Mode::ConfirmEtsImport && self.rest_action_pending {
                        self.ets_import_notice =
                            Some("REST control worker stopped unexpectedly.".into());
                    }
                    self.model
                        .notice("REST control worker stopped unexpectedly".into());
                    self.rest_receiver = None;
                    self.rest_action_pending = false;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    fn poll_discovery_and_connection(&mut self) {
        if let Some(receiver) = &self.discovery_receiver {
            match receiver.try_recv() {
                Ok(Ok(gateways)) => {
                    let endpoints = gateways
                        .iter()
                        .map(|gateway| format!("{} ({})", gateway.name, gateway.address))
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.model.notice(format!(
                        "Discovered {} gateways: {endpoints}",
                        gateways.len()
                    ));
                    self.discovery_receiver = None;
                }
                Ok(Err(error)) => {
                    self.model.notice(error);
                    self.discovery_receiver = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.model
                        .notice("Gateway discovery stopped unexpectedly".into());
                    self.discovery_receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(receiver) = &self.connection_receiver {
            match receiver.try_recv() {
                Ok(Ok(notice)) => {
                    if self.mode == Mode::ConfirmEtsImport {
                        self.ets_import_notice = Some(notice.clone());
                    }
                    self.model.notice(notice);
                    self.connection_receiver = None;
                }
                Ok(Err(error)) => {
                    if self.mode == Mode::ConfirmEtsImport {
                        self.ets_import_notice = Some(error.clone());
                    }
                    self.model.notice(error);
                    self.connection_receiver = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    if self.mode == Mode::ConfirmEtsImport {
                        self.ets_import_notice =
                            Some("Connection worker stopped unexpectedly.".into());
                    }
                    self.model
                        .notice("Connection worker stopped unexpectedly".into());
                    self.connection_receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    fn visible_count(&self) -> usize {
        self.model
            .rows
            .iter()
            .filter(|row| row.matches(&self.model.filter))
            .count()
    }

    fn start_operation(&mut self, request: devknx::operations::OperationRequest) {
        if self.action_receiver.is_some() {
            self.model.notice("An operation is already running".into());
            return;
        }
        let database = self.model.database.clone();
        let (sender, receiver) = mpsc::channel();
        self.action_receiver = Some(receiver);
        std::thread::spawn(move || {
            let _ = sender.send(interface::operate(&database, &request));
        });
    }

    fn start_connection(&mut self) {
        if self.import_commit_pending() {
            self.model
                .notice("Wait for the ETS import to finish before connecting".into());
            return;
        }
        if self.connection_receiver.is_some() {
            return;
        }
        let endpoint = match self.settings.endpoint() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.model.notice(error);
                return;
            }
        };
        let database = if self.fixed_database {
            Ok(self.model.database.clone())
        } else {
            paths::database_for_endpoint(&endpoint)
        };
        let database = match database {
            Ok(database) => database,
            Err(error) => {
                self.model.notice(error);
                return;
            }
        };
        if let Err(error) = self.select_endpoint_database(
            self.settings.clone(),
            database,
            interface::save_recent_settings,
        ) {
            self.model
                .notice(format!("Could not prepare connection storage: {error}"));
            return;
        }
        let database = self.model.database.clone();
        let (sender, receiver) = mpsc::channel();
        self.connection_receiver = Some(receiver);
        std::thread::spawn(move || {
            let _ = sender.send(interface::connect_owner(&database, &endpoint));
        });
    }

    fn toggle_rest(&mut self) {
        if self.rest_receiver.is_some() {
            self.model
                .notice("REST control is already in progress".into());
            return;
        }
        let endpoint = match self.settings.endpoint() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.model.notice(error);
                return;
            }
        };
        let database = self.model.database.clone();
        let (sender, receiver) = mpsc::channel();
        self.rest_receiver = Some(receiver);
        self.rest_action_pending = true;
        std::thread::spawn(move || {
            let result = interface::rest_status().and_then(|status| {
                if status.enabled {
                    if status.endpoint.is_none()
                        || status.endpoint.as_deref() == Some(endpoint.as_str())
                    {
                        interface::rest_disable()
                    } else {
                        Err("REST belongs to another KNX session; use the CLI to inspect it".into())
                    }
                } else {
                    interface::rest_enable(
                        Some(&endpoint),
                        Some(database),
                        "127.0.0.1:8765",
                        None,
                        false,
                    )
                }
            });
            let _ = sender.send(result);
        });
    }

    fn select_endpoint_database(
        &mut self,
        settings: ConnectionSettings,
        database: PathBuf,
        save_recent: impl FnOnce(&ConnectionSettings) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.import_commit_pending() {
            return Err("Wait for the ETS import to finish before changing capture storage".into());
        }
        if self.model.owner_available && self.settings != settings {
            return Err("Disconnect before changing connection settings".into());
        }
        let changing_database = self.model.database != database;
        if changing_database && self.connection_receiver.is_some() {
            return Err("Wait for the current connection operation to finish".into());
        }
        if changing_database && self.action_receiver.is_some() {
            return Err(
                "Wait for the current operation to finish before changing capture storage".into(),
            );
        }

        interface::ensure_database(&database)?;
        let replacement_model = if changing_database {
            Some(MonitorModel::open(database.clone())?)
        } else {
            None
        };
        interface::save_settings(&database, &settings)?;
        let recent_error = if self.fixed_database {
            None
        } else {
            save_recent(&settings).err()
        };

        if let Some(model) = replacement_model {
            self.clear_ets_import();
            self.follower = Follower::start(database);
            self.model = model;
            self.selected = self.model.rows.len().saturating_sub(1);
            self.follow_tail = true;
        }
        self.settings = settings;
        self.model
            .notice("Connection settings saved. Press c to connect.".into());
        if let Some(error) = recent_error {
            self.model.notice(format!(
                "Endpoint settings were saved, but recent connection settings could not be updated: {error}"
            ));
        }
        Ok(())
    }

    fn submit_endpoint_with(
        &mut self,
        input: &str,
        resolve_database: impl FnOnce(&str) -> Result<PathBuf, String>,
        save_recent: impl FnOnce(&ConnectionSettings) -> Result<(), String>,
    ) -> Result<(), String> {
        let settings = ConnectionSettings::from_endpoint(input)?;
        let endpoint = settings.endpoint()?;
        let database = if self.fixed_database {
            self.model.database.clone()
        } else {
            resolve_database(&endpoint)?
        };
        self.select_endpoint_database(settings, database, save_recent)
    }

    fn disconnect_connection(&mut self) {
        if self.connection_receiver.is_some() {
            return;
        }
        let database = self.model.database.clone();
        let (sender, receiver) = mpsc::channel();
        self.connection_receiver = Some(receiver);
        std::thread::spawn(move || {
            let _ = sender.send(interface::disconnect_owner(&database));
        });
    }

    fn submit(&mut self) {
        let input = std::mem::take(&mut self.input);
        match self.mode {
            Mode::Filter => {
                self.model.filter = input;
                self.selected = self.visible_count().saturating_sub(1);
                self.mode = Mode::Normal;
            }
            Mode::Read => {
                match interface::read_request(&input) {
                    Ok(request) => self.start_operation(request),
                    Err(error) => self.model.notice(error),
                }
                self.mode = Mode::Normal;
            }
            Mode::WriteAddress => {
                self.write_address = input;
                self.mode = Mode::WriteDpt;
            }
            Mode::WriteDpt => {
                self.write_dpt = input;
                self.mode = Mode::WriteValue;
            }
            Mode::WriteValue => {
                self.write_value = input;
                match self.model.preview_write(
                    &self.write_address,
                    &self.write_dpt,
                    &self.write_value,
                ) {
                    Ok(preview) => {
                        self.preview = preview;
                        self.mode = Mode::ConfirmWrite;
                    }
                    Err(error) => {
                        self.model.notice(error);
                        self.mode = Mode::Normal;
                    }
                }
            }
            Mode::Export => {
                match interface::export_csv(&self.model.database, &PathBuf::from(input.trim())) {
                    Ok(count) => self.model.notice(format!("Exported {count} captures")),
                    Err(error) => self.model.notice(error),
                }
                self.mode = Mode::Normal;
            }
            Mode::ConnectionEndpoint => {
                if let Err(error) = self.submit_endpoint_with(
                    &input,
                    paths::database_for_endpoint,
                    interface::save_recent_settings,
                ) {
                    self.model
                        .notice(format!("Could not save connection settings: {error}"));
                }
                self.mode = Mode::Normal;
            }
            Mode::EtsImportPath => {
                let path = expand_user_path(input.trim());
                if path.as_os_str().is_empty() {
                    self.model.notice("Enter an ETS export file path".into());
                    self.mode = Mode::EtsImportPath;
                } else {
                    self.ets_import_file = Some(path);
                    self.mode = Mode::EtsImportFormat;
                }
            }
            Mode::Normal
            | Mode::ConfirmWrite
            | Mode::ConfirmRest
            | Mode::EtsImportFormat
            | Mode::EtsImportEncoding
            | Mode::EtsImportPreparing
            | Mode::ConfirmEtsImport
            | Mode::EtsImportCommitting => {}
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the terminal key map is a single explicit state machine"
    )]
    fn key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        if self.import_commit_pending() {
            return false;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        if key.code == KeyCode::Esc {
            if matches!(
                self.mode,
                Mode::EtsImportPath
                    | Mode::EtsImportFormat
                    | Mode::EtsImportEncoding
                    | Mode::EtsImportPreparing
                    | Mode::ConfirmEtsImport
            ) {
                self.cancel_ets_import();
            } else {
                self.mode = Mode::Normal;
                self.input.clear();
            }
            return false;
        }
        if key.code == KeyCode::F(8) {
            if color::ui_is_locked() {
                self.model
                    .notice("Color is controlled by --color or NO_COLOR".into());
            } else {
                self.color_enabled = !self.color_enabled;
                if let Err(error) = color::save_preference(self.color_enabled) {
                    self.model
                        .notice(format!("Could not save color preference: {error}"));
                } else {
                    self.model.notice(if self.color_enabled {
                        "Capture colors on".into()
                    } else {
                        "Capture colors off".into()
                    });
                }
            }
            return false;
        }
        if self.mode == Mode::ConfirmWrite {
            if key.code == KeyCode::Char('y') {
                match interface::write_request(
                    &self.write_address,
                    &self.write_dpt,
                    &self.write_value,
                ) {
                    Ok(request) => self.start_operation(request),
                    Err(error) => self.model.notice(error),
                }
            }
            self.mode = Mode::Normal;
            self.preview.clear();
            return false;
        }
        if self.mode == Mode::ConfirmRest {
            if key.code == KeyCode::Char('y') {
                self.toggle_rest();
            }
            self.mode = Mode::Normal;
            return false;
        }
        if self.mode == Mode::ConfirmEtsImport {
            match key.code {
                KeyCode::Char('y') => self.confirm_ets_import(),
                KeyCode::Char('c') => {
                    if self.model.owner_available {
                        if self.connection_receiver.is_some() {
                            let notice = "Disconnect operation pending; preview retained.";
                            self.ets_import_notice = Some(notice.into());
                            self.model.notice(notice.into());
                        } else {
                            self.ets_import_notice =
                                Some("Disconnecting owner; preview retained.".into());
                            self.disconnect_connection();
                        }
                    } else {
                        let notice = "No active owner to disconnect; preview retained.";
                        self.ets_import_notice = Some(notice.into());
                        self.model.notice(notice.into());
                    }
                }
                KeyCode::Char('n') => self.cancel_ets_import(),
                KeyCode::Up => {
                    self.ets_import_scroll
                        .set(self.ets_import_scroll.get().saturating_sub(1));
                }
                KeyCode::Down => {
                    self.ets_import_scroll.set(
                        self.ets_import_scroll
                            .get()
                            .saturating_add(1)
                            .min(self.ets_import_scroll_max.get()),
                    );
                }
                KeyCode::PageUp => {
                    self.ets_import_scroll
                        .set(self.ets_import_scroll.get().saturating_sub(5));
                }
                KeyCode::PageDown => {
                    self.ets_import_scroll.set(
                        self.ets_import_scroll
                            .get()
                            .saturating_add(5)
                            .min(self.ets_import_scroll_max.get()),
                    );
                }
                _ => {}
            }
            return false;
        }
        if self.mode == Mode::EtsImportFormat {
            match key.code {
                KeyCode::Char('c') => self.mode = Mode::EtsImportEncoding,
                KeyCode::Char('x') => {
                    self.start_ets_import_prepare(EtsFormat::GaXml01, CsvEncoding::Utf8);
                }
                _ => {}
            }
            return false;
        }
        if self.mode == Mode::EtsImportEncoding {
            match key.code {
                KeyCode::Char('u') => {
                    self.start_ets_import_prepare(EtsFormat::Csv31, CsvEncoding::Utf8);
                }
                KeyCode::Char('l') => {
                    self.start_ets_import_prepare(EtsFormat::Csv31, CsvEncoding::Latin1);
                }
                _ => {}
            }
            return false;
        }
        if self.mode == Mode::EtsImportPreparing || self.mode == Mode::EtsImportCommitting {
            return false;
        }
        if self.mode != Mode::Normal {
            match key.code {
                KeyCode::Enter => self.submit(),
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.input.push(ch);
                }
                _ => {}
            }
            return false;
        }
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('c') => {
                if self.model.owner_available {
                    self.disconnect_connection();
                } else if self.settings.address.is_empty() {
                    self.mode = Mode::ConnectionEndpoint;
                    self.input = "tunnel://".into();
                } else {
                    self.start_connection();
                }
            }
            KeyCode::Char('s') => {
                self.mode = Mode::ConnectionEndpoint;
                self.input = self
                    .settings
                    .endpoint()
                    .unwrap_or_else(|_| "tunnel://".into());
            }
            KeyCode::Char('a') => {
                self.mode = Mode::ConfirmRest;
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Filter;
                self.input = self.model.filter.clone();
            }
            KeyCode::Char('r') => {
                self.mode = Mode::Read;
                self.input.clear();
            }
            KeyCode::Char('w') => {
                self.mode = Mode::WriteAddress;
                self.input.clear();
            }
            KeyCode::Char('e') => {
                self.mode = Mode::Export;
                self.input.clear();
            }
            KeyCode::Char('i') => {
                self.mode = Mode::EtsImportPath;
                self.input.clear();
                self.ets_import_file = None;
                self.ets_import_preview = None;
                self.ets_import_notice = None;
                self.ets_import_scroll.set(0);
                self.ets_import_scroll_max.set(0);
            }
            KeyCode::Char('h') => {
                if let Err(error) = self.model.reload_history() {
                    self.model.notice(error);
                }
            }
            KeyCode::PageUp => {
                let previous_visible = self.visible_count();
                match self.model.load_older() {
                    Ok(0) => self.model.notice("No older retained captures".into()),
                    Ok(count) => {
                        let added_visible = self.visible_count().saturating_sub(previous_visible);
                        self.selected = self
                            .selected
                            .saturating_add(added_visible)
                            .min(self.visible_count().saturating_sub(1));
                        self.follow_tail = false;
                        self.model.notice(format!("Loaded {count} older captures"));
                    }
                    Err(error) => self.model.notice(error),
                }
            }
            KeyCode::Char('d') => {
                let (sender, receiver) = mpsc::channel();
                std::thread::spawn(move || {
                    let _ = sender.send(interface::discover_gateways());
                });
                self.discovery_receiver = Some(receiver);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.visible_count().saturating_sub(1));
                self.follow_tail = self.selected + 1 >= self.visible_count();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.follow_tail = false;
            }
            KeyCode::End => {
                self.selected = self.visible_count().saturating_sub(1);
                self.follow_tail = true;
            }
            KeyCode::Home => {
                self.selected = 0;
                self.follow_tail = false;
            }
            _ => {}
        }
        false
    }

    const fn ets_import_readiness(&self) -> &'static str {
        if self.connection_receiver.is_some() && self.model.owner_available {
            "Busy: disconnect in progress"
        } else if self.model.owner_available {
            "Busy: active owner; press c"
        } else if self.connection_receiver.is_some() {
            "Busy: connection pending"
        } else if self.action_receiver.is_some() {
            "Busy: KNX action pending"
        } else if self.rest_action_pending {
            "Busy: REST action pending"
        } else {
            "Ready: press y to import"
        }
    }

    fn draw<B: Backend>(&self, terminal: &mut Terminal<B>) -> Result<(), B::Error> {
        terminal.draw(|frame| {
            let areas = Layout::vertical([Constraint::Length(3), Constraint::Min(5), Constraint::Length(3), Constraint::Length(3)])
                .split(frame.area());
            let status = if self.model.owner_available {
                match &self.model.state {
                    devknx::ipc::WireState::Idle => "Starting".into(),
                    devknx::ipc::WireState::Connecting { .. } => "Connecting".into(),
                    devknx::ipc::WireState::Connected { endpoint } => format!("Connected · {endpoint}"),
                    devknx::ipc::WireState::WaitingRetry { reason, .. } => format!("Retrying · {reason}"),
                    devknx::ipc::WireState::Stopped => "Disconnected".into(),
                    devknx::ipc::WireState::StorageFailed { reason } => format!("Storage error · {reason}"),
                }
            } else {
                "Disconnected".to_owned()
            };
            let selected_endpoint = self.settings.endpoint().ok();
            let rest = match &self.rest_status {
                Some(rest) if rest.enabled && rest.endpoint.as_ref() == selected_endpoint.as_ref() => "on",
                Some(rest) if rest.enabled => "other session",
                Some(_) => "off",
                None => "unknown",
            };
            let title = format!(" KNXnet/IP · {status} · REST {rest} ");
            frame.render_widget(Paragraph::new(title).block(Block::default().borders(Borders::ALL)), areas[0]);

            let visible: Vec<_> = self.model.rows.iter().filter(|row| row.matches(&self.model.filter)).collect();
            let capacity = usize::from(areas[1].height.saturating_sub(2));
            let selected = self.selected.min(visible.len().saturating_sub(1));
            let start = selected.saturating_sub(capacity.saturating_sub(1));
            let end = (start + capacity).min(visible.len());
            let items: Vec<ListItem> = visible[start..end].iter().enumerate().map(|(index, row)| {
                let text = format!("{} {:8} {:8} {:9} {:15} {:10} {:14} {}",
                    interface::format_time(row.timestamp_ms), row.direction, row.source, row.destination,
                    row.service, row.value_text(), row.dpt_text(), row.label.as_deref().unwrap_or(""));
                let style = if start + index == selected {
                    if self.color_enabled { Style::default().fg(Color::Black).bg(Color::Cyan) }
                    else { Style::default().add_modifier(Modifier::REVERSED) }
                } else if self.color_enabled && color::direction_tone(&row.direction) == Tone::Sent {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                };
                ListItem::new(text).style(style)
            }).collect();
            frame.render_widget(List::new(items).block(Block::default().title(format!(
                " Captures · {} visible / {} buffered · filter: {} ",
                visible.len(), self.model.rows.len(), self.model.filter)).borders(Borders::ALL)), areas[1]);
            let detail = visible.get(selected).map_or_else(
                || "No capture selected".to_owned(),
                |row| format!("{} · {} · DPT [{}] · value {} · raw cEMI {}", row.destination,
                    row.label.as_deref().unwrap_or("no ETS label"), row.dpts.join(", "),
                    row.value_text(), row.raw_cemi));
            frame.render_widget(Paragraph::new(detail).block(Block::default().title(" Details ").borders(Borders::ALL)), areas[2]);
            let prompt = match self.mode {
                Mode::Normal => "c connect/disconnect · s endpoint · i ETS import · a REST · d discover · / filter · r read · w write · e export · h reload · F8 color · q quit".to_owned(),
                Mode::Filter => format!("Filter: {}", self.input),
                Mode::Read => format!("Read group address: {}", self.input),
                Mode::WriteAddress => format!("Write group address: {}", self.input),
                Mode::WriteDpt => format!("DPT (blank = ETS): {}", self.input),
                Mode::WriteValue => format!("Typed value: {}", self.input),
                Mode::ConfirmWrite => format!("{} · y transmit / n or Esc cancel", self.preview),
                Mode::ConfirmRest => "Toggle REST for the selected KNX endpoint (enable uses loopback)? y confirm / n or Esc cancel".to_owned(),
                Mode::Export => format!("New CSV file path: {}", self.input),
                Mode::ConnectionEndpoint => format!("KNXnet/IP endpoint (tunnel://IP:3671 or router://MULTICAST:3671): {}", self.input),
                Mode::EtsImportPath => format!("ETS export file path: {}", self.input),
                Mode::EtsImportFormat => "ETS format: [c] CSV 3/1 · [x] KNX GA XML 01".into(),
                Mode::EtsImportEncoding => "CSV encoding: [u] UTF-8 · [l] ISO-8859-1".into(),
                Mode::EtsImportPreparing => "Preparing ETS import preview… Esc cancels".into(),
                Mode::ConfirmEtsImport => "Review the ETS import preview".into(),
                Mode::EtsImportCommitting => "ETS import in progress… input is disabled".into(),
            };
            let notice = self.model.notices.last().map_or("", String::as_str);
            let notice_style = if self.color_enabled {
                match color::notice_tone(notice) {
                    Tone::Error => Style::default().fg(Color::Red),
                    Tone::Warning => Style::default().fg(Color::Yellow),
                    Tone::Sent | Tone::Plain => Style::default(),
                }
            } else {
                Style::default()
            };
            frame.render_widget(Paragraph::new(vec![Line::raw(prompt), Line::styled(notice, notice_style)])
                .block(Block::default().borders(Borders::ALL)), areas[3]);

            self.draw_ets_import_overlay(frame);
        })?;
        Ok(())
    }

    fn draw_ets_import_overlay(&self, frame: &mut Frame<'_>) {
        if self.mode == Mode::ConfirmEtsImport
            && let Some(preview) = &self.ets_import_preview
        {
            let mut body = format!(
                "This replaces the ETS catalogue. Raw captures and previous revisions are preserved.\n\
                 Only declared, unambiguous DPTs decode values. Standard four-column CSV supplies names but no DPTs.\n\n\
                 Target database: {}\nSource file: {}\nSummary: {}\nSamples:\n",
                preview.database.display(),
                preview.file.display(),
                preview.summary(),
            );
            for sample in preview.sample_lines() {
                writeln!(body, "  {sample}").expect("String write cannot fail");
            }
            render_ets_confirmation_overlay(
                frame,
                self.ets_import_readiness(),
                self.ets_import_notice
                    .as_deref()
                    .unwrap_or("Preview ready; no data has changed."),
                &body,
                self.ets_import_scroll.get(),
                &self.ets_import_scroll,
                &self.ets_import_scroll_max,
            );
        } else if self.mode == Mode::EtsImportPreparing {
            let body = format!(
                "Preparing the ETS preview. The capture database is not being changed.\n\nTarget database: {}\nSource file: {}\n\nPress Esc to cancel.",
                self.model.database.display(),
                self.ets_import_file
                    .as_deref()
                    .map_or_else(|| "unknown".to_owned(), |path| path.display().to_string())
            );
            render_import_overlay(frame, "Preparing ETS import", &body);
        } else if self.mode == Mode::EtsImportCommitting {
            let body = format!(
                "Writing the confirmed ETS revision to the selected capture database.\n\nTarget database: {}\nSource file: {}\n\nPlease wait. Keyboard input is disabled until the transaction finishes.",
                self.ets_import_target.as_deref().map_or_else(
                    || self.model.database.display().to_string(),
                    |path| { path.display().to_string() }
                ),
                self.ets_import_file
                    .as_deref()
                    .map_or_else(|| "unknown".to_owned(), |path| path.display().to_string())
            );
            render_import_overlay(frame, "Importing ETS", &body);
        }
    }
}

fn expand_user_path(input: &str) -> PathBuf {
    if input == "~" {
        if let Some(home) = user_home_dir() {
            return PathBuf::from(home);
        }
    } else if let Some(relative) = input.strip_prefix("~/")
        && let Some(home) = user_home_dir()
    {
        return PathBuf::from(home).join(relative);
    }
    PathBuf::from(input)
}

fn user_home_dir() -> Option<std::ffi::OsString> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
}

fn render_import_overlay(frame: &mut Frame<'_>, title: &str, text: &str) {
    let area = frame.area();
    let overlay = import_overlay_area(area);
    frame.render_widget(Clear, overlay);
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        overlay,
    );
}

fn render_ets_confirmation_overlay(
    frame: &mut Frame<'_>,
    readiness: &str,
    notice: &str,
    body: &str,
    scroll: u16,
    scroll_state: &Cell<u16>,
    scroll_limit: &Cell<u16>,
) {
    let overlay = import_overlay_area(frame.area());
    let block = Block::default()
        .title("Confirm ETS import")
        .borders(Borders::ALL);
    let inner = block.inner(overlay);
    frame.render_widget(Clear, overlay);
    frame.render_widget(block, overlay);

    let header_height = inner.height.min(4);
    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(header_height), Constraint::Min(0)]).areas(inner);
    let notice_width = inner.width.saturating_sub(6);
    let notice = truncate_overlay_line(notice, usize::from(notice_width));
    let header = format!(
        "y import / c disconnect / Esc cancel / ↓ scroll\nStatus: {readiness}\nLast: {notice}"
    );
    frame.render_widget(
        Paragraph::new(header).wrap(Wrap { trim: true }),
        header_area,
    );

    let wrapped_body = wrap_overlay_text(body, body_area.width);
    let paragraph = Paragraph::new(wrapped_body.as_str()).wrap(Wrap { trim: true });
    let content_lines = wrapped_body.lines().count();
    let max_scroll = u16::try_from(
        content_lines
            .saturating_sub(usize::from(body_area.height))
            .min(usize::from(u16::MAX)),
    )
    .unwrap_or(u16::MAX);
    let scroll = scroll.min(max_scroll);
    scroll_state.set(scroll);
    scroll_limit.set(max_scroll);
    frame.render_widget(paragraph.scroll((scroll, 0)), body_area);
}

fn import_overlay_area(area: Rect) -> Rect {
    let width = area.width.saturating_sub(2).min(100);
    let height = area.height.saturating_sub(2).min(22);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn truncate_overlay_line(text: &str, max_width: usize) -> String {
    let ellipsis = "…";
    let ellipsis_width = Line::from(ellipsis).width();
    let text_width = Line::from(text).width();
    if text_width <= max_width {
        return text.to_owned();
    }
    let max_prefix_width = max_width.saturating_sub(ellipsis_width);
    let mut output = String::new();
    let mut width = 0;
    for grapheme in Line::from(text).styled_graphemes(Style::default()) {
        let grapheme_width = usize::from(grapheme.symbol.cell_width());
        if width + grapheme_width > max_prefix_width {
            break;
        }
        width += grapheme_width;
        output.push_str(grapheme.symbol);
    }
    if max_width >= ellipsis_width {
        output.push_str(ellipsis);
    }
    output
}

fn wrap_overlay_text(text: &str, width: u16) -> String {
    let width = usize::from(width.max(1));
    let mut output = String::new();
    for (line_index, line) in text.trim_end_matches('\n').split('\n').enumerate() {
        if line_index > 0 {
            output.push('\n');
        }
        let mut current_width = 0;
        for word in line.split_whitespace() {
            append_wrapped_word(word, width, &mut current_width, &mut output);
        }
    }
    output
}

fn append_wrapped_word(
    word: &str,
    max_width: usize,
    current_width: &mut usize,
    output: &mut String,
) {
    let word_width = Line::from(word).width();
    if word_width <= max_width {
        if *current_width > 0 {
            if *current_width + 1 + word_width > max_width {
                output.push('\n');
                *current_width = 0;
            } else {
                output.push(' ');
                *current_width += 1;
            }
        }
        output.push_str(word);
        *current_width += word_width;
        return;
    }

    if *current_width > 0 {
        output.push('\n');
        *current_width = 0;
    }
    for grapheme in Line::from(word).styled_graphemes(Style::default()) {
        let grapheme_width = usize::from(grapheme.symbol.cell_width());
        if *current_width > 0 && *current_width + grapheme_width > max_width {
            output.push('\n');
            *current_width = 0;
        }
        output.push_str(grapheme.symbol);
        *current_width += grapheme_width;
    }
}

pub fn run(
    database: PathBuf,
    fixed_database: bool,
    selected_endpoint: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    interface::ensure_database(&database).map_err(io::Error::other)?;
    let mut app =
        App::new(database, fixed_database, selected_endpoint).map_err(io::Error::other)?;
    install_panic_hook();
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    loop {
        app.poll();
        app.draw(&mut terminal)?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && app.key(key)
        {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use devknx::storage::CaptureStore;
    use std::num::NonZeroU32;
    use std::thread;

    const ETS_CSV: &str = "Main;Middle;Sub;Address\nHome;Lights;Desk;1/2/4\n";

    fn press(app: &mut App, code: KeyCode) -> bool {
        app.key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn preview_for(database: PathBuf, file: PathBuf) -> crate::ets_import::EtsImportPreview {
        crate::ets_import::prepare(database, file, EtsFormat::Csv31, CsvEncoding::Utf8).unwrap()
    }

    #[test]
    fn import_path_expands_home_prefix() {
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                expand_user_path("~/exports/groups.csv"),
                PathBuf::from(home).join("exports/groups.csv")
            );
        }
        assert_eq!(
            expand_user_path("~user/groups.csv"),
            PathBuf::from("~user/groups.csv")
        );
    }

    fn wait_for_import_preview(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.mode == Mode::EtsImportPreparing && Instant::now() < deadline {
            app.poll_ets_import();
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(app.mode, Mode::ConfirmEtsImport);
        assert!(app.ets_import_preview.is_some());
    }

    fn read_screen(terminal: &Terminal<ratatui::backend::TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    fn read_screen_lines(terminal: &Terminal<ratatui::backend::TestBackend>) -> Vec<String> {
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(40)
            .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect())
            .collect()
    }

    #[test]
    fn startup_resize_and_input_modes_render_without_a_live_owner() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap());
        let mut app = App::new(database, true, None).unwrap();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        app.draw(&mut terminal).unwrap();
        assert!(read_screen(&terminal).contains("i ETS import"));
        terminal.resize(Rect::new(0, 0, 40, 12)).unwrap();
        app.draw(&mut terminal).unwrap();
        assert!(!app.key(crossterm::event::KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.mode, Mode::Filter);
        app.input = "light".into();
        app.submit();
        assert_eq!(app.model.filter, "light");
        app.draw(&mut terminal).unwrap();
    }

    #[test]
    fn malformed_connection_settings_do_not_hide_capture_history() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap());
        std::fs::write(paths::connection_settings_file(&database), b"not JSON").unwrap();
        let app = App::new(database, true, None).unwrap();
        assert!(
            app.model
                .notices
                .last()
                .unwrap()
                .contains("could not be loaded")
        );
        assert_eq!(app.settings, ConnectionSettings::default());
    }

    #[test]
    fn endpoint_submission_saves_without_starting_a_connection() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap());
        let mut app = App::new(database.clone(), true, None).unwrap();
        app.mode = Mode::ConnectionEndpoint;
        app.input = "tunnel://192.0.2.8:3671".into();
        app.submit();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.connection_receiver.is_none());
        assert_eq!(
            interface::load_settings(&database).unwrap(),
            ConnectionSettings::from_endpoint("tunnel://192.0.2.8:3671").unwrap()
        );
        assert_eq!(app.model.database, database);
        assert!(
            app.model
                .notices
                .last()
                .unwrap()
                .contains("Press c to connect")
        );
    }

    #[test]
    fn endpoint_selector_populates_connection_before_first_connect() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("endpoint.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap());
        let endpoint = "tunnel://192.0.2.8:3671";
        let app = App::new(database, false, Some(endpoint)).unwrap();
        assert_eq!(app.settings.endpoint().unwrap(), endpoint);
        assert!(app.connection_receiver.is_none());
    }

    #[test]
    fn ets_import_flow_shows_confirmation_details_and_cancel_does_not_write() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        let file = directory.path().join("groups.csv");
        drop(CaptureStore::open_for_ets_import(&database).unwrap());
        std::fs::write(&file, ETS_CSV).unwrap();
        let mut app = App::new(database.clone(), true, None).unwrap();

        assert!(!press(&mut app, KeyCode::Char('i')));
        assert_eq!(app.mode, Mode::EtsImportPath);
        app.input = file.display().to_string();
        assert!(!press(&mut app, KeyCode::Enter));
        assert_eq!(app.mode, Mode::EtsImportFormat);
        assert!(!press(&mut app, KeyCode::Char('c')));
        assert_eq!(app.mode, Mode::EtsImportEncoding);
        assert!(!press(&mut app, KeyCode::Char('u')));
        wait_for_import_preview(&mut app);

        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();
        app.draw(&mut terminal).unwrap();
        let screen = read_screen(&terminal);
        assert!(screen.contains("Target database:"));
        assert!(screen.contains(database.to_str().unwrap()));
        assert!(screen.contains("Source file:"));
        assert!(screen.contains(file.to_str().unwrap()));
        assert!(screen.contains("1 group address"));
        assert!(screen.contains("1/2/4"));
        assert!(screen.contains("Raw captures"));
        assert_eq!(
            CaptureStore::open_existing(&database)
                .unwrap()
                .ets_revision()
                .unwrap(),
            None
        );

        assert!(!press(&mut app, KeyCode::Char('n')));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.ets_import_preview.is_none());
        assert_eq!(
            CaptureStore::open_existing(&database)
                .unwrap()
                .ets_revision()
                .unwrap(),
            None
        );
    }

    #[test]
    fn ets_confirmation_on_small_terminal_keeps_status_visible_and_clamps_scroll() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        let file = directory.path().join("groups.csv");
        drop(CaptureStore::open_for_ets_import(&database).unwrap());
        std::fs::write(&file, ETS_CSV).unwrap();
        let mut app = App::new(database.clone(), true, None).unwrap();
        app.ets_import_preview = Some(preview_for(database, file));
        app.model.owner_available = true;
        let (_connection_sender, connection_receiver) = mpsc::channel();
        app.connection_receiver = Some(connection_receiver);
        app.mode = Mode::ConfirmEtsImport;
        assert!(!press(&mut app, KeyCode::Char('y')));
        assert!(
            app.ets_import_notice
                .as_deref()
                .is_some_and(|notice| notice.contains("Owner active"))
        );

        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(40, 12)).unwrap();
        app.draw(&mut terminal).unwrap();
        let visible = read_screen_lines(&terminal);
        assert!(
            visible
                .iter()
                .any(|line| line.contains("Busy: disconnect in progress"))
        );
        assert!(
            visible
                .iter()
                .any(|line| line.contains("Last: Owner active"))
        );
        let max_scroll = app.ets_import_scroll_max.get();
        assert!(max_scroll > 5);

        for _ in 0..5 {
            assert!(!press(&mut app, KeyCode::Down));
        }
        app.draw(&mut terminal).unwrap();
        assert!(read_screen(&terminal).contains("CSV supplies names"));

        for _ in 0..usize::from(max_scroll) {
            assert!(!press(&mut app, KeyCode::Down));
        }
        assert_eq!(app.ets_import_scroll.get(), max_scroll);
        assert!(!press(&mut app, KeyCode::Down));
        assert_eq!(app.ets_import_scroll.get(), max_scroll);
        app.draw(&mut terminal).unwrap();
        let visible = read_screen_lines(&terminal);
        assert!(visible.iter().any(|line| line.contains("1/2/4")));
        assert!(
            visible
                .iter()
                .any(|line| line.contains("Busy: disconnect in progress"))
        );
        assert!(
            visible
                .iter()
                .any(|line| line.contains("Last: Owner active"))
        );
    }

    #[test]
    fn ets_import_format_and_csv_encoding_choices_select_xml_and_latin1() {
        let directory = tempfile::tempdir().unwrap();
        let xml_database = directory.path().join("xml.sqlite");
        let xml_file = directory.path().join("groups.xml");
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
            <GroupAddress-Export xmlns="http://knx.org/xml/ga-export/01">
              <GroupRange Name="Lighting" RangeStart="1" RangeEnd="2047">
                <GroupAddress Name="Desk" Address="1/2/4" />
              </GroupRange>
            </GroupAddress-Export>"#;
        drop(CaptureStore::open_for_ets_import(&xml_database).unwrap());
        std::fs::write(&xml_file, xml).unwrap();
        let mut xml_app = App::new(xml_database, true, None).unwrap();
        assert!(!press(&mut xml_app, KeyCode::Char('i')));
        xml_app.input = xml_file.display().to_string();
        xml_app.submit();
        assert!(!press(&mut xml_app, KeyCode::Char('x')));
        wait_for_import_preview(&mut xml_app);
        assert!(xml_app.ets_import_preview.as_ref().unwrap().sample_lines()[0].contains("Desk"));
        assert!(!press(&mut xml_app, KeyCode::Char('n')));

        let csv_database = directory.path().join("latin1.sqlite");
        let csv_file = directory.path().join("legacy.csv");
        drop(CaptureStore::open_for_ets_import(&csv_database).unwrap());
        std::fs::write(
            &csv_file,
            b"Main;Middle;Sub;Address\nHome;Lights;K\xe4che;1/2/4\n",
        )
        .unwrap();
        let mut csv_app = App::new(csv_database, true, None).unwrap();
        assert!(!press(&mut csv_app, KeyCode::Char('i')));
        csv_app.input = csv_file.display().to_string();
        csv_app.submit();
        assert!(!press(&mut csv_app, KeyCode::Char('c')));
        assert_eq!(csv_app.mode, Mode::EtsImportEncoding);
        assert!(!press(&mut csv_app, KeyCode::Char('l')));
        wait_for_import_preview(&mut csv_app);
        assert!(csv_app.ets_import_preview.as_ref().unwrap().sample_lines()[0].contains("Käche"));
        assert!(!press(&mut csv_app, KeyCode::Char('n')));
    }

    #[test]
    fn cancelling_background_ets_parse_drops_receiver_and_cannot_restore_preview() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        let file = directory.path().join("groups.csv");
        drop(CaptureStore::open_for_ets_import(&database).unwrap());
        std::fs::write(&file, ETS_CSV).unwrap();
        let mut app = App::new(database.clone(), true, None).unwrap();
        app.mode = Mode::EtsImportPath;
        app.input = file.display().to_string();
        app.submit();
        assert!(!press(&mut app, KeyCode::Char('c')));
        assert!(!press(&mut app, KeyCode::Char('u')));
        assert_eq!(app.mode, Mode::EtsImportPreparing);
        assert!(app.ets_import_receiver.is_some());

        assert!(!press(&mut app, KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.ets_import_receiver.is_none());
        for _ in 0..20 {
            app.poll_ets_import();
            thread::sleep(Duration::from_millis(1));
        }
        assert!(app.ets_import_preview.is_none());
        assert_eq!(
            CaptureStore::open_existing(&database)
                .unwrap()
                .ets_revision()
                .unwrap(),
            None
        );
    }

    #[test]
    fn ets_import_confirmation_stays_open_while_owner_or_action_is_busy() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        let file = directory.path().join("groups.csv");
        drop(CaptureStore::open_for_ets_import(&database).unwrap());
        std::fs::write(&file, ETS_CSV).unwrap();
        let mut app = App::new(database.clone(), true, None).unwrap();
        app.ets_import_preview = Some(preview_for(database.clone(), file));
        app.mode = Mode::ConfirmEtsImport;

        app.model.owner_available = true;
        assert!(!press(&mut app, KeyCode::Char('y')));
        assert_eq!(app.mode, Mode::ConfirmEtsImport);
        assert!(app.ets_import_preview.is_some());

        app.model.owner_available = false;
        let (_sender, receiver) = mpsc::channel();
        app.action_receiver = Some(receiver);
        assert!(!press(&mut app, KeyCode::Char('y')));
        assert_eq!(app.mode, Mode::ConfirmEtsImport);
        assert!(app.ets_import_preview.is_some());
        assert_eq!(
            CaptureStore::open_existing(&database)
                .unwrap()
                .ets_revision()
                .unwrap(),
            None
        );
    }

    #[test]
    fn confirmed_ets_import_refreshes_rows_and_blocks_database_switch_until_done() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        let other_database = directory.path().join("other.sqlite");
        let file = directory.path().join("groups.csv");
        drop(CaptureStore::open_for_ets_import(&database).unwrap());
        std::fs::write(&file, ETS_CSV).unwrap();
        let mut app = App::new(database.clone(), true, None).unwrap();
        app.model.rows.push(interface::DisplayCapture {
            id: Some(77),
            timestamp_ms: 123,
            direction: "in".into(),
            source: "1.1.1".into(),
            destination: "1/2/4".into(),
            service: "GroupValueWrite".into(),
            label: None,
            dpts: Vec::new(),
            value: None,
            raw_cemi: "29 00".into(),
        });
        app.model.filter = "1/2/4".into();
        app.selected = 0;
        app.follow_tail = false;
        app.ets_import_preview = Some(preview_for(database.clone(), file));
        app.mode = Mode::ConfirmEtsImport;

        assert!(!press(&mut app, KeyCode::Char('y')));
        assert_eq!(app.mode, Mode::EtsImportCommitting);
        assert!(app.import_commit_pending());
        assert!(!press(&mut app, KeyCode::Char('q')));
        assert!(!press(&mut app, KeyCode::Esc));
        assert!(!app.key(crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        let switch =
            app.select_endpoint_database(ConnectionSettings::default(), other_database, |_| Ok(()));
        assert!(switch.unwrap_err().contains("ETS import to finish"));
        assert_eq!(app.model.database, database);

        let deadline = Instant::now() + Duration::from_secs(3);
        while app.import_commit_pending() && Instant::now() < deadline {
            app.poll_ets_import();
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!app.import_commit_pending());
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.model.rows.len(), 1);
        assert_eq!(app.model.rows[0].id, Some(77));
        assert_eq!(app.model.rows[0].label.as_deref(), Some("Desk"));
        assert_eq!(app.model.rows[0].destination, "1/2/4");
        assert_eq!(app.model.rows[0].raw_cemi, "29 00");
        assert_eq!(app.model.filter, "1/2/4");
        assert_eq!(app.selected, 0);
        assert!(!app.follow_tail);
        let store = CaptureStore::open_existing(&database).unwrap();
        assert_eq!(store.ets_revision().unwrap(), Some(1));
        assert!(
            app.model
                .notices
                .last()
                .unwrap()
                .contains("history preserved")
        );
    }

    #[test]
    fn automatic_endpoint_submission_switches_storage_without_connecting() {
        let directory = tempfile::tempdir().unwrap();
        let source_database = directory.path().join("source.sqlite");
        let target_database = directory.path().join("endpoint.sqlite");
        drop(CaptureStore::open(&source_database, NonZeroU32::new(10).unwrap()).unwrap());
        let mut app = App::new(source_database, false, None).unwrap();
        app.model.rows.push(interface::DisplayCapture {
            id: Some(1),
            timestamp_ms: 1,
            direction: "in".into(),
            source: "1.1.1".into(),
            destination: "1/1/1".into(),
            service: "GroupValueWrite".into(),
            label: None,
            dpts: Vec::new(),
            value: None,
            raw_cemi: "29 00".into(),
        });
        app.selected = 1;
        app.follow_tail = false;
        let recent_saved = std::cell::Cell::new(false);
        let endpoint = "tunnel://192.0.2.9:3671";

        app.submit_endpoint_with(
            endpoint,
            |resolved_endpoint| {
                assert_eq!(resolved_endpoint, endpoint);
                Ok(target_database.clone())
            },
            |_| {
                recent_saved.set(true);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(app.model.database, target_database);
        assert!(app.model.rows.is_empty());
        assert_eq!(app.selected, 0);
        assert!(app.follow_tail);
        assert_eq!(
            app.settings,
            ConnectionSettings::from_endpoint(endpoint).unwrap()
        );
        assert_eq!(
            interface::load_settings(&target_database).unwrap(),
            app.settings
        );
        assert!(recent_saved.get());
        assert!(app.connection_receiver.is_none());
    }

    #[test]
    fn sustained_rows_scroll_filter_and_resize() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap());
        let mut app = App::new(database, true, None).unwrap();
        for id in 0..5_000 {
            app.model.rows.push(interface::DisplayCapture {
                id: Some(id),
                timestamp_ms: u64::try_from(id).unwrap(),
                direction: "in".into(),
                source: "1.1.1".into(),
                destination: format!("1/1/{id}"),
                service: "GroupValueWrite".into(),
                label: (id == 2_500).then(|| "kitchen light".into()),
                dpts: vec!["DPT 1.001".into()],
                value: None,
                raw_cemi: "29 00".into(),
            });
        }
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        app.selected = 4_999;
        app.draw(&mut terminal).unwrap();
        app.key(crossterm::event::KeyEvent::new(
            KeyCode::Home,
            KeyModifiers::NONE,
        ));
        assert_eq!(app.selected, 0);
        assert!(!app.follow_tail);
        app.draw(&mut terminal).unwrap();
        app.key(crossterm::event::KeyEvent::new(
            KeyCode::End,
            KeyModifiers::NONE,
        ));
        assert_eq!(app.selected, 4_999);
        assert!(app.follow_tail);
        app.key(crossterm::event::KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        ));
        assert_eq!(app.selected, 4_998);
        assert!(!app.follow_tail);
        terminal.resize(Rect::new(0, 0, 40, 12)).unwrap();
        app.draw(&mut terminal).unwrap();
        app.model.filter = "kitchen light".into();
        assert_eq!(app.visible_count(), 1);
        app.selected = 0;
        app.draw(&mut terminal).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(screen.contains("kitchen light"));
    }
}
