// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Shared, non-visual model for the terminal and desktop monitors.

use std::borrow::Cow;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use devknx::control::{ControlClient, ControlRequest, ControlResponse, RestStatus};
use devknx::enrichment::{CaptureEnrichment, enrich_capture, raw_group_value};
use devknx::ets::parse_group_address;
use devknx::ipc::{IpcClient, IpcMessage, WireState};
use devknx::operations::{OperationRequest, prepare};
use devknx::paths;
use devknx::service::LiveCapture;
use devknx::storage::CaptureStore;
use knx_rs_ip::discovery::{self, GatewayInfo};
use knx_rs_ip::{ConnectionSpec, parse_url};
use serde::{Deserialize, Serialize};

const VISIBLE_CAP: usize = 5_000;

/// One capture displayed by either interactive surface.
#[derive(Clone, Debug)]
pub struct DisplayCapture {
    pub id: Option<i64>,
    pub timestamp_ms: u64,
    pub direction: String,
    pub source: String,
    pub destination: String,
    pub service: String,
    pub label: Option<String>,
    pub dpts: Vec<String>,
    pub value: Option<String>,
    pub raw_cemi: String,
}

impl DisplayCapture {
    /// Prefer the declared-DPT value, otherwise show clearly untyped data.
    pub fn value_text(&self) -> Cow<'_, str> {
        self.value.as_deref().map_or_else(
            || {
                raw_group_value(&self.raw_cemi).map_or(Cow::Borrowed("—"), |data| {
                    Cow::Owned(format!("raw 0x{}", hex(&data)))
                })
            },
            Cow::Borrowed,
        )
    }

    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || [
                self.direction.as_str(),
                self.source.as_str(),
                self.destination.as_str(),
                self.service.as_str(),
                self.label.as_deref().unwrap_or(""),
                self.value.as_deref().unwrap_or(""),
                self.raw_cemi.as_str(),
            ]
            .iter()
            .any(|part| part.to_lowercase().contains(&query))
    }
}

/// State and bounded history shared by GUI and TUI rendering.
pub struct MonitorModel {
    pub database: PathBuf,
    pub state: WireState,
    pub owner_available: bool,
    pub rows: Vec<DisplayCapture>,
    pub notices: Vec<String>,
    pub router_lost_messages: u64,
    pub local_lag_events: u64,
    pub filter: String,
    last_seen_id: i64,
    store: CaptureStore,
}

impl MonitorModel {
    pub fn open(database: PathBuf) -> Result<Self, String> {
        let store = CaptureStore::open_existing(&database).map_err(|error| error.to_string())?;
        let mut model = Self {
            database,
            state: WireState::Idle,
            owner_available: false,
            rows: Vec::new(),
            notices: Vec::new(),
            router_lost_messages: 0,
            local_lag_events: 0,
            filter: String::new(),
            last_seen_id: 0,
            store,
        };
        model.reload_history()?;
        Ok(model)
    }

    pub fn reload_history(&mut self) -> Result<(), String> {
        let page = self
            .store
            .read_latest(NonZeroU32::new(1_000).expect("nonzero"))
            .map_err(|error| error.to_string())?;
        self.rows.clear();
        self.last_seen_id = 0;
        for stored in page {
            self.capture(IpcMessage::from(&LiveCapture {
                id: Some(stored.id),
                event: stored.event,
            }));
        }
        Ok(())
    }

    pub fn catch_up(&mut self) -> Result<(), String> {
        // Reading history first and subscribing second leaves a short race.
        // Reconcile by monotonic ID after the IPC subscription is established.
        // A cap prevents a continuously busy writer from trapping the UI here.
        for _ in 0..100 {
            let page = self
                .store
                .read_after(self.last_seen_id, NonZeroU32::new(1_000).expect("nonzero"))
                .map_err(|error| error.to_string())?;
            let count = page.len();
            for stored in page {
                self.capture(IpcMessage::from(&LiveCapture {
                    id: Some(stored.id),
                    event: stored.event,
                }));
            }
            if count < 1_000 {
                return Ok(());
            }
        }
        self.notice("History catch-up reached its page limit; reload if needed".into());
        Ok(())
    }

