// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Versioned HTTP adapter over durable history and the sole KNX connection owner.

use std::borrow::Cow;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::Duration;

use async_stream::stream;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header, uri::Authority};
use axum::middleware::{self, Next};
use axum::response::{
    IntoResponse, Response,
    sse::{Event, KeepAlive, Sse},
};
use axum::routing::{get, post};
use axum::{Json, Router};
use knx_rs_core::address::GroupAddress;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use subtle::ConstantTimeEq as _;

use crate::ets::parse_group_address;
use crate::ipc::{IpcClient, IpcMessage};
use crate::operations::{OperationOrigin, OperationRequest, prepare};
use crate::service::LiveCapture;
use crate::storage::CaptureStore;

const MAX_PAGE: u32 = 1_000;

/// Explicit listener policy. Constructing this value does not start HTTP.
#[derive(Clone, Debug)]
pub struct ApiConfig {
    /// Existing capture database; the API does not become a second writer.
    pub database: PathBuf,
    /// Listener address. Loopback is the safe default at the CLI boundary.
    pub bind: SocketAddr,
    /// Secret loaded from an environment variable, never a CLI argument.
    pub token: Option<String>,
    /// Explicit permission for typed writes on a non-loopback listener.
    pub allow_remote_writes: bool,
}

impl ApiConfig {
    /// Reject an externally reachable listener without a usable token.
    ///
    /// # Errors
    ///
    /// Returns a configuration error before any socket is bound.
    pub fn validate(&self) -> Result<(), String> {
        if !self.bind.ip().is_loopback() && self.token.as_ref().is_none_or(|token| token.len() < 32)
        {
            return Err("non-loopback REST binding requires a token of at least 32 bytes".into());
        }
        if self.token.as_ref().is_some_and(|token| {
            token.len() < 32 || !token.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            return Err("REST token must contain at least 32 printable ASCII bytes".into());
        }
        if self.allow_remote_writes && self.bind.ip().is_loopback() {
            return Err("--allow-remote-writes only applies to non-loopback bindings".into());
        }
        Ok(())
    }

    const fn origin(&self) -> OperationOrigin {
        if self.bind.ip().is_loopback() {
            OperationOrigin::RestLoopback
        } else {
            OperationOrigin::RestRemote
        }
    }

    const fn writes_allowed(&self) -> bool {
        self.bind.ip().is_loopback() || self.allow_remote_writes
    }
}

#[derive(Clone)]
struct ApiState {
    config: ApiConfig,
}

#[derive(Debug)]
struct ApiError(StatusCode, Cow<'static, str>);

impl ApiError {
    const fn static_message(status: StatusCode, message: &'static str) -> Self {
        Self(status, Cow::Borrowed(message))
    }

    const fn dynamic(status: StatusCode, message: String) -> Self {
        Self(status, Cow::Owned(message))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// Run REST until interrupted. This is intentionally separate from `serve`.
///
/// # Errors
///
/// Returns invalid configuration, missing database or listener failures.
pub async fn run(mut config: ApiConfig) -> Result<(), Box<dyn std::error::Error>> {
    config.validate()?;
    CaptureStore::open_existing(&config.database)?;
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let address = listener.local_addr()?;
    config.bind = address;
    eprintln!(
        "REST listening on http://{address}/v1 (typed writes allowed: {})",
        config.writes_allowed()
    );
    axum::serve(listener, router(config))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn router(config: ApiConfig) -> Router {
    let state = ApiState { config };
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/captures", get(captures))
        .route("/v1/events", get(events))
        .route("/v1/ets/{address}", get(ets_lookup))
        .route("/v1/operations/preview", post(write_preview))
        .route("/v1/operations/typed-write", post(typed_write))
        .route("/v1/operations/read", post(read))
        .route("/v1/openapi.json", get(openapi))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> ApiResult<Response> {
    let headers = request.headers();
    // No CORS and no browser-initiated cross-site requests, including plain
    // navigation to a loopback listener from a DNS-rebinding page.
    if headers.contains_key(header::ORIGIN)
        || headers
            .get("sec-fetch-site")
            .is_some_and(|value| value == "cross-site")
    {
        return Err(ApiError::static_message(
            StatusCode::FORBIDDEN,
            "browser cross-site requests are forbidden",
        ));
    }
    if state.config.bind.ip().is_loopback() {
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<Authority>().ok());
        let valid = host.as_ref().is_some_and(|host| {
            matches!(
                host.host().trim_matches(['[', ']']),
                "localhost" | "127.0.0.1" | "::1"
            ) && host.port_u16() == Some(state.config.bind.port())
        });
        if !valid {
            return Err(ApiError::static_message(
                StatusCode::FORBIDDEN,
                "invalid loopback Host header",
            ));
        }
    }
    if let Some(token) = state.config.token.as_deref() {
        let authorized = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|provided| {
                provided.len() == token.len()
                    && bool::from(provided.as_bytes().ct_eq(token.as_bytes()))
            });
        if !authorized {
            return Err(ApiError::static_message(
                StatusCode::UNAUTHORIZED,
                "valid bearer token required",
            ));
        }
    }
    Ok(next.run(request).await)
}

async fn health(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    let database = state.config.database.clone();
    let revision =
        tokio::task::spawn_blocking(move || CaptureStore::open_existing(&database)?.ets_revision())
            .await
            .map_err(|_| internal_error())?
            .map_err(|_| internal_error())?;
    let owner = tokio::time::timeout(Duration::from_secs(2), async {
        let mut client = IpcClient::connect(&state.config.database, false)
            .await
            .ok()?;
        client.next().await.ok().flatten()
    })
    .await
    .unwrap_or_default();
    Ok(Json(json!({
        "api": "ready",
        "capture_owner": owner,
        "ets_revision": revision,
        "writes_allowed": state.config.writes_allowed(),
    })))
}

#[derive(Deserialize)]
struct PageQuery {
    #[serde(default)]
    after: i64,
    limit: Option<u32>,
}

#[derive(Serialize)]
struct CapturePage {
    items: Vec<IpcMessage>,
    next_after: i64,
    limit: u32,
}

async fn captures(
    State(state): State<ApiState>,
    Query(query): Query<PageQuery>,
) -> ApiResult<Json<CapturePage>> {
    if query.after < 0 {
        return Err(ApiError::static_message(
            StatusCode::BAD_REQUEST,
            "after must be nonnegative",
        ));
    }
    let limit = query.limit.unwrap_or(100);
    let limit_nonzero = NonZeroU32::new(limit)
        .filter(|value| value.get() <= MAX_PAGE)
        .ok_or(ApiError::static_message(
            StatusCode::BAD_REQUEST,
            "limit must be 1–1000",
        ))?;
    let database = state.config.database.clone();
    let rows = tokio::task::spawn_blocking(move || {
        CaptureStore::open_existing(&database)?.read_after(query.after, limit_nonzero)
    })
    .await
    .map_err(|_| internal_error())?
    .map_err(|_| internal_error())?;
    let next_after = rows.last().map_or(query.after, |row| row.id);
    Ok(Json(CapturePage {
        items: rows.into_iter().map(capture_message).collect(),
        next_after,
        limit,
    }))
}

fn capture_message(row: crate::storage::StoredCapture) -> IpcMessage {
    IpcMessage::from(&LiveCapture {
        id: Some(row.id),
        event: row.event,
    })
}

async fn ets_lookup(
    State(state): State<ApiState>,
    Path(address): Path<String>,
) -> ApiResult<Json<Value>> {
    let address = parse_group_address(&address)
        .map_err(|_| ApiError::static_message(StatusCode::BAD_REQUEST, "invalid group address"))?;
    let database = state.config.database.clone();
    let (revision, group) = tokio::task::spawn_blocking(move || {
        let store = CaptureStore::open_existing(&database)?;
        Ok::<_, crate::storage::StorageError>((store.ets_revision()?, store.ets_group(address)?))
    })
    .await
    .map_err(|_| internal_error())?
    .map_err(|_| internal_error())?;
    Ok(Json(
        json!({ "revision": revision, "address_raw": address.raw(), "group": group }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TypedInput {
    address: String,
    dpt: Option<String>,
    value: String,
}

impl TypedInput {
    fn request(self) -> ApiResult<OperationRequest> {
        let address_raw = parse_group_address(&self.address)
            .map_err(|_| {
                ApiError::static_message(StatusCode::BAD_REQUEST, "invalid group address")
            })?
            .raw();
        Ok(OperationRequest::TypedWrite {
            address_raw,
            dpt: self.dpt,
            value: self.value,
        })
    }
}

async fn write_preview(
    State(state): State<ApiState>,
    Json(input): Json<TypedInput>,
) -> ApiResult<Json<Value>> {
    let request = input.request()?;
    let OperationRequest::TypedWrite { address_raw, .. } = request else {
        unreachable!("typed input only constructs typed writes")
    };
    let database = state.config.database.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        let store = CaptureStore::open_existing(&database).map_err(|_| internal_error())?;
        let group = store
            .ets_group(GroupAddress::from_raw(address_raw))
            .map_err(|_| internal_error())?;
        prepare(request, group.as_ref())
            .map_err(|error| ApiError::dynamic(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()))
    })
    .await
    .map_err(|_| internal_error())??;
    Ok(Json(json!({
        "address_raw": address_raw,
        "dpt": prepared.dpt.map(|dpt| dpt.to_string()),
        "raw_cemi": hex(prepared.frame.as_bytes()),
        "transmitted": false,
    })))
}

async fn typed_write(
    State(state): State<ApiState>,
    Json(input): Json<TypedInput>,
) -> ApiResult<Json<IpcMessage>> {
    if !state.config.writes_allowed() {
        return Err(ApiError::static_message(
            StatusCode::FORBIDDEN,
            "remote writes are disabled",
        ));
    }
    let request = input.request()?;
    operate(&state, request).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    address: String,
    #[serde(default = "default_read_timeout")]
    timeout_ms: u32,
}

const fn default_read_timeout() -> u32 {
    2_000
}

async fn read(
    State(state): State<ApiState>,
    Json(input): Json<ReadInput>,
) -> ApiResult<Json<IpcMessage>> {
    let address_raw = parse_group_address(&input.address)
        .map_err(|_| ApiError::static_message(StatusCode::BAD_REQUEST, "invalid group address"))?
        .raw();
    if !(1..=30_000).contains(&input.timeout_ms) {
        return Err(ApiError::static_message(
            StatusCode::UNPROCESSABLE_ENTITY,
            "read timeout must be 1–30000 ms",
        ));
    }
    operate(
        &state,
        OperationRequest::Read {
            address_raw,
            timeout_ms: input.timeout_ms,
        },
    )
    .await
}

async fn operate(state: &ApiState, request: OperationRequest) -> ApiResult<Json<IpcMessage>> {
    let result = IpcClient::operate_as(&state.config.database, &request, state.config.origin())
        .await
        .map_err(|_| {
            ApiError::static_message(StatusCode::SERVICE_UNAVAILABLE, "capture owner unavailable")
        })?;
    match result {
        IpcMessage::OperationResult { .. } => Ok(Json(result)),
        IpcMessage::OperationError { reason } => {
            Err(ApiError::dynamic(StatusCode::UNPROCESSABLE_ENTITY, reason))
        }
        _ => Err(internal_error()),
    }
}

#[derive(Deserialize)]
struct EventQuery {
    after: Option<i64>,
}

async fn events(
    State(state): State<ApiState>,
    Query(query): Query<EventQuery>,
    headers: HeaderMap,
) -> ApiResult<Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>>> {
    let last_id = headers.get("last-event-id").map(|value| {
        value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
    });
    if query.after.is_some() && last_id.is_some() {
        return Err(ApiError::static_message(
            StatusCode::BAD_REQUEST,
            "choose after or Last-Event-ID",
        ));
    }
    let cursor = match last_id {
        Some(Some(id)) => id,
        Some(None) => {
            return Err(ApiError::static_message(
                StatusCode::BAD_REQUEST,
                "invalid Last-Event-ID",
            ));
        }
        None => query.after.unwrap_or(0),
    };
    if cursor < 0 {
        return Err(ApiError::static_message(
            StatusCode::BAD_REQUEST,
            "event cursor must be nonnegative",
        ));
    }
    let database = state.config.database;
    let event_stream = stream! {
        let mut cursor = cursor;
        loop {
            let path = database.clone();
            let result = tokio::task::spawn_blocking(move || {
                CaptureStore::open_existing(&path)?.read_after(cursor, NonZeroU32::new(100).expect("nonzero"))
            }).await;
            if let Ok(Ok(rows)) = result {
                if rows.is_empty() {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                for row in rows {
                    if row.id > cursor.saturating_add(1) {
                        let data = json!({ "after": cursor, "next_available": row.id });
                        yield Ok(Event::default().event("retention_gap").data(data.to_string()));
                    }
                    cursor = row.id;
                    let data = serde_json::to_string(&capture_message(row)).expect("capture message serializes");
                    yield Ok(Event::default().event("capture").id(cursor.to_string()).data(data));
                }
            } else {
                yield Ok(Event::default().event("error").data("capture database unavailable"));
                break;
            }
        }
    };
    Ok(Sse::new(event_stream).keep_alive(KeepAlive::default()))
}

async fn openapi() -> Json<Value> {
    Json(json!({
        "openapi": "3.1.0",
        "info": { "title": "devknx REST API", "version": "1.0.0", "description": "Bearer authentication is required when a token is configured and always for non-loopback bindings. Remote typed writes additionally require explicit enablement. Raw writes are not exposed." },
        "paths": {
            "/v1/health": { "get": { "operationId": "health", "responses": { "200": { "description": "API and capture owner state", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Health" } } } } } } },
            "/v1/captures": { "get": { "operationId": "listCaptures", "parameters": [
                { "name": "after", "in": "query", "schema": { "type": "integer", "minimum": 0 } },
                { "name": "limit", "in": "query", "schema": { "type": "integer", "minimum": 1, "maximum": 1000 } }
            ], "responses": { "200": { "description": "Cursor page", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/CapturePage" } } } } } } },
            "/v1/events": { "get": { "operationId": "streamCaptures", "parameters": [
                { "name": "after", "in": "query", "schema": { "type": "integer", "minimum": 0 } },
                { "name": "Last-Event-ID", "in": "header", "schema": { "type": "integer", "minimum": 0 } }
            ], "responses": { "200": { "description": "Resumable capture events", "content": { "text/event-stream": { "schema": { "type": "string" } } } } } } },
            "/v1/ets/{address}": { "get": { "operationId": "lookupEts", "parameters": [
                { "name": "address", "in": "path", "required": true, "schema": { "type": "string" } }
            ], "responses": { "200": { "description": "Active ETS group metadata", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/EtsLookup" } } } } } } },
            "/v1/operations/preview": { "post": { "operationId": "previewTypedWrite", "requestBody": { "$ref": "#/components/requestBodies/TypedWrite" }, "responses": { "200": { "description": "Exact unsent cEMI preview", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/WritePreview" } } } } } } },
            "/v1/operations/typed-write": { "post": { "operationId": "typedWrite", "requestBody": { "$ref": "#/components/requestBodies/TypedWrite" }, "responses": { "200": { "description": "Transport receipt, not device-state confirmation", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/OperationResult" } } } } } } },
            "/v1/operations/read": { "post": { "operationId": "readGroup", "requestBody": { "$ref": "#/components/requestBodies/Read" }, "responses": { "200": { "description": "Send receipt and observed response or timeout", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/OperationResult" } } } } } } },
            "/v1/openapi.json": { "get": { "operationId": "openApi", "responses": { "200": { "description": "OpenAPI 3.1 document" } } } }
        },
        "components": {
            "securitySchemes": { "bearer": { "type": "http", "scheme": "bearer" } },
            "schemas": {
                "Capture": { "type": "object", "required": ["type", "id", "observed_at_ms", "endpoint", "direction", "source", "destination", "service", "raw_cemi"], "properties": {
                    "type": { "const": "capture" }, "id": { "type": "integer", "minimum": 1 },
                    "observed_at_ms": { "type": "integer", "minimum": 0 }, "endpoint": { "type": "string" },
                    "direction": { "type": "string" }, "source": { "type": "string" },
                    "destination": { "type": "string" }, "service": { "type": "string" },
                    "raw_cemi": { "type": "string", "pattern": "^[0-9a-f]+$" }
                } },
                "CapturePage": { "type": "object", "required": ["items", "next_after", "limit"], "properties": {
                    "items": { "type": "array", "items": { "$ref": "#/components/schemas/Capture" } },
                    "next_after": { "type": "integer", "minimum": 0 }, "limit": { "type": "integer", "minimum": 1, "maximum": 1000 }
                } },
                "Health": { "type": "object", "required": ["api", "capture_owner", "ets_revision", "writes_allowed"], "properties": {
                    "api": { "const": "ready" }, "capture_owner": { "type": ["object", "null"] },
                    "ets_revision": { "type": ["integer", "null"] }, "writes_allowed": { "type": "boolean" }
                } },
                "EtsLookup": { "type": "object", "required": ["revision", "address_raw", "group"], "properties": {
                    "revision": { "type": ["integer", "null"] }, "address_raw": { "type": "integer", "minimum": 0, "maximum": 65535 },
                    "group": { "type": ["object", "null"] }
                } },
                "WritePreview": { "type": "object", "required": ["address_raw", "dpt", "raw_cemi", "transmitted"], "properties": {
                    "address_raw": { "type": "integer", "minimum": 0, "maximum": 65535 }, "dpt": { "type": "string" },
                    "raw_cemi": { "type": "string", "pattern": "^[0-9a-f]+$" }, "transmitted": { "const": false }
                } },
                "OperationResult": { "type": "object", "required": ["type", "audit_id", "capture_id", "raw_cemi", "read"], "properties": {
                    "type": { "const": "operation_result" }, "audit_id": { "type": "integer", "minimum": 1 },
                    "capture_id": { "type": "integer", "minimum": 1 }, "raw_cemi": { "type": "string", "pattern": "^[0-9a-f]+$" },
                    "read": { "type": ["object", "null"] }
                } }
            },
            "requestBodies": {
                "TypedWrite": { "required": true, "content": { "application/json": { "schema": { "type": "object", "required": ["address", "value"], "properties": {
                    "address": { "type": "string" }, "dpt": { "type": "string" }, "value": { "type": "string" }
                } } } } },
                "Read": { "required": true, "content": { "application/json": { "schema": { "type": "object", "required": ["address"], "properties": {
                    "address": { "type": "string" }, "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 30000 }
                } } } } }
            }
        }
    }))
}

const fn internal_error() -> ApiError {
    ApiError::static_message(
        StatusCode::INTERNAL_SERVER_ERROR,
        "capture database unavailable",
    )
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
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request};
    use futures_util::StreamExt as _;
    use knx_rs_core::address::{DestinationAddress, IndividualAddress};
    use knx_rs_core::cemi::CemiFrame;
    use knx_rs_core::message::MessageCode;
    use knx_rs_core::types::Priority;
    use tower::ServiceExt as _;

    fn config(database: PathBuf) -> ApiConfig {
        ApiConfig {
            database,
            bind: "127.0.0.1:8765".parse().unwrap(),
            token: None,
            allow_remote_writes: false,
        }
    }

    async fn request(app: Router, method: Method, path: &str, body: Option<Value>) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "127.0.0.1:8765");
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        app.oneshot(
            request
                .body(Body::from(
                    body.map_or_else(String::new, |value| value.to_string()),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[test]
    fn external_bind_fails_closed_without_secret_and_writes_remain_separate() {
        let mut config = config(PathBuf::from("unused"));
        config.bind = "0.0.0.0:8765".parse().unwrap();
        assert!(config.validate().is_err());
        config.token = Some("a".repeat(32));
        assert!(config.validate().is_ok());
        assert!(!config.writes_allowed());
        config.allow_remote_writes = true;
        assert!(config.writes_allowed());
    }

    #[tokio::test]
    async fn loopback_rejects_dns_rebinding_and_browser_origin() {
        let app = router(config(PathBuf::from("unused")));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/openapi.json")
                    .header(header::HOST, "evil.example:8765")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/openapi.json")
                    .header(header::HOST, "localhost:8765")
                    .header(header::ORIGIN, "https://evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn openapi_and_pages_are_versioned_and_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("api.sqlite");
        CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap();
        let app = router(config(database));
        let response = request(app.clone(), Method::GET, "/v1/openapi.json", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let schema: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(schema["openapi"], "3.1.0");
        assert!(schema["paths"]["/v1/events"].is_object());
        let response = request(
            app.clone(),
            Method::GET,
            "/v1/captures?after=0&limit=1",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let response = request(app, Method::GET, "/v1/captures?limit=1001", None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn typed_preview_uses_shared_dpt_validation() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("api.sqlite");
        CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap();
        let app = router(config(database));
        let input = json!({ "address": "1/2/3", "dpt": "1.001", "value": "true" });
        let response = request(
            app.clone(),
            Method::POST,
            "/v1/operations/preview",
            Some(input),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let input = json!({ "address": "1/2/3", "value": "true" });
        let response = request(app, Method::POST, "/v1/operations/preview", Some(input)).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn bearer_token_and_explicit_remote_write_policy_are_independent() {
        let mut config = config(PathBuf::from("unused"));
        config.bind = "0.0.0.0:8765".parse().unwrap();
        config.token = Some("s".repeat(32));
        config.validate().unwrap();
        let app = router(config);
        let input = json!({ "address": "1/2/3", "dpt": "1.001", "value": "true" });
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/operations/typed-write")
            .header(header::HOST, "192.0.2.4:8765")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(input.to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/operations/typed-write")
            .header(header::HOST, "192.0.2.4:8765")
            .header(header::AUTHORIZATION, format!("Bearer {}", "s".repeat(32)))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(input.to_string()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn sse_replays_only_captures_after_last_event_id() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("api.sqlite");
        let mut store = CaptureStore::open(&database, NonZeroU32::new(1).unwrap()).unwrap();
        let endpoint = CaptureEndpoint::Tunnel("127.0.0.1:3671".parse().unwrap());
        for value in [1, 2] {
            let frame = CemiFrame::new_l_data(
                MessageCode::LDataInd,
                IndividualAddress::from_raw(0x1101),
                DestinationAddress::Group(GroupAddress::from_raw(0x0a03)),
                Priority::Low,
                &[0x00, 0x80, value],
            );
            store
                .insert(&CaptureEvent::received(endpoint, frame))
                .unwrap();
        }
        drop(store);
        let app = router(config(database));
        let request = Request::builder()
            .uri("/v1/events")
            .header(header::HOST, "127.0.0.1:8765")
            .header("last-event-id", "1")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        let chunk = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = String::from_utf8(chunk.to_vec()).unwrap();
        assert!(text.contains("id: 2"), "{text}");
        assert!(!text.contains("id: 1"), "{text}");
        let request = Request::builder()
            .uri("/v1/events?after=0")
            .header(header::HOST, "127.0.0.1:8765")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let mut stream = response.into_body().into_data_stream();
        let chunk = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = String::from_utf8(chunk.to_vec()).unwrap();
        assert!(text.contains("event: retention_gap"), "{text}");
        assert!(text.contains("next_available"), "{text}");
        let request = Request::builder()
            .uri("/v1/events?after=1")
            .header(header::HOST, "127.0.0.1:8765")
            .header("last-event-id", "1")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
