// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Shared, non-visual model for the terminal and desktop monitors.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use devknx::ets::parse_group_address;
use devknx::ipc::{IpcClient, IpcMessage, WireState};
use devknx::operations::{OperationRequest, prepare};
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
        if id.is_some() && self.rows.iter().any(|row| row.id == id) {
            return;
        }
        if let Some(id) = id {
            self.last_seen_id = self.last_seen_id.max(id);
        }
        let group = if destination.contains('/') {
            parse_group_address(&destination)
                .ok()
                .and_then(|address| self.store.ets_group(address).ok().flatten())
        } else {
            None
        };
        let dpts = group
            .as_ref()
            .map_or_else(Vec::new, |group| group.dpts.clone());
        let value = decode_capture_value(&raw_cemi, &dpts, &service);
        self.rows.push(DisplayCapture {
            id,
            timestamp_ms: observed_at_ms,
            direction,
            source,
            destination,
            service,
            label: group.as_ref().map(|group| group.name.clone()),
            value,
            dpts,
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
            self.notice(format!("Capture service disconnected: {error}"));
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

/// Connection profile shared by the interactive surfaces. No credential is stored.
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
        let ip: IpAddr = self
            .address
            .trim()
            .parse()
            .map_err(|_| "Enter a numeric gateway IP address".to_owned())?;
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

pub fn default_database() -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Application Support"));
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".local/share"))
        });
    let base = base.ok_or("Cannot find the current user's application data directory")?;
    Ok(base.join("devknx").join("captures.sqlite"))
}

pub fn ensure_database(database: &Path) -> Result<(), String> {
    if database.exists() {
        return Ok(());
    }
    let parent = database
        .parent()
        .ok_or("Capture path has no parent directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(parent).map_err(|error| error.to_string())?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    drop(
        CaptureStore::open(database, NonZeroU32::new(100_000).expect("nonzero"))
            .map_err(|error| error.to_string())?,
    );
    Ok(())
}

pub fn load_settings(database: &Path) -> Result<ConnectionSettings, String> {
    let path = database.with_file_name("connection.json");
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ConnectionSettings::default())
        }
        Err(error) => Err(error.to_string()),
    }
}

pub fn save_settings(database: &Path, settings: &ConnectionSettings) -> Result<(), String> {
    let path = database.with_file_name("connection.json");
    let parent = path
        .parent()
        .ok_or("Settings path has no parent directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    serde_json::to_writer_pretty(&mut file, settings).map_err(|error| error.to_string())?;
    file.persist(&path).map_err(|error| error.to_string())?;
    Ok(())
}

pub fn connect_owner(database: &Path, endpoint: &str) -> Result<String, String> {
    parse_url(endpoint).map_err(|error| error.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    if let Ok(mut client) = runtime.block_on(IpcClient::connect(database, false)) {
        let state = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), client.next()).await });
        match state {
            Ok(Ok(Some(IpcMessage::State {
                value,
                configured_endpoint,
            }))) => {
                verify_owner_endpoint(&value, configured_endpoint.as_deref(), endpoint)?;
                return Ok("Attached to the active capture service".into());
            }
            _ => return Err("A capture owner is present but did not report its state".into()),
        }
    }
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut child = Command::new(executable)
        .arg("serve")
        .arg(endpoint)
        .arg("--database")
        .arg(database)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Could not start capture service: {error}"))?;
    for _ in 0..50 {
        if let Ok(mut client) = runtime.block_on(IpcClient::connect(database, false)) {
            let state = runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(2), client.next()).await
            });
            if let Ok(Ok(Some(IpcMessage::State {
                value,
                configured_endpoint,
            }))) = state
            {
                match verify_owner_endpoint(&value, configured_endpoint.as_deref(), endpoint) {
                    Ok(()) => return Ok(format!("Capture service started for {endpoint}")),
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(error);
                    }
                }
            }
        }
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Err(format!("Capture service exited during startup ({status})"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err("Capture service did not start within five seconds".into())
}

fn verify_owner_endpoint(
    state: &WireState,
    configured_endpoint: Option<&str>,
    requested_endpoint: &str,
) -> Result<(), String> {
    let active = configured_endpoint.or(match state {
        WireState::Connected { endpoint } => Some(endpoint.as_str()),
        _ => None,
    });
    match active {
        Some(endpoint) if endpoint == requested_endpoint => Ok(()),
        Some(endpoint) => Err(format!(
            "Capture is already configured for {endpoint}. Disconnect it before switching gateways."
        )),
        None => Err("A capture owner is present but did not report its configured endpoint".into()),
    }
}

pub fn disconnect_owner(database: &Path) -> Result<String, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime
        .block_on(IpcClient::stop(database))
        .map_err(|error| error.to_string())?;
    Ok("Capture service stopped".into())
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

fn decode_capture_value(raw_cemi: &str, dpts: &[String], service: &str) -> Option<String> {
    if service == "Read" {
        return Some("Read request".into());
    }
    if !matches!(service, "Write" | "Response")
        || dpts.len() != 1
        || !raw_cemi.len().is_multiple_of(2)
    {
        return None;
    }
    let bytes = raw_cemi
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<Vec<_>>>()?;
    let frame = knx_rs_core::cemi::CemiFrame::parse(&bytes).ok()?;
    let apdu = frame.tpdu()?.apdu()?.clone();
    let dpt = devknx::ets::parse_dpt(&dpts[0]).ok()?;
    knx_rs_core::dpt::decode(dpt, &apdu.data)
        .ok()
        .map(|value| value.to_string())
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
    }

    #[test]
    fn request_parsing_rejects_invalid_addresses() {
        assert!(read_request("99/9/999").is_err());
        assert!(write_request("99/9/999", "1.001", "true").is_err());
    }

    #[test]
    fn connection_profiles_validate_mode_and_address_without_network_access() {
        let tunnel = ConnectionSettings::from_endpoint("tunnel://192.168.2.8:3671").unwrap();
        assert_eq!(tunnel.mode, ConnectionMode::Tunnel);
        assert_eq!(tunnel.endpoint().unwrap(), "tunnel://192.168.2.8:3671");
        let router = ConnectionSettings::from_endpoint("router://224.0.23.12:3671").unwrap();
        assert_eq!(router.mode, ConnectionMode::Routing);
        assert_eq!(router.endpoint().unwrap(), "router://224.0.23.12:3671");
        assert!(ConnectionSettings::from_endpoint("router://192.168.2.8:3671").is_err());
        assert!(ConnectionSettings::from_endpoint("tunnel://224.0.23.12:3671").is_err());
        assert!(ConnectionSettings::from_endpoint("tunnel://not-an-ip:3671").is_err());
    }

    #[test]
    fn owner_identity_is_checked_even_during_reconnect() {
        let requested = "tunnel://192.0.2.8:3671";
        let other = "tunnel://192.0.2.9:3671";
        let retry = WireState::WaitingRetry {
            reason: "gateway unavailable".into(),
            delay_ms: 2_000,
        };
        assert!(verify_owner_endpoint(&retry, Some(requested), requested).is_ok());
        assert!(verify_owner_endpoint(&retry, Some(other), requested).is_err());
        assert!(verify_owner_endpoint(&retry, None, requested).is_err());
        assert!(verify_owner_endpoint(&WireState::Idle, Some(other), requested).is_err());
        assert!(
            verify_owner_endpoint(
                &WireState::Connecting { attempt: 1 },
                Some(other),
                requested
            )
            .is_err()
        );
        assert!(
            verify_owner_endpoint(
                &WireState::Connected {
                    endpoint: requested.into()
                },
                None,
                requested
            )
            .is_ok()
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