    pub fn load_older(&mut self) -> Result<usize, String> {
        let Some(oldest_id) = self.rows.iter().find_map(|row| row.id) else {
            return Ok(0);
        };
        let page = self
            .store
            .read_before(oldest_id, NonZeroU32::new(500).expect("nonzero"))
            .map_err(|error| error.to_string())?;
        let count = page.len();
        let current = std::mem::take(&mut self.rows);
        for stored in page {
            self.capture(IpcMessage::from(&LiveCapture {
                id: Some(stored.id),
                event: stored.event,
            }));
        }
        self.rows.extend(current);
        self.rows.truncate(VISIBLE_CAP);
        Ok(count)
    }

    pub fn ingest(&mut self, message: IpcMessage) {
        match message {
            IpcMessage::State { value, .. } => {
                self.owner_available = true;
                self.state = value;
                if let Err(error) = self.catch_up() {
                    self.notice(format!("History catch-up failed: {error}"));
                }
            }
            IpcMessage::Capture { id: Some(id), .. } if id <= self.last_seen_id => {}
            IpcMessage::Capture { .. } => self.capture(message),
            IpcMessage::RoutingLostMessage {
                source,
                lost_messages,
                device_state,
                ..
            } => {
                self.router_lost_messages += u64::from(lost_messages);
                self.notice(format!(
                    "Router {source} reported {lost_messages} lost routing frames (device state {device_state})"
                ));
            }
            IpcMessage::Lagged { stream, count } => {
                self.local_lag_events += count;
                self.notice(format!("Local {stream:?} subscriber missed {count} events"));
                if matches!(stream, devknx::ipc::LaggedStream::Capture) {
                    if let Err(error) = self.catch_up() {
                        self.notice(format!("History catch-up failed: {error}"));
                    }
                } else {
                    self.notice(
                        "Use `devknx router-losses` for durable router-loss history".into(),
                    );
                }
            }
            IpcMessage::OperationResult { audit_id, read, .. } => {
                self.notice(format!(
                    "Operation transmitted (audit {audit_id}); read={read:?}"
                ));
            }
            IpcMessage::OperationError { reason } => self.notice(reason),
        }
    }

    fn capture(&mut self, message: IpcMessage) {
        let IpcMessage::Capture { id, .. } = &message else {
            return;
        };
        if id.is_some() && self.rows.iter().any(|row| row.id == *id) {
            return;
        }
        let enrichment = match enrich_capture(&message, &self.store) {
            Ok(enrichment) => enrichment,
            Err(error) => {
                self.notice(format!("Capture enrichment failed: {error}"));
                CaptureEnrichment::default()
            }
        };
        let IpcMessage::Capture {
            id,
            observed_at_ms,
            direction,
            source,
            destination,
            service,
            raw_cemi,
            ..
        } = message
        else {
            return;
        };
        if let Some(id) = id {
            self.last_seen_id = self.last_seen_id.max(id);
        }
        self.rows.push(DisplayCapture {
            id,
            timestamp_ms: observed_at_ms,
            direction,
            source,
            destination,
            service,
            label: enrichment.group_name,
            value: enrichment.value,
            dpts: enrichment.dpts,
            raw_cemi,
        });
        if self.rows.len() > VISIBLE_CAP {
            self.rows.drain(..self.rows.len() - VISIBLE_CAP);
        }
    }

    pub fn notice(&mut self, notice: String) {
        if self.notices.last() == Some(&notice) {
            return;
        }
        self.notices.push(notice);
        if self.notices.len() > 100 {
            self.notices.remove(0);
        }
    }

    pub fn owner_unavailable(&mut self, error: &str) {
        let was_available = self.owner_available;
        self.owner_available = false;
        self.state = WireState::WaitingRetry {
            reason: "capture owner unavailable".to_owned(),
            delay_ms: 2_000,
        };
        if was_available {
            self.notice(if error == "capture owner closed IPC stream" {
                "Capture connection closed".into()
            } else {
                format!("Capture connection unavailable: {error}")
            });
        }
    }

