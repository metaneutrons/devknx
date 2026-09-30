// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Local MCP stdio adapter. All bus actions remain with the capture owner.

use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use knx_rs_core::address::GroupAddress;
use knx_rs_ip::discovery;
use rmcp::{
    ServerHandler, ServiceExt as _,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::control::{ControlClient, ControlRequest, ControlResponse, SessionInfo};
use crate::enrichment::{enrich_response_frame, enriched_capture_json};
use crate::ets::parse_group_address;
use crate::ipc::{IpcClient, IpcMessage, ReadOutcome};
use crate::operations::{OperationOrigin, OperationRequest, prepare};
use crate::paths;
use crate::service::{LiveCapture, LiveRoutingLoss};
use crate::storage::CaptureStore;

const SEARCH_SCAN_LIMIT: u32 = 10_000;
const DEFAULT_SESSION_MAX_EVENTS: u32 = 100_000;
const TOOL_CALLS_PER_MINUTE: usize = 120;
const BUS_CALLS_PER_MINUTE: usize = 12;
const DISCOVERY_CALLS_PER_MINUTE: usize = 6;

#[derive(Default)]
struct RateState {
    all: VecDeque<Instant>,
    bus: VecDeque<Instant>,
    discovery: VecDeque<Instant>,
}

#[derive(Clone, Copy)]
enum ToolKind {
    Local,
    Bus,
    Discovery,
}

impl RateState {
    fn check(&mut self, kind: ToolKind, now: Instant) -> Result<(), &'static str> {
        let window = Duration::from_secs(60);
        for queue in [&mut self.all, &mut self.bus, &mut self.discovery] {
            while queue
                .front()
                .is_some_and(|time| now.duration_since(*time) >= window)
            {
                queue.pop_front();
            }
        }
        if self.all.len() >= TOOL_CALLS_PER_MINUTE {
            return Err("MCP tool rate limit exceeded (120 calls per minute)");
        }
        match kind {
            ToolKind::Bus if self.bus.len() >= BUS_CALLS_PER_MINUTE => {
                return Err("KNX bus-operation rate limit exceeded (12 calls per minute)");
            }
            ToolKind::Discovery if self.discovery.len() >= DISCOVERY_CALLS_PER_MINUTE => {
                return Err("KNX discovery rate limit exceeded (6 calls per minute)");
            }
            _ => {}
        }
        self.all.push_back(now);
        match kind {
            ToolKind::Local => {}
            ToolKind::Bus => self.bus.push_back(now),
            ToolKind::Discovery => self.discovery.push_back(now),
        }
        Ok(())
    }
}

