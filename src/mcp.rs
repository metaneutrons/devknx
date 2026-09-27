// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Local MCP stdio adapter. All bus actions remain with the capture owner.

use std::net::Ipv4Addr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

use crate::ets::parse_group_address;
use crate::ipc::{IpcClient, IpcMessage};
use crate::operations::{OperationOrigin, OperationRequest, prepare};
use crate::service::LiveCapture;
use crate::storage::CaptureStore;

const SEARCH_SCAN_LIMIT: u32 = 10_000;

/// Serve MCP tools until the stdio client disconnects.
///
/// # Errors
///
/// Returns a missing database or protocol transport failure.
pub async fn run(database: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    CaptureStore::open_existing(&database)?;
    let service = McpServer::new(database).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// MCP tools over one existing capture database and current-user IPC owner.
#[derive(Clone)]
pub struct McpServer {
    tool_router: ToolRouter<Self>,
    database: PathBuf,
}

impl McpServer {
    /// Construct a local stdio adapter; no listener or bus connection is made.
    #[must_use]
    pub fn new(database: PathBuf) -> Self {
        Self {
            tool_router: Self::tools(),
            database,
        }
    }

    async fn operation(&self, request: OperationRequest) -> CallToolResult {
        match IpcClient::operate_as(&self.database, &request, OperationOrigin::McpStdio).await {
            Ok(result @ IpcMessage::OperationResult { .. }) => {
                structured(json!({ "receipt": result }))
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
        description = "Discover KNXnet/IP gateways and return structured addresses and names; multicast may be unavailable on some networks."
    )]
    async fn knx_discover(&self) -> CallToolResult {
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

    /// Read the capture-owner state and active ETS revision.
    #[tool(
        description = "Read capture-owner connection state and active ETS revision without changing the bus."
    )]
    async fn knx_status(&self) -> CallToolResult {
        let database = self.database.clone();
        let revision = tokio::task::spawn_blocking(move || {
            CaptureStore::open_existing(&database)?.ets_revision()
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

    /// Search durable capture history using a bounded scan and cursor.
    #[tool(
        description = "Search retained captures by text. Returns up to 100 matches and a continuation cursor; each call scans at most 10000 rows."
    )]
    async fn knx_search_captures(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> CallToolResult {
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
        description = "Look up an imported ETS group-address name, hierarchy and declared DPTs."
    )]
    async fn knx_ets_lookup(
        &self,
        Parameters(params): Parameters<AddressParams>,
    ) -> CallToolResult {
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
        description = "Send a KNX group read through the capture owner; reports a matching response or no response after a bounded timeout."
    )]
    async fn knx_read(&self, Parameters(params): Parameters<ReadParams>) -> CallToolResult {
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
        description = "Preview a DPT-validated typed group write without transmission; this is not a bus action."
    )]
    async fn knx_write_preview(
        &self,
        Parameters(params): Parameters<WriteParams>,
    ) -> CallToolResult {
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
        description = "Transmit a DPT-validated typed KNX group write. The receipt confirms a transport send, not actuator state; raw writes are unavailable."
    )]
    async fn knx_typed_write(&self, Parameters(params): Parameters<WriteParams>) -> CallToolResult {
        match params.into_request() {
            Ok(request) => self.operation(request).await,
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
            if capture_matches(&message, query) {
                let value = serde_json::to_value(message).map_err(|error| error.to_string())?;
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
    use crate::capture::{CaptureEndpoint, CaptureEvent};
    use knx_rs_core::address::{DestinationAddress, IndividualAddress};
    use knx_rs_core::cemi::CemiFrame;
    use knx_rs_core::message::MessageCode;
    use knx_rs_core::types::Priority;

    #[test]
    fn tool_vocabulary_is_structured_and_has_no_raw_write() {
        let router = McpServer::tools();
        let names: Vec<_> = router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(names.len(), 7);
        for required in [
            "knx_discover",
            "knx_status",
            "knx_search_captures",
            "knx_ets_lookup",
            "knx_read",
            "knx_write_preview",
            "knx_typed_write",
        ] {
            assert!(names.contains(&required.to_owned()), "missing {required}");
        }
        assert!(!names.iter().any(|name| name.contains("raw")));
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
        drop(store);
        let first = search_captures(&database, "1/2/", 0, 1).unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(), 1);
        assert_eq!(first["next_after"], 1);
        assert_eq!(first["complete"], false);
        let second = search_captures(&database, "1/2/", 1, 10).unwrap();
        assert_eq!(second["items"].as_array().unwrap().len(), 2);
        assert_eq!(second["next_after"], 3);
        assert_eq!(second["complete"], true);
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