    pub fn preview_write(&self, address: &str, dpt: &str, value: &str) -> Result<String, String> {
        let address = parse_group_address(address).map_err(|error| error.to_string())?;
        let group = self
            .store
            .ets_group(address)
            .map_err(|error| error.to_string())?;
        let prepared = prepare(
            OperationRequest::TypedWrite {
                address_raw: address.raw(),
                dpt: if dpt.trim().is_empty() {
                    None
                } else {
                    Some(dpt.trim().to_owned())
                },
                value: value.to_owned(),
            },
            group.as_ref(),
        )
        .map_err(|error| error.to_string())?;
        Ok(format!(
            "DPT {} · cEMI {} · not transmitted",
            prepared.dpt.expect("typed write has DPT"),
            hex(prepared.frame.as_bytes())
        ))
    }

    #[cfg(feature = "gui")]
    pub fn ets_for_address(&self, address: &str) -> Result<Option<(String, Vec<String>)>, String> {
        let address = parse_group_address(address).map_err(|error| error.to_string())?;
        Ok(self
            .store
            .ets_group(address)
            .map_err(|error| error.to_string())?
            .map(|group| (group.name, group.dpts)))
    }
}

/// Connection settings shared by the interactive surfaces. No credential is stored.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionSettings {
    pub mode: ConnectionMode,
    pub address: String,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionMode {
    #[default]
    Tunnel,
    Routing,
}

impl Default for ConnectionSettings {
    fn default() -> Self {
        Self {
            mode: ConnectionMode::Tunnel,
            address: String::new(),
            port: 3671,
        }
    }
}

impl ConnectionSettings {
    #[cfg(any(feature = "tui", test))]
    pub fn from_endpoint(endpoint: &str) -> Result<Self, String> {
        let spec = parse_url(endpoint.trim()).map_err(|error| error.to_string())?;
        let (mode, address) = match spec {
            ConnectionSpec::Tunnel(address) => (ConnectionMode::Tunnel, address),
            ConnectionSpec::Router(address) => (ConnectionMode::Routing, address),
        };
        let settings = Self {
            mode,
            address: address.ip().to_string(),
            port: address.port(),
        };
        settings.endpoint()?;
        Ok(settings)
    }

    pub fn endpoint(&self) -> Result<String, String> {
        let address_error = match self.mode {
            ConnectionMode::Tunnel => "Enter a numeric gateway IP address",
            ConnectionMode::Routing => "Enter a numeric multicast group address",
        };
        let ip: IpAddr = self
            .address
            .trim()
            .parse()
            .map_err(|_| address_error.to_owned())?;
        if self.port == 0 {
            return Err("The KNXnet/IP port must be nonzero".into());
        }
        let scheme = match self.mode {
            ConnectionMode::Tunnel if ip.is_multicast() => {
                return Err("A tunnel needs a unicast gateway address".into());
            }
            ConnectionMode::Routing if !matches!(ip, IpAddr::V4(address) if address.is_multicast()) =>
            {
                return Err("Routing needs a multicast group address".into());
            }
            ConnectionMode::Tunnel => "tunnel",
            ConnectionMode::Routing => "router",
        };
        let endpoint = format!("{scheme}://{}", SocketAddr::new(ip, self.port));
        let spec = parse_url(&endpoint).map_err(|error| error.to_string())?;
        if !matches!(
            (&self.mode, spec),
            (ConnectionMode::Tunnel, ConnectionSpec::Tunnel(_))
                | (ConnectionMode::Routing, ConnectionSpec::Router(_))
        ) {
            return Err("Connection mode and address disagree".into());
        }
        Ok(endpoint)
    }
}

pub fn ensure_database(database: &Path) -> Result<(), String> {
    if database.exists() {
        return Ok(());
    }
    let parent = database
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure_private_directory(parent)?;
    drop(
        CaptureStore::open(database, NonZeroU32::new(100_000).expect("nonzero"))
            .map_err(|error| error.to_string())?,
    );
    Ok(())
}

pub fn load_settings(database: &Path) -> Result<ConnectionSettings, String> {
    let path = paths::connection_settings_file(database);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if paths::legacy_database().ok().as_deref() == Some(database) {
                load_recent_settings()
            } else {
                Ok(ConnectionSettings::default())
            }
        }
        Err(error) => Err(error.to_string()),
    }
}

pub fn save_settings(database: &Path, settings: &ConnectionSettings) -> Result<(), String> {
    write_settings(&paths::connection_settings_file(database), settings)
}