/// Serve MCP tools until the stdio client disconnects.
///
/// # Errors
///
/// Returns a protocol transport failure.
pub async fn run(
    database: PathBuf,
    selected_endpoint: Option<String>,
    allowed_write_addresses: Vec<u16>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut adapter = McpServer::with_write_addresses(database, allowed_write_addresses);
    adapter.selected_endpoint = selected_endpoint;
    let service = adapter.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// MCP tools over one existing capture database and current-user IPC owner.
#[derive(Clone)]
pub struct McpServer {
    tool_router: ToolRouter<Self>,
    database: PathBuf,
    selected_endpoint: Option<String>,
    rate: Arc<Mutex<RateState>>,
    allowed_write_addresses: BTreeSet<u16>,
}

impl McpServer {
    /// Construct a local stdio adapter; no listener or bus connection is made.
    #[must_use]
    pub fn new(database: PathBuf) -> Self {
        Self::with_write_addresses(database, [])
    }

    /// Construct an adapter with an explicit exact-address typed-write allowlist.
    #[must_use]
    pub fn with_write_addresses(
        database: PathBuf,
        addresses: impl IntoIterator<Item = u16>,
    ) -> Self {
        let allowed_write_addresses: BTreeSet<_> = addresses.into_iter().collect();
        let mut tool_router = Self::tools();
        if allowed_write_addresses.is_empty() {
            tool_router.remove_route("knx_typed_write");
        }
        Self {
            tool_router,
            database,
            selected_endpoint: None,
            rate: Arc::new(Mutex::new(RateState::default())),
            allowed_write_addresses,
        }
    }

    fn check_rate(&self, kind: ToolKind) -> Result<(), CallToolResult> {
        self.rate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .check(kind, Instant::now())
            .map_err(tool_error)
    }

    async fn operation(&self, request: OperationRequest) -> CallToolResult {
        match IpcClient::operate_as(&self.database, &request, OperationOrigin::McpStdio).await {
            Ok(result @ IpcMessage::OperationResult { .. }) => {
                let raw = match &result {
                    IpcMessage::OperationResult {
                        read: Some(ReadOutcome::Response { raw_cemi }),
                        ..
                    } => Some(raw_cemi.clone()),
                    _ => None,
                };
                let response_enrichment = if let Some(raw_cemi) = raw {
                    let database = self.database.clone();
                    match tokio::task::spawn_blocking(move || {
                        let store = CaptureStore::open_existing(&database)
                            .map_err(|error| error.to_string())?;
                        enrich_response_frame(&raw_cemi, &store).map_err(|error| error.to_string())
                    })
                    .await
                    {
                        Ok(Ok(enrichment)) => enrichment,
                        Ok(Err(error)) => return tool_error(error),
                        Err(_) => return tool_error("response enrichment worker stopped"),
                    }
                } else {
                    None
                };
                structured(json!({ "receipt": result, "response_enrichment": response_enrichment }))
            }
            Ok(IpcMessage::OperationError { reason }) => tool_error(reason),
            Ok(_) => tool_error("unexpected capture owner response"),
            Err(error) => tool_error(format!("capture owner unavailable: {error}")),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddressParams {
    /// KNX group address in three-level, two-level, decimal or ETS hex notation.
    address: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ConnectParams {
    /// Explicit KNXnet/IP tunnel or router endpoint URL.
    endpoint: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchParams {
    /// Case-insensitive text within captured endpoint, address, service or raw cEMI.
    query: String,
    /// Exclusive capture cursor; defaults to zero.
    after: Option<i64>,
    /// Maximum matching results, 1–100; defaults to 50.
    limit: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PageParams {
    /// Exclusive stream cursor; defaults to zero. Capture and router-loss IDs are independent.
    after: Option<i64>,
    /// Maximum results, 1–100; defaults to 50.
    limit: Option<u32>,
}

impl PageParams {
    fn validated(self) -> Result<(i64, NonZeroU32), String> {
        let after = self.after.unwrap_or(0);
        if after < 0 {
            return Err("after must be nonnegative".into());
        }
        let limit = self.limit.unwrap_or(50);
        let limit = NonZeroU32::new(limit)
            .filter(|limit| limit.get() <= 100)
            .ok_or("limit must be 1–100")?;
        Ok((after, limit))
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadParams {
    /// KNX group address.
    address: String,
    /// Response deadline, 1–30000 milliseconds; defaults to 2000.
    timeout_ms: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteParams {
    /// KNX group address.
    address: String,
    /// Explicit supported DPT; required when ETS metadata is missing or ambiguous.
    dpt: Option<String>,
    /// Value interpreted and encoded according to the selected DPT.
    value: String,
}

impl WriteParams {
    fn into_request(self) -> Result<OperationRequest, String> {
        let address_raw = parse_group_address(&self.address)
            .map_err(|error| error.to_string())?
            .raw();
        Ok(OperationRequest::TypedWrite {
            address_raw,
            dpt: self.dpt,
            value: self.value,
        })
    }
}

#[tool_router(router = tools, vis = "pub")]
impl McpServer {
    /// Discover KNXnet/IP gateways on the local network.
    #[tool(
        description = "Discover KNXnet/IP gateways and return structured addresses and names; multicast may be unavailable on some networks.",
        annotations(
            title = "Discover KNX gateways",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn knx_discover(&self) -> CallToolResult {
        if let Err(error) = self.check_rate(ToolKind::Discovery) {
            return error;
        }
        match discovery::discover(Ipv4Addr::UNSPECIFIED).await {
            Ok(gateways) => structured(json!({
                "gateways": gateways.into_iter().map(|gateway| json!({
                    "address": gateway.address.to_string(),
                    "name": gateway.name,
                    "individual_address_raw": gateway.individual_address,
                })).collect::<Vec<_>>()
            })),
            Err(error) => tool_error(error.to_string()),
        }
    }

    /// List every session configured in the current-user capture daemon.
    #[tool(
        description = "List all capture-daemon sessions, including an empty list when no daemon is running.",
        annotations(
            title = "List KNX sessions",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn knx_sessions(&self) -> CallToolResult {
        if let Err(error) = self.check_rate(ToolKind::Local) {
            return error;
        }
        match ControlClient::request_existing(ControlRequest::List).await {
            Ok(ControlResponse::Sessions { sessions }) => {
                structured(json!({ "sessions": sessions }))
            }
            Ok(ControlResponse::Error { reason }) => tool_error(reason),
            Ok(_) => tool_error("unexpected capture daemon response"),
            Err(error) if daemon_unavailable(&error) => structured(json!({ "sessions": [] })),
            Err(_) => tool_error("cannot list capture daemon sessions"),
        }
    }

    /// Connect an explicit KNXnet/IP endpoint using this server's selected database.
    #[tool(
        description = "Connect the explicit KNXnet/IP endpoint using this MCP server's selected capture database and a requested retention limit of 100000 events.",
        annotations(
            title = "Connect KNX session",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn knx_connect(&self, Parameters(params): Parameters<ConnectParams>) -> CallToolResult {
        if let Err(error) = self.check_rate(ToolKind::Bus) {
            return error;
        }
        let endpoint = match paths::canonical_endpoint(&params.endpoint) {
            Ok(endpoint) => endpoint,
            Err(error) => return tool_error(format!("invalid endpoint: {error}")),
        };
        if self
            .selected_endpoint
            .as_ref()
            .is_some_and(|selected| selected != &endpoint)
        {
            return tool_error("endpoint differs from this MCP server's --endpoint selector");
        }
        let database = match normalize_database_path(&self.database) {
            Ok(database) => database,
            Err(error) => return tool_error(error),
        };
        match ControlClient::request(ControlRequest::Connect {
            endpoint,
            database: Some(database),
            max_events: DEFAULT_SESSION_MAX_EVENTS,
        })
        .await
        {
            Ok(ControlResponse::Session { session }) => structured(json!({
                "session": session,
                "requested_max_events": DEFAULT_SESSION_MAX_EVENTS,
            })),
            Ok(ControlResponse::Error { reason }) => tool_error(reason),
            Ok(_) => tool_error("unexpected capture daemon response"),
            Err(_) => tool_error("cannot connect capture daemon session"),
        }
    }

    /// Disconnect only the daemon session using this server's selected database.
    #[tool(
        description = "Disconnect only the daemon session currently associated with this MCP server's selected capture database; returns disconnected=false when none is associated.",
        annotations(
            title = "Disconnect KNX session",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn knx_disconnect(&self) -> CallToolResult {
        if let Err(error) = self.check_rate(ToolKind::Bus) {
            return error;
        }
        let database = match normalize_database_path(&self.database) {
            Ok(database) => database,
            Err(error) => return tool_error(error),
        };
        if !database.exists() {
            return structured(json!({ "disconnected": false, "session": null }));
        }
        let sessions = match ControlClient::request_existing(ControlRequest::List).await {
            Ok(ControlResponse::Sessions { sessions }) => sessions,
            Ok(ControlResponse::Error { reason }) => return tool_error(reason),
            Ok(_) => return tool_error("unexpected capture daemon response"),
            Err(error) if daemon_unavailable(&error) => {
                return structured(json!({ "disconnected": false, "session": null }));
            }
            Err(_) => return tool_error("cannot list capture daemon sessions"),
        };
        let session = match session_for_database(&sessions, &database) {
            Ok(Some(session)) => session,
            Ok(None) => return structured(json!({ "disconnected": false, "session": null })),
            Err(error) => return tool_error(error),
        };
        if self
            .selected_endpoint
            .as_ref()
            .is_some_and(|selected| selected != &session.endpoint)
        {
            return tool_error("selected capture is bound to a different KNX endpoint");
        }
        match ControlClient::request_existing(ControlRequest::DisconnectScoped { database }).await {
            Ok(ControlResponse::Disconnected) => structured(json!({
                "disconnected": true,
                "session": session,
            })),
            Ok(ControlResponse::Error { reason }) => tool_error(reason),
            Ok(_) => tool_error("unexpected capture daemon response"),
            Err(_) => tool_error("cannot disconnect capture daemon session"),
        }
    }

    /// Read the capture-owner state and active ETS revision.
    #[tool(
        description = "Read capture-owner connection state and active ETS revision without changing the bus.",
        annotations(
            title = "KNX capture status",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn knx_status(&self) -> CallToolResult {
        if let Err(error) = self.check_rate(ToolKind::Local) {
            return error;
        }
        let database = self.database.clone();
        let revision = tokio::task::spawn_blocking(move || {
            if database.exists() {
                CaptureStore::open_existing(&database)?.ets_revision()
            } else {
                Ok(None)
            }
        })
        .await;
        let Ok(Ok(revision)) = revision else {
            return tool_error("capture database unavailable");
        };
        let owner = tokio::time::timeout(Duration::from_secs(2), async {
            let mut client = IpcClient::connect(&self.database, false).await.ok()?;
            client.next().await.ok().flatten()
        })
        .await
        .unwrap_or_default();
        structured(json!({ "capture_owner": owner, "ets_revision": revision }))
    }

    /// Page through retained captures without requiring a text query.
    #[tool(
        description = "List retained captures in ID order. Use next_after for the next page; capture and router-loss IDs are independent.",
        annotations(
            title = "List KNX captures",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn knx_list_captures(
        &self,
        Parameters(params): Parameters<PageParams>,
    ) -> CallToolResult {
        if let Err(error) = self.check_rate(ToolKind::Local) {
            return error;
        }
        let (after, limit) = match params.validated() {
            Ok(value) => value,
            Err(error) => return tool_error(error),
        };
        let database = self.database.clone();
        match tokio::task::spawn_blocking(move || {
            let store =
                CaptureStore::open_existing(&database).map_err(|error| error.to_string())?;
            let rows = store
                .read_after(after, limit)
                .map_err(|error| error.to_string())?;
            let next_after = rows.last().map_or(after, |row| row.id);
            let items = rows
                .into_iter()
                .map(|row| {
                    let message = IpcMessage::from(&LiveCapture {
                        id: Some(row.id),
                        event: row.event,
                    });
                    enriched_capture_json(&message, &store).map_err(|error| error.to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, String>(json!({
                "items": items,
                "next_after": next_after,
                "limit": limit.get(),
            }))
        })
        .await
        {
            Ok(Ok(value)) => structured(value),
            Ok(Err(error)) => tool_error(error),
            Err(_) => tool_error("capture list worker stopped"),
        }
    }

    /// Page through durable router-reported losses, separate from local subscriber lag.
    #[tool(
        description = "List router-reported RoutingLostMessage records in ID order. These are not local lag or a bus-wide loss total.",
        annotations(
            title = "List KNX router losses",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn knx_list_routing_losses(
        &self,
        Parameters(params): Parameters<PageParams>,
    ) -> CallToolResult {
        if let Err(error) = self.check_rate(ToolKind::Local) {
            return error;
        }
        let (after, limit) = match params.validated() {
            Ok(value) => value,
            Err(error) => return tool_error(error),
        };
        let database = self.database.clone();
        match tokio::task::spawn_blocking(move || {
            let store = CaptureStore::open_existing(&database)?;
            let rows = store.read_routing_losses_after(after, limit)?;
            let next_after = rows.last().map_or(after, |row| row.id);
            let items: Vec<_> = rows
                .into_iter()
                .map(|row| {
                    IpcMessage::from(&LiveRoutingLoss {
                        id: Some(row.id),
                        event: row.event,
                    })
                })
                .collect();
            Ok::<_, crate::storage::StorageError>(json!({
                "items": items,
                "next_after": next_after,
                "limit": limit.get(),
            }))
        })
        .await
        {
            Ok(Ok(value)) => structured(value),
            Ok(Err(error)) => tool_error(error.to_string()),
            Err(_) => tool_error("router-loss list worker stopped"),
        }
    }

    /// Search durable capture history using a bounded scan and cursor.
    #[tool(
        description = "Search retained captures by text. Returns up to 100 matches and a continuation cursor; each call scans at most 10000 rows.",
        annotations(
            title = "Search KNX captures",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn knx_search_captures(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> CallToolResult {
        if let Err(error) = self.check_rate(ToolKind::Local) {
            return error;
        }
        let query = params.query.trim().to_lowercase();
        if query.is_empty() || query.chars().count() > 256 {
            return tool_error("query must contain 1–256 characters");
        }
        let after = params.after.unwrap_or(0);
        if after < 0 {
            return tool_error("after must be nonnegative");
        }
        let limit = params.limit.unwrap_or(50);
        if !(1..=100).contains(&limit) {
            return tool_error("limit must be 1–100");
        }
        let database = self.database.clone();
        match tokio::task::spawn_blocking(move || search_captures(&database, &query, after, limit))
            .await
        {
            Ok(Ok(result)) => structured(result),
            Ok(Err(error)) => tool_error(error),
            Err(_) => tool_error("capture search worker stopped"),
        }
    }

    /// Look up active ETS metadata for one group address.
    #[tool(
        description = "Look up an imported ETS group-address name, hierarchy and declared DPTs.",
        annotations(
            title = "Look up KNX ETS group",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn knx_ets_lookup(
        &self,
        Parameters(params): Parameters<AddressParams>,
    ) -> CallToolResult {
        if let Err(error) = self.check_rate(ToolKind::Local) {
            return error;
        }
        let address = match parse_group_address(&params.address) {
            Ok(address) => address,
            Err(error) => return tool_error(error.to_string()),
        };
        let database = self.database.clone();
        match tokio::task::spawn_blocking(move || {
            let store = CaptureStore::open_existing(&database)?;
            Ok::<_, crate::storage::StorageError>(json!({
                "address_raw": address.raw(),
                "revision": store.ets_revision()?,
                "group": store.ets_group(address)?,
            }))
        })
        .await
        {
            Ok(Ok(value)) => structured(value),
            _ => tool_error("capture database unavailable"),
        }
    }

    /// Send a group read and distinguish its transport receipt from an observed response.
    #[tool(
        description = "Send a KNX group read through the capture owner; reports a matching response or no response after a bounded timeout.",
        annotations(
            title = "Read KNX group",
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn knx_read(&self, Parameters(params): Parameters<ReadParams>) -> CallToolResult {
        if let Err(error) = self.check_rate(ToolKind::Bus) {
            return error;
        }
        let address_raw = match parse_group_address(&params.address) {
            Ok(address) => address.raw(),
            Err(error) => return tool_error(error.to_string()),
        };
        let timeout_ms = params.timeout_ms.unwrap_or(2_000);
        if !(1..=30_000).contains(&timeout_ms) {
            return tool_error("read timeout must be 1–30000 ms");
        }
        self.operation(OperationRequest::Read {
            address_raw,
            timeout_ms,
        })
        .await
    }

    /// Prepare the exact DPT-encoded frame without transmitting it.
    #[tool(
        description = "Preview a DPT-validated typed group write without transmission; this is not a bus action.",
        annotations(
            title = "Preview KNX write",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn knx_write_preview(
        &self,
        Parameters(params): Parameters<WriteParams>,
    ) -> CallToolResult {
        if let Err(error) = self.check_rate(ToolKind::Local) {
            return error;
        }
        let request = match params.into_request() {
            Ok(request) => request,
            Err(error) => return tool_error(error),
        };
        let OperationRequest::TypedWrite { address_raw, .. } = request else {
            unreachable!("typed parameters only")
        };
        let database = self.database.clone();
        match tokio::task::spawn_blocking(move || {
            let store =
                CaptureStore::open_existing(&database).map_err(|error| error.to_string())?;
            let group = store
                .ets_group(GroupAddress::from_raw(address_raw))
                .map_err(|error| error.to_string())?;
            let prepared = prepare(request, group.as_ref()).map_err(|error| error.to_string())?;
            Ok::<_, String>(json!({
                "address_raw": address_raw,
                "dpt": prepared.dpt.map(|dpt| dpt.to_string()),
                "raw_cemi": hex(prepared.frame.as_bytes()),
                "transmitted": false,
            }))
        })
        .await
        {
            Ok(Ok(value)) => structured(value),
            Ok(Err(error)) => tool_error(error),
            Err(_) => tool_error("write preview worker stopped"),
        }
    }

    /// Transmit a typed, DPT-validated group write through the capture owner.
    #[tool(
        description = "Transmit a DPT-validated typed KNX group write. The receipt confirms a transport send, not actuator state; raw writes are unavailable.",
        annotations(
            title = "Write KNX group",
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    async fn knx_typed_write(&self, Parameters(params): Parameters<WriteParams>) -> CallToolResult {
        if let Err(error) = self.check_rate(ToolKind::Bus) {
            return error;
        }
        match params.into_request() {
            Ok(request @ OperationRequest::TypedWrite { address_raw, .. }) => {
                if !self.allowed_write_addresses.contains(&address_raw) {
                    return tool_error("MCP typed write is not enabled for this group address");
                }
                self.operation(request).await
            }
            Ok(_) => unreachable!("write parameters only construct typed writes"),
            Err(error) => tool_error(error),
        }
    }
}

#[tool_handler(router = self.tool_router)]
#[expect(
    clippy::unused_async_trait_impl,
    reason = "rmcp tool_handler generates an async trait method"
)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("devknx", env!("CARGO_PKG_VERSION")))
            .with_instructions("KNXnet/IP monitoring and typed group operations. Always inspect ETS metadata and preview before a write; transmission is not actuator-state confirmation.")
    }
}

fn search_captures(database: &Path, query: &str, after: i64, limit: u32) -> Result<Value, String> {
    let store = CaptureStore::open_existing(database).map_err(|error| error.to_string())?;
    let mut cursor = after;
    let mut scanned = 0;
    let mut matches = Vec::new();
    let mut exhausted = false;
    while scanned < SEARCH_SCAN_LIMIT && matches.len() < limit as usize {
        let page_size = (SEARCH_SCAN_LIMIT - scanned).min(1_000);
        let rows = store
            .read_after(cursor, NonZeroU32::new(page_size).expect("nonzero"))
            .map_err(|error| error.to_string())?;
        if rows.is_empty() {
            exhausted = true;
            break;
        }
        let page_len = rows.len();
        let mut fully_processed = true;
        for row in rows {
            cursor = row.id;
            scanned += 1;
            let message = IpcMessage::from(&LiveCapture {
                id: Some(row.id),
                event: row.event,
            });
            let value =
                enriched_capture_json(&message, &store).map_err(|error| error.to_string())?;
            if capture_matches(&message, query) || enrichment_matches(&value, query) {
                matches.push(value);
                if matches.len() >= limit as usize {
                    fully_processed = false;
                    break;
                }
            }
        }
        if fully_processed && page_len < page_size as usize {
            exhausted = true;
            break;
        }
    }
    Ok(json!({
        "items": matches,
        "next_after": cursor,
        "scanned": scanned,
        "complete": exhausted,
    }))
}

fn enrichment_matches(value: &Value, query: &str) -> bool {
    let enrichment = &value["enrichment"];
    enrichment["group_name"]
        .as_str()
        .is_some_and(|name| name.to_lowercase().contains(query))
        || enrichment["hierarchy"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item.as_str()
                    .is_some_and(|name| name.to_lowercase().contains(query))
            })
        })
        || enrichment["dpts"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item.as_str()
                    .is_some_and(|dpt| dpt.to_lowercase().contains(query))
            })
        })
        || enrichment["value"]
            .as_str()
            .is_some_and(|decoded| decoded.to_lowercase().contains(query))
}

fn capture_matches(message: &IpcMessage, query: &str) -> bool {
    let IpcMessage::Capture {
        endpoint,
        direction,
        source,
        destination,
        service,
        raw_cemi,
        ..
    } = message
    else {
        return false;
    };
    [endpoint, direction, source, destination, service, raw_cemi]
        .into_iter()
        .any(|field| field.to_lowercase().contains(query))
}

fn structured(value: Value) -> CallToolResult {
    CallToolResult::structured(value)
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::structured_error(json!({ "error": message.into() }))
}

fn daemon_unavailable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::AddrNotAvailable
    )
}

fn normalize_database_path(path: &Path) -> Result<PathBuf, &'static str> {
    if path == Path::new(":memory:") {
        return Err("selected capture database must be file-backed");
    }
    let name = path
        .file_name()
        .ok_or("selected capture database path is invalid")?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let absolute_parent = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| "selected capture database directory is unavailable")?
            .join(parent)
    };
    let normalized_parent = match std::fs::canonicalize(&absolute_parent) {
        Ok(parent) => parent,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => absolute_parent,
        Err(_) => return Err("selected capture database directory is unavailable"),
    };
    Ok(normalized_parent.join(name))
}

#[cfg(any(test, not(unix)))]
fn normalize_existing_database_path(path: &Path) -> Result<PathBuf, &'static str> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| "selected capture database is unavailable")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("selected capture database must be a regular file");
    }
    let name = path
        .file_name()
        .ok_or("selected capture database path is invalid")?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = std::fs::canonicalize(parent)
        .map_err(|_| "selected capture database directory is unavailable")?;
    Ok(parent.join(name))
}

#[derive(PartialEq, Eq)]
enum DatabaseIdentity {
    #[cfg(unix)]
    File { device: u64, inode: u64 },
    #[cfg(not(unix))]
    Path(PathBuf),
}

fn database_identity(path: &Path) -> Result<Option<DatabaseIdentity>, &'static str> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("cannot inspect capture database identity"),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(Some(DatabaseIdentity::File {
            device: metadata.dev(),
            inode: metadata.ino(),
        }))
    }
    #[cfg(not(unix))]
    {
        normalize_existing_database_path(path)
            .map(DatabaseIdentity::Path)
            .map(Some)
    }
}

fn session_for_database(
    sessions: &[SessionInfo],
    database: &Path,
) -> Result<Option<SessionInfo>, &'static str> {
    let Some(identity) = database_identity(database)? else {
        return Err("selected capture database is unavailable");
    };
    let mut matching = None;
    for session in sessions {
        if database_identity(&session.database)?.as_ref() == Some(&identity) {
            if matching.is_some() {
                return Err("multiple daemon sessions use the selected capture database");
            }
            matching = Some(session.clone());
        }
    }
    Ok(matching)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(result, "{byte:02x}").expect("String write cannot fail");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CaptureEndpoint, CaptureEvent, RoutingLossEvent};
    use crate::ets::{CsvEncoding, EtsCatalog, EtsFormat};
    use knx_rs_core::address::{DestinationAddress, IndividualAddress};
    use knx_rs_core::cemi::CemiFrame;
    use knx_rs_core::message::MessageCode;
    use knx_rs_core::types::Priority;
    use knx_rs_ip::RoutingLostMessage;

    #[test]
    fn tool_vocabulary_is_structured_and_has_no_raw_write() {
        let router = McpServer::tools();
        let tools = router.list_all();
        let names: Vec<_> = tools.iter().map(|tool| tool.name.to_string()).collect();
        assert_eq!(names.len(), 12);
        for required in [
            "knx_discover",
            "knx_sessions",
            "knx_connect",
            "knx_disconnect",
            "knx_status",
            "knx_list_captures",
            "knx_list_routing_losses",
            "knx_search_captures",
            "knx_ets_lookup",
            "knx_read",
            "knx_write_preview",
            "knx_typed_write",
        ] {
            assert!(names.contains(&required.to_owned()), "missing {required}");
        }
        assert!(!names.iter().any(|name| name.contains("raw")));
        let write = tools
            .iter()
            .find(|tool| tool.name == "knx_typed_write")
            .unwrap();
        let write_annotations = write.annotations.as_ref().unwrap();
        assert_eq!(write_annotations.read_only_hint, Some(false));
        assert_eq!(write_annotations.destructive_hint, Some(true));
        let preview = tools
            .iter()
            .find(|tool| tool.name == "knx_write_preview")
            .unwrap();
        assert_eq!(
            preview.annotations.as_ref().unwrap().read_only_hint,
            Some(true)
        );
        let sessions = tools
            .iter()
            .find(|tool| tool.name == "knx_sessions")
            .unwrap();
        assert_eq!(
            sessions.annotations.as_ref().unwrap().read_only_hint,
            Some(true)
        );
        let connect = tools
            .iter()
            .find(|tool| tool.name == "knx_connect")
            .unwrap();
        let connect_annotations = connect.annotations.as_ref().unwrap();
        assert_eq!(connect_annotations.read_only_hint, Some(false));
        assert_eq!(connect_annotations.destructive_hint, Some(false));
        let connect_schema = Value::Object(connect.input_schema.as_ref().clone());
        assert_eq!(connect_schema["required"][0], "endpoint");
        let disconnect = tools
            .iter()
            .find(|tool| tool.name == "knx_disconnect")
            .unwrap();
        assert_eq!(
            disconnect.annotations.as_ref().unwrap().read_only_hint,
            Some(false)
        );
        let default = McpServer::new(PathBuf::from("unused"));
        assert_eq!(default.tool_router.list_all().len(), 11);
        assert!(!default.tool_router.has_route("knx_typed_write"));
        let allowed = McpServer::with_write_addresses(PathBuf::from("unused"), [0x0a03]);
        assert!(allowed.tool_router.has_route("knx_typed_write"));
    }

    #[tokio::test]
    async fn connect_rejects_invalid_endpoint_before_daemon_access() {
        let server = McpServer::new(PathBuf::from("unused"));
        let result = server
            .knx_connect(Parameters(ConnectParams {
                endpoint: "http://example.invalid".into(),
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert!(
            result.structured_content.unwrap()["error"]
                .as_str()
                .unwrap()
                .starts_with("invalid endpoint:")
        );
    }

    #[tokio::test]
    async fn connect_rejects_an_endpoint_different_from_the_process_selector() {
        let mut server = McpServer::new(PathBuf::from("unused"));
        server.selected_endpoint = Some("tunnel://192.0.2.1:3671".into());
        let result = server
            .knx_connect(Parameters(ConnectParams {
                endpoint: "tunnel://192.0.2.2:3671".into(),
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert!(
            result.structured_content.unwrap()["error"]
                .as_str()
                .unwrap()
                .contains("--endpoint selector")
        );
    }

    #[test]
    fn disconnect_scoping_matches_normalized_database_identity_only() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.sqlite");
        let second = directory.path().join("second.sqlite");
        let third = directory.path().join("third.sqlite");
        std::fs::create_dir(directory.path().join("nested")).unwrap();
        drop(CaptureStore::open(&first, NonZeroU32::new(10).unwrap()).unwrap());
        drop(CaptureStore::open(&second, NonZeroU32::new(10).unwrap()).unwrap());
        drop(CaptureStore::open(&third, NonZeroU32::new(10).unwrap()).unwrap());
        let sessions = vec![
            SessionInfo {
                endpoint: "tunnel://192.0.2.1:3671".into(),
                database: first.clone(),
            },
            SessionInfo {
                endpoint: "tunnel://192.0.2.2:3671".into(),
                database: second,
            },
        ];
        let normalized_spelling = directory.path().join("nested/../first.sqlite");
        assert_eq!(
            session_for_database(&sessions, &normalized_spelling)
                .unwrap()
                .unwrap()
                .endpoint,
            "tunnel://192.0.2.1:3671"
        );
        #[cfg(unix)]
        {
            let alias = directory.path().join("first-alias.sqlite");
            std::fs::hard_link(&first, &alias).unwrap();
            assert_eq!(
                session_for_database(&sessions, &alias)
                    .unwrap()
                    .unwrap()
                    .endpoint,
                "tunnel://192.0.2.1:3671"
            );
        }
        assert_eq!(session_for_database(&sessions, &third).unwrap(), None);
        assert!(session_for_database(&sessions, &directory.path().join("missing.sqlite")).is_err());
        assert!(normalize_existing_database_path(&first).is_ok());
    }

    #[tokio::test]
    async fn write_policy_denies_unlisted_addresses_even_if_handler_is_called_directly() {
        let server = McpServer::with_write_addresses(PathBuf::from("unused"), [0x0a03]);
        let denied = server
            .knx_typed_write(Parameters(WriteParams {
                address: "1/2/4".into(),
                dpt: Some("1.001".into()),
                value: "true".into(),
            }))
            .await;
        assert_eq!(denied.is_error, Some(true));
        assert!(
            denied.structured_content.unwrap()["error"]
                .as_str()
                .unwrap()
                .contains("not enabled")
        );
        let default = McpServer::new(PathBuf::from("unused"));
        let denied = default
            .knx_typed_write(Parameters(WriteParams {
                address: "1/2/3".into(),
                dpt: Some("1.001".into()),
                value: "true".into(),
            }))
            .await;
        assert_eq!(denied.is_error, Some(true));
    }

    #[test]
    fn rate_limits_bound_bus_and_discovery_without_blocking_local_calls() {
        let mut state = RateState::default();
        let now = Instant::now();
        for _ in 0..BUS_CALLS_PER_MINUTE {
            assert!(state.check(ToolKind::Bus, now).is_ok());
        }
        assert!(state.check(ToolKind::Bus, now).is_err());
        assert!(state.check(ToolKind::Local, now).is_ok());
        for _ in 0..DISCOVERY_CALLS_PER_MINUTE {
            assert!(state.check(ToolKind::Discovery, now).is_ok());
        }
        assert!(state.check(ToolKind::Discovery, now).is_err());
        assert!(
            state
                .check(ToolKind::Bus, now + Duration::from_secs(60))
                .is_ok()
        );
        assert!(
            state
                .check(ToolKind::Discovery, now + Duration::from_secs(60))
                .is_ok()
        );
    }

    #[test]
    fn rate_limit_is_shared_across_server_clones() {
        let server = McpServer::new(PathBuf::from("unused"));
        let other = server.clone();
        for _ in 0..TOOL_CALLS_PER_MINUTE {
            assert!(server.check_rate(ToolKind::Local).is_ok());
        }
        assert!(other.check_rate(ToolKind::Local).is_err());
    }

    #[tokio::test]
    async fn independent_capture_and_router_loss_pages_reject_invalid_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("pages.sqlite");
        let mut store = CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap();
        let frame = CemiFrame::new_l_data(
            MessageCode::LDataInd,
            IndividualAddress::from_raw(0x1101),
            DestinationAddress::Group(GroupAddress::from_raw(0x0a01)),
            Priority::Low,
            &[0x00, 0x80, 1],
        );
        store
            .insert(&CaptureEvent::received(
                CaptureEndpoint::Tunnel("127.0.0.1:3671".parse().unwrap()),
                frame,
            ))
            .unwrap();
        store
            .insert_routing_loss(&RoutingLossEvent::received(
                CaptureEndpoint::Router("224.0.23.12:3671".parse().unwrap()),
                RoutingLostMessage {
                    source: "192.0.2.2:3671".parse().unwrap(),
                    device_state: 3,
                    lost_messages: 7,
                },
            ))
            .unwrap();
        drop(store);
        let server = McpServer::new(database);
        let captures = server
            .knx_list_captures(Parameters(PageParams {
                after: None,
                limit: Some(1),
            }))
            .await;
        assert_eq!(captures.is_error, Some(false));
        let captures = captures.structured_content.unwrap();
        assert_eq!(captures["items"][0]["type"], "capture");
        assert_eq!(captures["next_after"], 1);
        let losses = server
            .knx_list_routing_losses(Parameters(PageParams {
                after: None,
                limit: Some(1),
            }))
            .await;
        assert_eq!(losses.is_error, Some(false));
        let losses = losses.structured_content.unwrap();
        assert_eq!(losses["items"][0]["type"], "routing_lost_message");
        assert_eq!(losses["items"][0]["lost_messages"], 7);
        assert_eq!(losses["next_after"], 1);
        let invalid = server
            .knx_list_routing_losses(Parameters(PageParams {
                after: Some(-1),
                limit: Some(1),
            }))
            .await;
        assert_eq!(invalid.is_error, Some(true));
        let invalid = server
            .knx_list_captures(Parameters(PageParams {
                after: Some(0),
                limit: Some(101),
            }))
            .await;
        assert_eq!(invalid.is_error, Some(true));
    }

    #[test]
    fn search_scans_retained_history_with_a_continuation_cursor() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("search.sqlite");
        let mut store = CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap();
        let endpoint = CaptureEndpoint::Tunnel("127.0.0.1:3671".parse().unwrap());
        for (address, value) in [(0x0a01, 1), (0x0a02, 2), (0x0a03, 3)] {
            let frame = CemiFrame::new_l_data(
                MessageCode::LDataInd,
                IndividualAddress::from_raw(0x1101),
                DestinationAddress::Group(GroupAddress::from_raw(address)),
                Priority::Low,
                &[0x00, 0x80, value],
            );
            store
                .insert(&CaptureEvent::received(endpoint, frame))
                .unwrap();
        }
        let xml = br#"<GroupAddress-Export xmlns="http://knx.org/xml/ga-export/01"><GroupRange Name="Lighting"><GroupAddress Name="Desk lamp" Address="1/2/1" DPTs="DPT-1-1" /></GroupRange></GroupAddress-Export>"#;
        let catalog = EtsCatalog::from_bytes(xml, EtsFormat::GaXml01, CsvEncoding::Utf8).unwrap();
        store.import_ets(&catalog).unwrap();
        drop(store);
        let first = search_captures(&database, "1/2/", 0, 1).unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(), 1);
        assert_eq!(first["next_after"], 1);
        assert_eq!(first["complete"], false);
        let second = search_captures(&database, "1/2/", 1, 10).unwrap();
        assert_eq!(second["items"].as_array().unwrap().len(), 2);
        assert_eq!(second["next_after"], 3);
        assert_eq!(second["complete"], true);
        let named = search_captures(&database, "desk lamp", 0, 10).unwrap();
        assert_eq!(named["items"].as_array().unwrap().len(), 1);
        assert_eq!(named["items"][0]["enrichment"]["group_name"], "Desk lamp");
    }

    #[tokio::test]
    async fn preview_returns_structured_content_and_dpt_errors() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("preview.sqlite");
        CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap();
        let server = McpServer::new(database);
        let valid = server
            .knx_write_preview(Parameters(WriteParams {
                address: "1/2/3".into(),
                dpt: Some("1.001".into()),
                value: "true".into(),
            }))
            .await;
        assert_eq!(valid.is_error, Some(false));
        assert_eq!(valid.structured_content.unwrap()["transmitted"], false);
        let invalid = server
            .knx_write_preview(Parameters(WriteParams {
                address: "1/2/3".into(),
                dpt: None,
                value: "true".into(),
            }))
            .await;
        assert_eq!(invalid.is_error, Some(true));
        assert!(
            invalid.structured_content.unwrap()["error"]
                .as_str()
                .unwrap()
                .contains("no DPT")
        );
    }
}
