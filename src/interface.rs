// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Shared, non-visual model for the terminal and desktop monitors.

use std::net::Ipv4Addr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
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
    pub rows: Vec<DisplayCapture>,
    pub notices: Vec<String>,
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
            rows: Vec::new(),
            notices: Vec::new(),
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
            IpcMessage::State { value } => {
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
            } => self.notice(format!(
                "Router {source} reported {lost_messages} lost routing frames (device state {device_state})"
            )),
            IpcMessage::Lagged { stream, count } => {
                self.notice(format!("Local {stream:?} subscriber missed {count} events"));
                if matches!(stream, devknx::ipc::LaggedStream::Capture) {
                    if let Err(error) = self.catch_up() {
                        self.notice(format!("History catch-up failed: {error}"));
                    }
                } else {
                    self.notice("Use `devknx router-losses` for durable router-loss history".into());
                }
            }
            IpcMessage::OperationResult { audit_id, read, .. } => {
                self.notice(format!("Operation transmitted (audit {audit_id}); read={read:?}"));
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
        self.rows.push(DisplayCapture {
            id,
            timestamp_ms: observed_at_ms,
            direction,
            source,
            destination,
            service,
            label: group.as_ref().map(|group| group.name.clone()),
            dpts: group.map_or_else(Vec::new, |group| group.dpts),
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
        self.state = WireState::WaitingRetry {
            reason: "capture owner unavailable".to_owned(),
            delay_ms: 2_000,
        };
        self.notice(format!("Capture owner unavailable: {error}"));
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

    pub fn ets_for_address(&self, address: &str) -> Result<Option<(String, Vec<String>)>, String> {
        let address = parse_group_address(address).map_err(|error| error.to_string())?;
        Ok(self
            .store
            .ets_group(address)
            .map_err(|error| error.to_string())?
            .map(|group| (group.name, group.dpts)))
    }
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
                        value: WireState::Idle
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
                        value: WireState::Connected { .. }
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