pub fn load_recent_settings() -> Result<ConnectionSettings, String> {
    let path = paths::recent_connection_file()?;
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ConnectionSettings::default())
        }
        Err(error) => Err(error.to_string()),
    }
}

pub fn save_recent_settings(settings: &ConnectionSettings) -> Result<(), String> {
    write_settings(&paths::recent_connection_file()?, settings)
}

fn write_settings(path: &Path, settings: &ConnectionSettings) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure_private_directory(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    serde_json::to_writer_pretty(&mut file, settings).map_err(|error| error.to_string())?;
    file.persist(path).map_err(|error| error.to_string())?;
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path).map_err(|error| error.to_string())?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path).map_err(|error| error.to_string())?;
    Ok(())
}

pub fn connect_owner(database: &Path, endpoint: &str) -> Result<String, String> {
    let endpoint = paths::canonical_endpoint(endpoint)?;
    let database = std::path::absolute(database).map_err(|error| error.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        ControlClient::ensure_daemon()
            .await
            .map_err(|error| error.to_string())?;
        match ControlClient::request_existing(ControlRequest::Connect {
            endpoint: endpoint.clone(),
            database: Some(database),
            max_events: 100_000,
        })
        .await
        .map_err(|error| error.to_string())?
        {
            ControlResponse::Session { session } => Ok(format!(
                "Connected to {} using {}",
                session.endpoint,
                session.database.display()
            )),
            ControlResponse::Error { reason } => Err(reason),
            _ => Err("Capture daemon returned an unexpected connection response".into()),
        }
    })
}

pub fn disconnect_owner(database: &Path) -> Result<String, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    // Only the live database-scoped IPC owner can identify the session to
    // disconnect. A saved sidecar may be stale or describe a different owner.
    let endpoint = runtime.block_on(configured_endpoint(database));
    let Some(endpoint) = endpoint else {
        return Ok("No active capture connection".into());
    };
    let endpoint = paths::canonical_endpoint(&endpoint)?;
    match runtime.block_on(ControlClient::request_existing(
        ControlRequest::Disconnect {
            endpoint: endpoint.clone(),
        },
    )) {
        Ok(ControlResponse::Disconnected) => Ok(format!("Disconnected from {endpoint}")),
        Ok(ControlResponse::Error { reason }) => Err(reason),
        Ok(_) => Err("Capture daemon returned an unexpected disconnect response".into()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok("No active capture connection".into())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn control_request(request: ControlRequest) -> io::Result<ControlResponse> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(ControlClient::request_existing(request))
}

fn control_request_start(request: ControlRequest) -> io::Result<ControlResponse> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(ControlClient::request(request))
}

pub fn rest_status() -> Result<RestStatus, String> {
    match control_request(ControlRequest::RestStatus) {
        Ok(ControlResponse::Rest { status }) => Ok(status),
        Ok(ControlResponse::Error { reason }) => Err(reason),
        Ok(_) => Err("Capture daemon returned an unexpected REST response".into()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(RestStatus {
                enabled: false,
                endpoint: None,
                bind: None,
                allow_remote_writes: false,
            })
        }
        Err(error) => Err(error.to_string()),
    }
}

pub fn rest_enable(
    endpoint: Option<&str>,
    database: Option<PathBuf>,
    bind: &str,
    token: Option<String>,
    allow_remote_writes: bool,
) -> Result<RestStatus, String> {
    let endpoint = endpoint.map(paths::canonical_endpoint).transpose()?;
    if endpoint.is_none() && database.is_some() {
        return Err("REST database override requires an endpoint".into());
    }
    let database = database
        .map(std::path::absolute)
        .transpose()
        .map_err(|error| error.to_string())?;
    let bind: SocketAddr = bind
        .parse()
        .map_err(|_| "Enter a valid REST listen address".to_owned())?;
    match control_request_start(ControlRequest::RestEnable {
        endpoint,
        database,
        bind,
        token,
        allow_remote_writes,
    })
    .map_err(|error| error.to_string())?
    {
        ControlResponse::Rest { status } => Ok(status),
        ControlResponse::Error { reason } => Err(reason),
        _ => Err("Capture daemon returned an unexpected REST response".into()),
    }
}

pub fn rest_disable() -> Result<RestStatus, String> {
    match control_request(ControlRequest::RestDisable).map_err(|error| error.to_string())? {
        ControlResponse::Rest { status } => Ok(status),
        ControlResponse::Error { reason } => Err(reason),
        _ => Err("Capture daemon returned an unexpected REST response".into()),
    }
}

async fn configured_endpoint(database: &Path) -> Option<String> {
    let mut client = IpcClient::connect(database, false).await.ok()?;
    let message = tokio::time::timeout(Duration::from_secs(2), client.next())
        .await
        .ok()?
        .ok()??;
    match message {
        IpcMessage::State {
            value,
            configured_endpoint,
        } => endpoint_from_status(&value, configured_endpoint),
        _ => None,
    }
}

fn endpoint_from_status(state: &WireState, configured_endpoint: Option<String>) -> Option<String> {
    configured_endpoint.or_else(|| match state {
        WireState::Connected { endpoint } => Some(endpoint.clone()),
        _ => None,
    })
}

pub fn format_time(timestamp_ms: u64) -> String {
    use chrono::{Local, TimeZone as _};
    i64::try_from(timestamp_ms)
        .ok()
        .and_then(|timestamp| Local.timestamp_millis_opt(timestamp).single())
        .map_or_else(
            || "—".into(),
            |time| time.format("%H:%M:%S%.3f").to_string(),
        )
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(result, "{byte:02x}").expect("String write");
    }
    result
}

/// Background subscription, reconnecting to the same current-user IPC owner.
pub struct Follower {
    active: Arc<AtomicBool>,
    pub receiver: Receiver<Result<IpcMessage, String>>,
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
    }
}

impl Follower {
    pub fn start(database: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel();
        let active = Arc::new(AtomicBool::new(true));
        let worker_active = Arc::clone(&active);
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    return;
                }
            };
            runtime.block_on(async {
                while worker_active.load(Ordering::Relaxed) {
                    match IpcClient::connect(&database, true).await {
                        Ok(mut client) => loop {
                            if !worker_active.load(Ordering::Relaxed) {
                                return;
                            }
                            match tokio::time::timeout(Duration::from_millis(500), client.next())
                                .await
                            {
                                Err(_) => {}
                                Ok(Ok(Some(message))) => {
                                    if sender.send(Ok(message)).is_err() {
                                        return;
                                    }
                                }
                                Ok(Ok(None)) => {
                                    let _ =
                                        sender.send(Err("capture owner closed IPC stream".into()));
                                    break;
                                }
                                Ok(Err(error)) => {
                                    let _ = sender.send(Err(error.to_string()));
                                    break;
                                }
                            }
                        },
                        Err(error) => {
                            if sender.send(Err(error.to_string())).is_err() {
                                return;
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            });
        });
        Self { active, receiver }
    }
}

pub fn operate(database: &Path, request: &OperationRequest) -> Result<IpcMessage, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?
        .block_on(IpcClient::operate(database, request))
        .map_err(|error| error.to_string())
}

pub fn read_request(address: &str) -> Result<OperationRequest, String> {
    let address = parse_group_address(address).map_err(|error| error.to_string())?;
    Ok(OperationRequest::Read {
        address_raw: address.raw(),
        timeout_ms: 2_000,
    })
}

pub fn write_request(address: &str, dpt: &str, value: &str) -> Result<OperationRequest, String> {
    let address = parse_group_address(address).map_err(|error| error.to_string())?;
    Ok(OperationRequest::TypedWrite {
        address_raw: address.raw(),
        dpt: (!dpt.trim().is_empty()).then(|| dpt.trim().to_owned()),
        value: value.to_owned(),
    })
}

pub fn export_csv(database: &Path, output: &Path) -> Result<u64, String> {
    let store = CaptureStore::open_existing(database).map_err(|error| error.to_string())?;
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    let count = store
        .export_csv(&mut temporary, 0)
        .map_err(|error| error.to_string())?;
    temporary
        .persist_noclobber(output)
        .map_err(|error| error.to_string())?;
    Ok(count)
}

pub fn discover_gateways() -> Result<Vec<GatewayInfo>, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?
        .block_on(discovery::discover(Ipv4Addr::UNSPECIFIED))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use devknx::capture::{CaptureEndpoint, CaptureEvent};
    use devknx::ets::{CsvEncoding, EtsCatalog, EtsFormat};
    use devknx::ipc::IpcServer;
    use devknx::service::ConnectionState;
    use knx_rs_core::address::{DestinationAddress, GroupAddress, IndividualAddress};
    use knx_rs_core::cemi::CemiFrame;
    use knx_rs_core::message::MessageCode;
    use knx_rs_core::types::Priority;
    use tokio::sync::{broadcast, mpsc, watch};

    #[test]
    fn filters_include_ets_labels_and_raw_details() {
        let row = DisplayCapture {
            id: Some(1),
            timestamp_ms: 1,
            direction: "received".into(),
            source: "1.1.1".into(),
            destination: "1/2/3".into(),
            service: "Write".into(),
            label: Some("Kitchen light".into()),
            dpts: vec!["DPT-1-1".into()],
            value: Some("true".into()),
            raw_cemi: "2900".into(),
        };
        assert!(row.matches("kitchen"));
        assert!(row.matches("2900"));
        assert!(!row.matches("bedroom"));
        assert_eq!(row.value_text(), "true");
        let mut raw = row;
        raw.value = None;
        raw.raw_cemi = "2900bce0112b29000300800c56".into();
        assert_eq!(raw.value_text(), "raw 0x0c56");
        raw.raw_cemi = "2900".into();
        assert_eq!(raw.value_text(), "—");
    }

    #[test]
    fn request_parsing_rejects_invalid_addresses() {
        assert!(read_request("99/9/999").is_err());
        assert!(write_request("99/9/999", "1.001", "true").is_err());
    }

    #[test]
    fn connection_settings_validate_mode_and_address_without_network_access() {
        let start = ConnectionSettings::default();
        assert_eq!(start.mode, ConnectionMode::Tunnel);
        assert_eq!(start.port, 3671);
        let tunnel = ConnectionSettings::from_endpoint("tunnel://192.168.2.8:3671").unwrap();
        assert_eq!(tunnel.mode, ConnectionMode::Tunnel);
        assert_eq!(tunnel.endpoint().unwrap(), "tunnel://192.168.2.8:3671");
        let router = ConnectionSettings::from_endpoint("router://224.0.23.12:3671").unwrap();
        assert_eq!(router.mode, ConnectionMode::Routing);
        assert_eq!(router.endpoint().unwrap(), "router://224.0.23.12:3671");
        assert!(ConnectionSettings::from_endpoint("router://192.168.2.8:3671").is_err());
        assert!(ConnectionSettings::from_endpoint("tunnel://224.0.23.12:3671").is_err());
        assert!(ConnectionSettings::from_endpoint("tunnel://not-an-ip:3671").is_err());
        assert_eq!(
            ConnectionSettings {
                mode: ConnectionMode::Routing,
                address: "invalid".into(),
                port: 3671,
            }
            .endpoint()
            .unwrap_err(),
            "Enter a numeric multicast group address"
        );
    }

    #[test]
    fn disconnect_endpoint_uses_configured_identity_during_reconnect() {
        let configured = "tunnel://192.0.2.8:3671";
        let other = "tunnel://192.0.2.9:3671";
        let retry = WireState::WaitingRetry {
            reason: "gateway unavailable".into(),
            delay_ms: 2_000,
        };
        assert_eq!(
            endpoint_from_status(&retry, Some(configured.into())).as_deref(),
            Some(configured)
        );
        assert_eq!(
            endpoint_from_status(&retry, Some(other.into())).as_deref(),
            Some(other)
        );
        assert_eq!(endpoint_from_status(&retry, None), None);
        assert_eq!(
            endpoint_from_status(
                &WireState::Connected {
                    endpoint: configured.into(),
                },
                None,
            )
            .as_deref(),
            Some(configured)
        );
    }

    #[test]
    fn private_capture_storage_and_connection_settings_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("application").join("captures.sqlite");
        ensure_database(&database).unwrap();
        assert!(database.exists());
        assert!(MonitorModel::open(database.clone()).is_ok());
        let settings = ConnectionSettings::from_endpoint("tunnel://192.0.2.8:3671").unwrap();
        save_settings(&database, &settings).unwrap();
        assert_eq!(load_settings(&database).unwrap(), settings);
        ensure_database(&database).unwrap();
        assert_eq!(load_settings(&database).unwrap(), settings);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(database.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
        }
    }

    #[test]
    fn connection_settings_are_scoped_to_each_database() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.sqlite");
        let second = directory.path().join("second.sqlite");
        let first_settings = ConnectionSettings::from_endpoint("tunnel://192.0.2.8:3671").unwrap();
        let second_settings =
            ConnectionSettings::from_endpoint("router://224.0.23.12:3671").unwrap();
        save_settings(&first, &first_settings).unwrap();
        save_settings(&second, &second_settings).unwrap();
        assert_eq!(load_settings(&first).unwrap(), first_settings);
        assert_eq!(load_settings(&second).unwrap(), second_settings);
        assert!(!directory.path().join("connection.json").exists());
    }

    #[test]
    fn history_enrichment_preview_and_export_use_one_database() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        let mut store = CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap();
        let catalog = EtsCatalog::from_bytes(
            br#"<GroupAddress-Export xmlns="http://knx.org/xml/ga-export/01"><GroupRange Name="Lighting"><GroupAddress Name="Kitchen light" Address="1/2/3" DPTs="DPST-1-1" /></GroupRange></GroupAddress-Export>"#,
            EtsFormat::GaXml01,
            CsvEncoding::Utf8,
        ).unwrap();
        store.import_ets(&catalog).unwrap();
        let frame = CemiFrame::new_l_data(
            MessageCode::LDataInd,
            IndividualAddress::from_raw(0x1101),
            DestinationAddress::Group(GroupAddress::from_raw(0x0a03)),
            Priority::Low,
            &[0, 0x80, 1],
        );
        store
            .insert(&CaptureEvent::received(
                CaptureEndpoint::Tunnel("192.0.2.1:3671".parse().unwrap()),
                frame,
            ))
            .unwrap();
        drop(store);

        let mut model = MonitorModel::open(database.clone()).unwrap();
        assert_eq!(model.rows.len(), 1);
        assert_eq!(model.rows[0].label.as_deref(), Some("Kitchen light"));
        assert_eq!(model.rows[0].value.as_deref(), Some("true"));
        let mut writer = CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap();
        let late_frame = CemiFrame::new_l_data(
            MessageCode::LDataInd,
            IndividualAddress::from_raw(0x1101),
            DestinationAddress::Group(GroupAddress::from_raw(0x0a03)),
            Priority::Low,
            &[0, 0x80, 0],
        );
        writer
            .insert(&CaptureEvent::received(
                CaptureEndpoint::Tunnel("192.0.2.1:3671".parse().unwrap()),
                late_frame,
            ))
            .unwrap();
        drop(writer);
        model.ingest(IpcMessage::State {
            value: WireState::Idle,
            configured_endpoint: None,
        });
        assert_eq!(
            model.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [Some(1), Some(2)]
        );
        assert!(
            model
                .preview_write("1/2/3", "", "true")
                .unwrap()
                .contains("not transmitted")
        );
        assert!(model.preview_write("1/2/3", "", "nonsense").is_err());
        let output = directory.path().join("export.csv");
        assert_eq!(export_csv(&database, &output).unwrap(), 2);
        let first = std::fs::read(&output).unwrap();
        assert!(export_csv(&database, &output).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), first);
    }

    #[tokio::test]
    async fn follower_reconnects_after_owner_restart() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap());
        let first = IpcServer::bind(&database).unwrap();
        let (state_tx, state_rx) = watch::channel(ConnectionState::Idle);
        let (frames_tx, frames_rx) = broadcast::channel(8);
        let (losses_tx, losses_rx) = broadcast::channel(8);
        let (operations_tx, _operations_rx) = mpsc::channel(8);
        let server = tokio::spawn(async move {
            let _ = first
                .run(state_rx, frames_rx, losses_rx, operations_tx)
                .await;
        });
        let follower = Follower::start(database.clone());
        let mut saw_idle = false;
        for _ in 0..30 {
            while let Ok(event) = follower.receiver.try_recv() {
                if matches!(
                    event,
                    Ok(IpcMessage::State {
                        value: WireState::Idle,
                        ..
                    })
                ) {
                    saw_idle = true;
                }
            }
            if saw_idle {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(saw_idle, "subscriber should see the first owner");
        server.abort();
        let _ = server.await;
        drop(state_tx);
        drop(frames_tx);
        drop(losses_tx);
        let mut saw_disconnect = false;
        for _ in 0..30 {
            while let Ok(event) = follower.receiver.try_recv() {
                if event.is_err() {
                    saw_disconnect = true;
                }
            }
            if saw_disconnect {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(saw_disconnect, "subscriber should report owner shutdown");

        let second = IpcServer::bind(&database).unwrap();
        let (state_tx, state_rx) = watch::channel(ConnectionState::Connected {
            endpoint: CaptureEndpoint::Tunnel("192.0.2.1:3671".parse().unwrap()),
        });
        let (frames_tx, frames_rx) = broadcast::channel(8);
        let (losses_tx, losses_rx) = broadcast::channel(8);
        let (operations_tx, _operations_rx) = mpsc::channel(8);
        let server = tokio::spawn(async move {
            let _ = second
                .run(state_rx, frames_rx, losses_rx, operations_tx)
                .await;
        });
        let mut reconnected = false;
        for _ in 0..60 {
            while let Ok(event) = follower.receiver.try_recv() {
                if matches!(
                    event,
                    Ok(IpcMessage::State {
                        value: WireState::Connected { .. },
                        ..
                    })
                ) {
                    reconnected = true;
                }
            }
            if reconnected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            reconnected,
            "subscriber should reconnect to the replacement owner"
        );
        drop(follower);
        server.abort();
        drop((state_tx, frames_tx, losses_tx));
    }

    #[test]
    fn closed_capture_owner_has_a_readable_notice() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap());
        let mut model = MonitorModel::open(database).unwrap();
        model.owner_available = true;
        model.owner_unavailable("capture owner closed IPC stream");
        assert_eq!(
            model.notices.last().map(String::as_str),
            Some("Capture connection closed")
        );
    }

    #[test]
    fn sustained_capture_keeps_a_bounded_visible_window_without_duplicates() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap());
        let mut model = MonitorModel::open(database).unwrap();
        for id in 1..=5_500 {
            model.ingest(IpcMessage::Capture {
                id: Some(id),
                observed_at_ms: u64::try_from(id).unwrap(),
                endpoint: "tunnel://192.0.2.1:3671".into(),
                direction: "received".into(),
                source: "1.1.1".into(),
                destination: "1.1.2".into(),
                service: "Write".into(),
                raw_cemi: "2900".into(),
            });
        }
        assert_eq!(model.rows.len(), VISIBLE_CAP);
        assert_eq!(model.rows.first().unwrap().id, Some(501));
        assert_eq!(model.rows.last().unwrap().id, Some(5_500));
        model.ingest(IpcMessage::Capture {
            id: Some(5_500),
            observed_at_ms: 5_500,
            endpoint: "tunnel://192.0.2.1:3671".into(),
            direction: "received".into(),
            source: "1.1.1".into(),
            destination: "1.1.2".into(),
            service: "Write".into(),
            raw_cemi: "2900".into(),
        });
        assert_eq!(model.rows.len(), VISIBLE_CAP);
    }

    #[test]
    fn history_pages_backward_without_repeating_or_reordering_rows() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        let mut store = CaptureStore::open(&database, NonZeroU32::new(1_500).unwrap()).unwrap();
        let frame = CemiFrame::new_l_data(
            MessageCode::LDataInd,
            IndividualAddress::from_raw(0x1101),
            DestinationAddress::Group(GroupAddress::from_raw(0x0801)),
            Priority::Low,
            &[0, 0x80, 1],
        );
        let event = CaptureEvent::received(
            CaptureEndpoint::Tunnel("192.0.2.1:3671".parse().unwrap()),
            frame,
        );
        for _ in 0..1_200 {
            store.insert(&event).unwrap();
        }
        drop(store);
        let mut model = MonitorModel::open(database).unwrap();
        assert_eq!(model.rows.len(), 1_000);
        assert_eq!(model.rows.first().unwrap().id, Some(201));
        assert_eq!(model.load_older().unwrap(), 200);
        assert_eq!(model.rows.first().unwrap().id, Some(1));
        assert_eq!(model.rows.last().unwrap().id, Some(1_200));
        assert_eq!(model.load_older().unwrap(), 0);
    }
}
