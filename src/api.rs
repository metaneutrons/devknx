// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Versioned HTTP adapter over durable history and the sole KNX connection owner.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_stream::stream;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header, uri::Authority};
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
use tokio::sync::Semaphore;

use crate::control::{ControlClient, ControlRequest, ControlResponse, SessionInfo};
use crate::enrichment::{enrich_response_frame, enriched_capture_json};
use crate::ets::parse_group_address;
use crate::ipc::{IpcClient, IpcMessage, ReadOutcome};
use crate::operations::{OperationOrigin, OperationRequest, prepare};
use crate::paths;
use crate::service::{LiveCapture, LiveRoutingLoss};
use crate::storage::CaptureStore;

const MAX_PAGE: u32 = 1_000;
const REQUESTS_PER_MINUTE: usize = 600;
const BUS_REQUESTS_PER_MINUTE: usize = 60;
const MAX_SSE_CLIENTS: usize = 8;

#[derive(Default)]
struct ApiRate {
    all: VecDeque<Instant>,
    bus: VecDeque<Instant>,
}

impl ApiRate {
    fn check(&mut self, bus: bool, now: Instant) -> Result<(), &'static str> {
        for queue in [&mut self.all, &mut self.bus] {
            while queue
                .front()
                .is_some_and(|time| now.duration_since(*time) >= Duration::from_secs(60))
            {
                queue.pop_front();
            }
        }
        if self.all.len() >= REQUESTS_PER_MINUTE {
            return Err("REST request rate limit exceeded");
        }
        if bus && self.bus.len() >= BUS_REQUESTS_PER_MINUTE {
            return Err("REST KNX operation rate limit exceeded");
        }
        self.all.push_back(now);
        if bus {
            self.bus.push_back(now);
        }
        Ok(())
    }
}

/// Explicit listener policy. Constructing this value does not start HTTP.
#[derive(Clone, Debug)]
pub struct ApiConfig {
    /// Existing capture database; the API does not become a second writer.
    pub database: Option<PathBuf>,
    /// Optional default target for daemon-managed REST.
    pub endpoint: Option<String>,
    /// False only for the legacy database-scoped foreground API.
    pub managed: bool,
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
    known_databases: Arc<Mutex<HashMap<String, PathBuf>>>,
    rate: Arc<Mutex<ApiRate>>,
    sse_slots: Arc<Semaphore>,
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
        let unauthorized = self.0 == StatusCode::UNAUTHORIZED;
        let mut response = (self.0, Json(json!({ "error": self.1 }))).into_response();
        if unauthorized {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"devknx\""),
            );
        }
        if self.0 == StatusCode::TOO_MANY_REQUESTS {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
        }
        response
    }
}

fn path_extractor_error(rejection: &PathRejection) -> ApiError {
    ApiError::static_message(rejection.status(), "invalid path parameter")
}

fn query_extractor_error(rejection: &QueryRejection) -> ApiError {
    ApiError::static_message(rejection.status(), "invalid query parameters")
}

fn json_extractor_error(rejection: &JsonRejection) -> ApiError {
    ApiError::static_message(rejection.status(), "invalid JSON request body")
}

type ApiResult<T> = Result<T, ApiError>;

/// Validated and bound REST listener. Binding precedes an enabled status reply.
pub struct ApiServer {
    listener: tokio::net::TcpListener,
    config: ApiConfig,
}

impl ApiServer {
    /// Validate policy and bind one database-scoped listener.
    ///
    /// # Errors
    ///
    /// Returns invalid configuration, missing legacy database or listener failures.
    pub async fn bind(
        mut config: ApiConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        config.validate()?;
        if let Some(database) = config.database.as_deref() {
            if database.exists() {
                let store = CaptureStore::open_existing(database)?;
                if let Some(endpoint) = config.endpoint.as_deref() {
                    store.validate_endpoint(endpoint)?;
                }
            } else if !config.managed {
                CaptureStore::open_existing(database)?;
            }
        } else if !config.managed || config.endpoint.is_some() {
            return Err("REST configuration requires a capture database".into());
        }
        let listener = tokio::net::TcpListener::bind(config.bind).await?;
        config.bind = listener.local_addr()?;
        Ok(Self { listener, config })
    }

    /// Actual bound address, including the assigned port when port zero was requested.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.config.bind
    }

    /// Serve until the owning daemon requests graceful shutdown.
    ///
    /// # Errors
    ///
    /// Returns a listener failure.
    pub async fn run(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        eprintln!(
            "REST listening on http://{}/v1 (typed writes allowed: {})",
            self.config.bind,
            self.config.writes_allowed()
        );
        axum::serve(self.listener, router(self.config))
            .with_graceful_shutdown(shutdown)
            .await?;
        Ok(())
    }
}

/// Run REST in the foreground until interrupted.
///
/// # Errors
///
/// Returns invalid configuration, missing database or listener failures.
pub async fn run(config: ApiConfig) -> Result<(), Box<dyn std::error::Error>> {
    ApiServer::bind(config)
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|error| std::io::Error::other(error.to_string()).into())
}

fn router(config: ApiConfig) -> Router {
    let mut known_databases = HashMap::new();
    if let (Some(endpoint), Some(database)) = (&config.endpoint, &config.database) {
        known_databases.insert(endpoint.clone(), database.clone());
    }
    let state = ApiState {
        config,
        known_databases: Arc::new(Mutex::new(known_databases)),
        rate: Arc::new(Mutex::new(ApiRate::default())),
        sse_slots: Arc::new(Semaphore::new(MAX_SSE_CLIENTS)),
    };
    Router::new()
        .route("/v1/health", get(health))
        .route(
            "/v1/connection",
            get(connection)
                .post(connect_connection)
                .delete(disconnect_connection),
        )
        .route(
            "/v1/sessions",
            get(sessions)
                .post(connect_session)
                .delete(disconnect_session),
        )
        .route("/v1/captures", get(captures))
        .route("/v1/events", get(events))
        .route("/v1/routing-losses", get(routing_losses))
        .route("/v1/routing-loss-events", get(routing_loss_events))
        .route("/v1/ets/{address}", get(ets_lookup))
        .route("/v1/operations/preview", post(write_preview))
        .route("/v1/operations/typed-write", post(typed_write))
        .route("/v1/operations/read", post(read))
        .route("/v1/openapi.json", get(openapi))
        .method_not_allowed_fallback(|| async {
            ApiError::static_message(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
        })
        .fallback(|| async { ApiError::static_message(StatusCode::NOT_FOUND, "route not found") })
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), rate_limit))
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn rate_limit(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> ApiResult<Response> {
    let bus = matches!(
        request.uri().path(),
        "/v1/operations/read" | "/v1/operations/typed-write"
    ) || (matches!(request.uri().path(), "/v1/connection" | "/v1/sessions")
        && request.method() != Method::GET);
    state
        .rate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .check(bus, Instant::now())
        .map_err(|message| ApiError::static_message(StatusCode::TOO_MANY_REQUESTS, message))?;
    Ok(next.run(request).await)
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
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
            .map(|(_, credentials)| credentials.trim_start_matches(' '))
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
    let revision = tokio::task::spawn_blocking(move || match database {
        Some(database) if database.exists() => {
            CaptureStore::open_existing(&database)?.ets_revision()
        }
        _ => Ok(None),
    })
    .await
    .map_err(|_| internal_error())?
    .map_err(|_| internal_error())?;
    let owner = tokio::time::timeout(Duration::from_secs(2), async {
        let database = state.config.database.as_deref()?;
        let mut client = IpcClient::connect(database, false).await.ok()?;
        client.next().await.ok().flatten()
    })
    .await
    .unwrap_or_default();
    Ok(Json(json!({
        "api": "ready",
        "capture_owner": owner,
        "target_endpoint": state.config.endpoint.as_deref(),
        "ets_revision": revision,
        "writes_allowed": state.config.writes_allowed(),
    })))
}

fn managed_endpoint(state: &ApiState, endpoint: Option<&str>) -> ApiResult<String> {
    if !state.config.managed {
        return Err(ApiError::static_message(
            StatusCode::SERVICE_UNAVAILABLE,
            "connection control requires the daemon-managed REST listener",
        ));
    }
    let endpoint =
        endpoint
            .or(state.config.endpoint.as_deref())
            .ok_or(ApiError::static_message(
                StatusCode::BAD_REQUEST,
                "explicit endpoint is required",
            ))?;
    paths::canonical_endpoint(endpoint)
        .map_err(|reason| ApiError::dynamic(StatusCode::BAD_REQUEST, reason))
}

async fn listed_sessions() -> ApiResult<Vec<SessionInfo>> {
    let response = ControlClient::request_existing(ControlRequest::List)
        .await
        .map_err(|_| {
            ApiError::static_message(
                StatusCode::SERVICE_UNAVAILABLE,
                "daemon control unavailable",
            )
        })?;
    let ControlResponse::Sessions { sessions } = response else {
        return Err(ApiError::static_message(
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon control unavailable",
        ));
    };
    Ok(sessions)
}

async fn database_for(state: &ApiState, endpoint: Option<&str>) -> ApiResult<PathBuf> {
    if !state.config.managed {
        if endpoint.is_some() {
            return Err(ApiError::static_message(
                StatusCode::BAD_REQUEST,
                "endpoint selection is unavailable for the foreground API",
            ));
        }
        return state.config.database.clone().ok_or_else(internal_error);
    }
    let endpoint = managed_endpoint(state, endpoint)?;
    if let Some(session) = listed_sessions()
        .await?
        .into_iter()
        .find(|session| session.endpoint == endpoint)
    {
        state
            .known_databases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(endpoint, session.database.clone());
        return Ok(session.database);
    }
    if let Some(database) = state
        .known_databases
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&endpoint)
    {
        return Ok(database.clone());
    }
    paths::database_for_endpoint(&endpoint)
        .map_err(|reason| ApiError::dynamic(StatusCode::BAD_REQUEST, reason))
}

#[derive(Deserialize)]
struct EndpointQuery {
    endpoint: Option<String>,
}

async fn connection(
    State(state): State<ApiState>,
    query: Result<Query<EndpointQuery>, QueryRejection>,
) -> ApiResult<Json<Value>> {
    let Query(query) = query.map_err(|error| query_extractor_error(&error))?;
    let endpoint = managed_endpoint(&state, query.endpoint.as_deref())?;
    let sessions = listed_sessions().await?;
    let session_active = sessions.iter().any(|session| session.endpoint == endpoint);
    let owner = if session_active {
        let database = database_for(&state, Some(&endpoint)).await?;
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut client = IpcClient::connect(&database, false).await.ok()?;
            client.next().await.ok().flatten()
        })
        .await
        .unwrap_or_default()
    } else {
        None
    };
    Ok(Json(json!({
        "endpoint": endpoint,
        "session_active": session_active,
        "capture_owner": owner,
    })))
}

async fn connect_connection(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    let endpoint = managed_endpoint(&state, None)?;
    connect_endpoint(&state, endpoint, 100_000).await
}

async fn connect_endpoint(
    state: &ApiState,
    endpoint: String,
    max_events: u32,
) -> ApiResult<Json<Value>> {
    let database = database_for(state, Some(&endpoint)).await?;
    let response = ControlClient::request_existing(ControlRequest::Connect {
        endpoint: endpoint.clone(),
        database: Some(database),
        max_events,
    })
    .await
    .map_err(|_| {
        ApiError::static_message(
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon control unavailable",
        )
    })?;
    match response {
        ControlResponse::Session { session } => {
            state
                .known_databases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(endpoint, session.database.clone());
            Ok(Json(json!({ "session": session })))
        }
        ControlResponse::Error { reason } => Err(ApiError::dynamic(StatusCode::CONFLICT, reason)),
        _ => Err(ApiError::static_message(
            StatusCode::SERVICE_UNAVAILABLE,
            "unexpected daemon response",
        )),
    }
}

async fn disconnect_connection(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    let endpoint = managed_endpoint(&state, None)?;
    disconnect_endpoint(endpoint).await
}

async fn disconnect_endpoint(endpoint: String) -> ApiResult<Json<Value>> {
    let response = ControlClient::request_existing(ControlRequest::Disconnect { endpoint })
        .await
        .map_err(|_| {
            ApiError::static_message(
                StatusCode::SERVICE_UNAVAILABLE,
                "daemon control unavailable",
            )
        })?;
    match response {
        ControlResponse::Disconnected => Ok(Json(json!({ "disconnected": true }))),
        ControlResponse::Error { reason } => Err(ApiError::dynamic(StatusCode::CONFLICT, reason)),
        _ => Err(ApiError::static_message(
            StatusCode::SERVICE_UNAVAILABLE,
            "unexpected daemon response",
        )),
    }
}

async fn sessions(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    if !state.config.managed {
        return Err(ApiError::static_message(
            StatusCode::SERVICE_UNAVAILABLE,
            "session control requires the daemon-managed REST listener",
        ));
    }
    Ok(Json(json!({ "sessions": listed_sessions().await? })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectInput {
    endpoint: String,
    #[serde(default = "default_max_events")]
    max_events: u32,
}

const fn default_max_events() -> u32 {
    100_000
}

async fn connect_session(
    State(state): State<ApiState>,
    input: Result<Json<ConnectInput>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    let Json(input) = input.map_err(|error| json_extractor_error(&error))?;
    let endpoint = managed_endpoint(&state, Some(&input.endpoint))?;
    connect_endpoint(&state, endpoint, input.max_events).await
}

async fn disconnect_session(
    State(state): State<ApiState>,
    query: Result<Query<EndpointQuery>, QueryRejection>,
) -> ApiResult<Json<Value>> {
    let Query(query) = query.map_err(|error| query_extractor_error(&error))?;
    let endpoint = query.endpoint.as_deref().ok_or(ApiError::static_message(
        StatusCode::BAD_REQUEST,
        "explicit endpoint is required",
    ))?;
    disconnect_endpoint(managed_endpoint(&state, Some(endpoint))?).await
}

#[derive(Deserialize)]
struct PageQuery {
    endpoint: Option<String>,
    #[serde(default)]
    after: i64,
    limit: Option<u32>,
}

#[derive(Serialize)]
struct CapturePage {
    items: Vec<Value>,
    next_after: i64,
    limit: u32,
}

#[derive(Serialize)]
struct RoutingLossPage {
    items: Vec<IpcMessage>,
    next_after: i64,
    limit: u32,
}

fn page_bounds(query: &PageQuery) -> ApiResult<NonZeroU32> {
    if query.after < 0 {
        return Err(ApiError::static_message(
            StatusCode::BAD_REQUEST,
            "after must be nonnegative",
        ));
    }
    NonZeroU32::new(query.limit.unwrap_or(100))
        .filter(|value| value.get() <= MAX_PAGE)
        .ok_or(ApiError::static_message(
            StatusCode::BAD_REQUEST,
            "limit must be 1–1000",
        ))
}

async fn captures(
    State(state): State<ApiState>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<Json<CapturePage>> {
    let Query(query) = query.map_err(|error| query_extractor_error(&error))?;
    let limit_nonzero = page_bounds(&query)?;
    let limit = limit_nonzero.get();
    let database = database_for(&state, query.endpoint.as_deref()).await?;
    if !database.exists() {
        return Ok(Json(CapturePage {
            items: Vec::new(),
            next_after: query.after,
            limit,
        }));
    }
    let (items, next_after) = tokio::task::spawn_blocking(move || {
        let store = CaptureStore::open_existing(&database).map_err(|error| error.to_string())?;
        let rows = store
            .read_after(query.after, limit_nonzero)
            .map_err(|error| error.to_string())?;
        let next_after = rows.last().map_or(query.after, |row| row.id);
        let items = rows
            .into_iter()
            .map(|row| {
                enriched_capture_json(&capture_message(row), &store)
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok::<_, String>((items, next_after))
    })
    .await
    .map_err(|_| internal_error())?
    .map_err(|_| internal_error())?;
    Ok(Json(CapturePage {
        items,
        next_after,
        limit,
    }))
}

async fn routing_losses(
    State(state): State<ApiState>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<Json<RoutingLossPage>> {
    let Query(query) = query.map_err(|error| query_extractor_error(&error))?;
    let limit_nonzero = page_bounds(&query)?;
    let database = database_for(&state, query.endpoint.as_deref()).await?;
    if !database.exists() {
        return Ok(Json(RoutingLossPage {
            items: Vec::new(),
            next_after: query.after,
            limit: limit_nonzero.get(),
        }));
    }
    let rows = tokio::task::spawn_blocking(move || {
        CaptureStore::open_existing(&database)?
            .read_routing_losses_after(query.after, limit_nonzero)
    })
    .await
    .map_err(|_| internal_error())?
    .map_err(|_| internal_error())?;
    let next_after = rows.last().map_or(query.after, |row| row.id);
    Ok(Json(RoutingLossPage {
        items: rows.into_iter().map(routing_loss_message).collect(),
        next_after,
        limit: limit_nonzero.get(),
    }))
}

fn capture_message(row: crate::storage::StoredCapture) -> IpcMessage {
    IpcMessage::from(&LiveCapture {
        id: Some(row.id),
        event: row.event,
    })
}

fn routing_loss_message(row: crate::storage::StoredRoutingLoss) -> IpcMessage {
    IpcMessage::from(&LiveRoutingLoss {
        id: Some(row.id),
        event: row.event,
    })
}

async fn ets_lookup(
    State(state): State<ApiState>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<EndpointQuery>, QueryRejection>,
) -> ApiResult<Json<Value>> {
    let Path(address) = path.map_err(|error| path_extractor_error(&error))?;
    let Query(query) = query.map_err(|error| query_extractor_error(&error))?;
    let address = parse_group_address(&address)
        .map_err(|_| ApiError::static_message(StatusCode::BAD_REQUEST, "invalid group address"))?;
    let database = database_for(&state, query.endpoint.as_deref()).await?;
    if !database.exists() {
        return Ok(Json(json!({
            "revision": null,
            "address_raw": address.raw(),
            "group": null,
        })));
    }
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
    endpoint: Option<String>,
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
    input: Result<Json<TypedInput>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    let Json(input) = input.map_err(|error| json_extractor_error(&error))?;
    let database = database_for(&state, input.endpoint.as_deref()).await?;
    let request = input.request()?;
    let OperationRequest::TypedWrite { address_raw, .. } = request else {
        unreachable!("typed input only constructs typed writes")
    };
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
    input: Result<Json<TypedInput>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    if !state.config.writes_allowed() {
        return Err(ApiError::static_message(
            StatusCode::FORBIDDEN,
            "remote writes are disabled",
        ));
    }
    let Json(input) = input.map_err(|error| json_extractor_error(&error))?;
    let database = database_for(&state, input.endpoint.as_deref()).await?;
    let request = input.request()?;
    operate(&state, &database, request).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    endpoint: Option<String>,
    address: String,
    #[serde(default = "default_read_timeout")]
    timeout_ms: u32,
}

const fn default_read_timeout() -> u32 {
    2_000
}

async fn read(
    State(state): State<ApiState>,
    input: Result<Json<ReadInput>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    let Json(input) = input.map_err(|error| json_extractor_error(&error))?;
    let database = database_for(&state, input.endpoint.as_deref()).await?;
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
        &database,
        OperationRequest::Read {
            address_raw,
            timeout_ms: input.timeout_ms,
        },
    )
    .await
}

async fn operate(
    state: &ApiState,
    database: &std::path::Path,
    request: OperationRequest,
) -> ApiResult<Json<Value>> {
    let result = IpcClient::operate_as(database, &request, state.config.origin())
        .await
        .map_err(|_| {
            ApiError::static_message(StatusCode::SERVICE_UNAVAILABLE, "capture owner unavailable")
        })?;
    match result {
        IpcMessage::OperationResult { ref read, .. } => {
            let response_raw = match read {
                Some(ReadOutcome::Response { raw_cemi }) => Some(raw_cemi.clone()),
                _ => None,
            };
            let enrichment = if let Some(raw_cemi) = response_raw {
                let database = database.to_path_buf();
                tokio::task::spawn_blocking(move || {
                    let store = CaptureStore::open_existing(&database)
                        .map_err(|error| error.to_string())?;
                    enrich_response_frame(&raw_cemi, &store).map_err(|error| error.to_string())
                })
                .await
                .map_err(|_| internal_error())?
                .map_err(|_| internal_error())?
            } else {
                None
            };
            let mut value = serde_json::to_value(result).expect("operation result serializes");
            value
                .as_object_mut()
                .expect("operation result serializes as object")
                .insert(
                    "response_enrichment".into(),
                    serde_json::to_value(enrichment).expect("response enrichment serializes"),
                );
            Ok(Json(value))
        }
        IpcMessage::OperationError { reason } => {
            Err(ApiError::dynamic(StatusCode::UNPROCESSABLE_ENTITY, reason))
        }
        _ => Err(internal_error()),
    }
}

#[derive(Deserialize)]
struct EventQuery {
    endpoint: Option<String>,
    after: Option<i64>,
}

async fn events(
    State(state): State<ApiState>,
    query: Result<Query<EventQuery>, QueryRejection>,
    headers: HeaderMap,
) -> ApiResult<Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>>> {
    let Query(query) = query.map_err(|error| query_extractor_error(&error))?;
    let cursor = event_cursor(&query, &headers)?;
    let permit = state.sse_slots.clone().try_acquire_owned().map_err(|_| {
        ApiError::static_message(StatusCode::TOO_MANY_REQUESTS, "too many active SSE clients")
    })?;
    let database = database_for(&state, query.endpoint.as_deref()).await?;
    let event_stream = stream! {
        let _permit = permit;
        let mut cursor = cursor;
        let mut live = IpcClient::connect(&database, true).await.ok();
        loop {
            if !database.exists() {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            let path = database.clone();
            let result = tokio::task::spawn_blocking(move || {
                let store = CaptureStore::open_existing(&path).map_err(|error| error.to_string())?;
                let rows = store.read_after(cursor, NonZeroU32::new(100).expect("nonzero")).map_err(|error| error.to_string())?;
                rows.into_iter()
                    .map(|row| {
                        let id = row.id;
                        enriched_capture_json(&capture_message(row), &store)
                            .map(|value| (id, value))
                            .map_err(|error| error.to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()
            }).await;
            if let Ok(Ok(rows)) = result {
                if rows.is_empty() {
                    wait_for_live(&mut live, &database, cursor, false).await;
                }
                for (id, value) in rows {
                    if id > cursor.saturating_add(1) {
                        let data = json!({ "after": cursor, "next_available": id });
                        yield Ok(Event::default().event("retention_gap").data(data.to_string()));
                    }
                    cursor = id;
                    let data = value.to_string();
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

fn event_cursor(query: &EventQuery, headers: &HeaderMap) -> ApiResult<i64> {
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
    Ok(cursor)
}

async fn routing_loss_events(
    State(state): State<ApiState>,
    query: Result<Query<EventQuery>, QueryRejection>,
    headers: HeaderMap,
) -> ApiResult<Sse<impl futures_core::Stream<Item = Result<Event, Infallible>>>> {
    let Query(query) = query.map_err(|error| query_extractor_error(&error))?;
    let cursor = event_cursor(&query, &headers)?;
    let permit = state.sse_slots.clone().try_acquire_owned().map_err(|_| {
        ApiError::static_message(StatusCode::TOO_MANY_REQUESTS, "too many active SSE clients")
    })?;
    let database = database_for(&state, query.endpoint.as_deref()).await?;
    let event_stream = stream! {
        let _permit = permit;
        let mut cursor = cursor;
        let mut live = IpcClient::connect(&database, true).await.ok();
        loop {
            if !database.exists() {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            let path = database.clone();
            let result = tokio::task::spawn_blocking(move || {
                CaptureStore::open_existing(&path)?.read_routing_losses_after(cursor, NonZeroU32::new(100).expect("nonzero"))
            }).await;
            if let Ok(Ok(rows)) = result {
                if rows.is_empty() {
                    wait_for_live(&mut live, &database, cursor, true).await;
                }
                for row in rows {
                    if row.id > cursor.saturating_add(1) {
                        let data = json!({ "after": cursor, "next_available": row.id });
                        yield Ok(Event::default().event("retention_gap").data(data.to_string()));
                    }
                    cursor = row.id;
                    let data = serde_json::to_string(&routing_loss_message(row)).expect("router-loss message serializes");
                    yield Ok(Event::default().event("routing_loss").id(cursor.to_string()).data(data));
                }
            } else {
                yield Ok(Event::default().event("error").data("router-loss database unavailable"));
                break;
            }
        }
    };
    Ok(Sse::new(event_stream).keep_alive(KeepAlive::default()))
}

async fn wait_for_live(
    live: &mut Option<IpcClient>,
    database: &std::path::Path,
    cursor: i64,
    routing_losses: bool,
) {
    if let Some(client) = live {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            match tokio::time::timeout(remaining, client.next()).await {
                Ok(Ok(Some(IpcMessage::Capture { id: Some(id), .. })))
                    if !routing_losses && id > cursor =>
                {
                    return;
                }
                Ok(Ok(Some(IpcMessage::RoutingLostMessage { id: Some(id), .. })))
                    if routing_losses && id > cursor =>
                {
                    return;
                }
                Ok(Ok(Some(IpcMessage::Lagged { .. }))) | Err(_) => return,
                Ok(Ok(Some(_))) => {}
                Ok(Ok(None) | Err(_)) => {
                    *live = None;
                    break;
                }
            }
        }
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    *live = IpcClient::connect(database, true).await.ok();
}

#[expect(
    clippy::too_many_lines,
    reason = "the complete OpenAPI route and schema document is kept in one auditable value"
)]
async fn openapi(State(state): State<ApiState>) -> Json<Value> {
    let mut document = json!({
        "openapi": "3.1.0",
        "info": { "title": "devknx REST API", "version": env!("CARGO_PKG_VERSION"), "description": "The daemon-managed listener starts without a KNX session. GET/POST/DELETE /v1/sessions control explicit endpoints. Data requests require an endpoint unless a listener default was selected. Bearer authentication is required when a token is configured and always for non-loopback bindings. Remote typed writes additionally require explicit enablement. Raw writes are not exposed." },
        "paths": {
            "/v1/health": { "get": { "operationId": "health", "responses": { "200": { "description": "API and capture owner state", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Health" } } } } } } },
            "/v1/connection": {
                "get": { "operationId": "connectionStatus", "responses": { "200": { "description": "Selected KNX session and owner state", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Connection" } } } } } },
                "post": { "operationId": "connectKnx", "responses": { "200": { "description": "Selected KNX session started or already active", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ConnectionStart" } } } } } },
                "delete": { "operationId": "disconnectKnx", "responses": { "200": { "description": "Selected KNX session stopped; REST remains available", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ConnectionStop" } } } } } }
            },
            "/v1/sessions": {
                "get": { "operationId": "listSessions", "responses": { "200": { "description": "Active KNX sessions", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Sessions" } } } } } },
                "post": { "operationId": "startSession", "requestBody": { "$ref": "#/components/requestBodies/Connect" }, "responses": { "200": { "description": "Explicit KNX session started or already active", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ConnectionStart" } } } } } },
                "delete": { "operationId": "stopSession", "parameters": [{ "name": "endpoint", "in": "query", "required": true, "schema": { "type": "string" } }], "responses": { "200": { "description": "Explicit KNX session stopped; REST remains available", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ConnectionStop" } } } } } }
            },
            "/v1/captures": { "get": { "operationId": "listCaptures", "parameters": [
                { "name": "after", "in": "query", "schema": { "type": "integer", "minimum": 0 } },
                { "name": "limit", "in": "query", "schema": { "type": "integer", "minimum": 1, "maximum": 1000 } }
            ], "responses": { "200": { "description": "Cursor page", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/CapturePage" } } } } } } },
            "/v1/events": { "get": { "operationId": "streamCaptures", "parameters": [
                { "name": "after", "in": "query", "schema": { "type": "integer", "minimum": 0 } },
                { "name": "Last-Event-ID", "in": "header", "schema": { "type": "integer", "minimum": 0 } }
            ], "responses": { "200": { "description": "Resumable capture events", "content": { "text/event-stream": { "schema": { "type": "string" } } } } } } },
            "/v1/routing-losses": { "get": { "operationId": "listRoutingLosses", "parameters": [
                { "name": "after", "in": "query", "schema": { "type": "integer", "minimum": 0 } },
                { "name": "limit", "in": "query", "schema": { "type": "integer", "minimum": 1, "maximum": 1000 } }
            ], "responses": { "200": { "description": "Separate router-loss cursor page", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/RoutingLossPage" } } } } } } },
            "/v1/routing-loss-events": { "get": { "operationId": "streamRoutingLosses", "parameters": [
                { "name": "after", "in": "query", "schema": { "type": "integer", "minimum": 0 } },
                { "name": "Last-Event-ID", "in": "header", "schema": { "type": "integer", "minimum": 0 } }
            ], "responses": { "200": { "description": "Resumable router-loss events", "content": { "text/event-stream": { "schema": { "type": "string" } } } } } } },
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
                "Capture": { "type": "object", "required": ["type", "id", "observed_at_ms", "endpoint", "direction", "source", "destination", "service", "raw_cemi", "enrichment"], "properties": {
                    "type": { "const": "capture" }, "id": { "type": "integer", "minimum": 1 },
                    "observed_at_ms": { "type": "integer", "minimum": 0 }, "endpoint": { "type": "string" },
                    "direction": { "type": "string" }, "source": { "type": "string" },
                    "destination": { "type": "string" }, "service": { "type": "string" },
                    "raw_cemi": { "type": "string", "pattern": "^[0-9a-f]+$" },
                    "enrichment": { "$ref": "#/components/schemas/CaptureEnrichment" }
                } },
                "CaptureEnrichment": { "type": "object", "required": ["schema_version", "ets_revision", "group_name", "hierarchy", "dpts", "value"], "properties": {
                    "schema_version": { "const": 1 }, "ets_revision": { "type": ["integer", "null"] },
                    "group_name": { "type": ["string", "null"] }, "hierarchy": { "type": "array", "items": { "type": "string" } },
                    "dpts": { "type": "array", "items": { "type": "string" } }, "value": { "type": ["string", "null"] }
                } },
                "CapturePage": { "type": "object", "required": ["items", "next_after", "limit"], "properties": {
                    "items": { "type": "array", "items": { "$ref": "#/components/schemas/Capture" } },
                    "next_after": { "type": "integer", "minimum": 0 }, "limit": { "type": "integer", "minimum": 1, "maximum": 1000 }
                } },
                "RoutingLoss": { "type": "object", "required": ["type", "id", "observed_at_ms", "endpoint", "source", "device_state", "lost_messages"], "properties": {
                    "type": { "const": "routing_lost_message" }, "id": { "type": "integer", "minimum": 1 },
                    "observed_at_ms": { "type": "integer", "minimum": 0 }, "endpoint": { "type": "string" },
                    "source": { "type": "string" }, "device_state": { "type": "integer", "minimum": 0, "maximum": 255 },
                    "lost_messages": { "type": "integer", "minimum": 0, "maximum": 65535 }
                } },
                "RoutingLossPage": { "type": "object", "required": ["items", "next_after", "limit"], "properties": {
                    "items": { "type": "array", "items": { "$ref": "#/components/schemas/RoutingLoss" } },
                    "next_after": { "type": "integer", "minimum": 0 }, "limit": { "type": "integer", "minimum": 1, "maximum": 1000 }
                } },
                "Health": { "type": "object", "required": ["api", "capture_owner", "target_endpoint", "ets_revision", "writes_allowed"], "properties": {
                    "api": { "const": "ready" }, "capture_owner": { "type": ["object", "null"] },
                    "target_endpoint": { "type": ["string", "null"] },
                    "ets_revision": { "type": ["integer", "null"] }, "writes_allowed": { "type": "boolean" }
                } },
                "Connection": { "type": "object", "required": ["endpoint", "session_active", "capture_owner"], "properties": {
                    "endpoint": { "type": "string" }, "session_active": { "type": "boolean" },
                    "capture_owner": { "type": ["object", "null"] }
                } },
                "Sessions": { "type": "object", "required": ["sessions"], "properties": {
                    "sessions": { "type": "array", "items": { "type": "object", "required": ["endpoint", "database"], "properties": {
                        "endpoint": { "type": "string" }, "database": { "type": "string" }
                    } } }
                } },
                "ConnectionStart": { "type": "object", "required": ["session"], "properties": {
                    "session": { "type": "object", "required": ["endpoint", "database"], "properties": {
                        "endpoint": { "type": "string" }, "database": { "type": "string" }
                    } }
                } },
                "ConnectionStop": { "type": "object", "required": ["disconnected"], "properties": {
                    "disconnected": { "const": true }
                } },
                "EtsLookup": { "type": "object", "required": ["revision", "address_raw", "group"], "properties": {
                    "revision": { "type": ["integer", "null"] }, "address_raw": { "type": "integer", "minimum": 0, "maximum": 65535 },
                    "group": { "type": ["object", "null"] }
                } },
                "WritePreview": { "type": "object", "required": ["address_raw", "dpt", "raw_cemi", "transmitted"], "properties": {
                    "address_raw": { "type": "integer", "minimum": 0, "maximum": 65535 }, "dpt": { "type": "string" },
                    "raw_cemi": { "type": "string", "pattern": "^[0-9a-f]+$" }, "transmitted": { "const": false }
                } },
                "OperationResult": { "type": "object", "required": ["type", "audit_id", "capture_id", "raw_cemi", "read", "response_enrichment"], "properties": {
                    "type": { "const": "operation_result" }, "audit_id": { "type": "integer", "minimum": 1 },
                    "capture_id": { "type": "integer", "minimum": 1 }, "raw_cemi": { "type": "string", "pattern": "^[0-9a-f]+$" },
                    "read": { "type": ["object", "null"] },
                    "response_enrichment": { "anyOf": [{ "$ref": "#/components/schemas/CaptureEnrichment" }, { "type": "null" }] }
                } },
                "ApiError": { "type": "object", "required": ["error"], "properties": {
                    "error": { "type": "string" }
                } }
            },
            "requestBodies": {
                "TypedWrite": { "required": true, "content": { "application/json": { "schema": { "type": "object", "required": ["address", "value"], "properties": {
                    "endpoint": { "type": "string", "description": "Required unless a listener default endpoint was selected" }, "address": { "type": "string" }, "dpt": { "type": "string" }, "value": { "type": "string" }
                } } } } },
                "Read": { "required": true, "content": { "application/json": { "schema": { "type": "object", "required": ["address"], "properties": {
                    "endpoint": { "type": "string", "description": "Required unless a listener default endpoint was selected" }, "address": { "type": "string" }, "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 30000 }
                } } } } },
                "Connect": { "required": true, "content": { "application/json": { "schema": { "type": "object", "required": ["endpoint"], "properties": {
                    "endpoint": { "type": "string" }, "max_events": { "type": "integer", "minimum": 1, "default": 100_000 }
                } } } } }
            }
        }
    });
    if let Some(paths) = document.get_mut("paths").and_then(Value::as_object_mut) {
        for path in [
            "/v1/captures",
            "/v1/events",
            "/v1/routing-losses",
            "/v1/routing-loss-events",
            "/v1/ets/{address}",
            "/v1/connection",
        ] {
            if let Some(parameters) = paths
                .get_mut(path)
                .and_then(|item| item.get_mut("get"))
                .and_then(|get| get.as_object_mut())
                .map(|get| get.entry("parameters").or_insert_with(|| json!([])))
                .and_then(Value::as_array_mut)
            {
                parameters.push(json!({ "name": "endpoint", "in": "query", "required": false, "description": "Required when the listener has no default endpoint", "schema": { "type": "string" } }));
            }
        }
    }
    if let Some(paths) = document.get_mut("paths").and_then(Value::as_object_mut) {
        for path_item in paths.values_mut() {
            let Some(operations) = path_item.as_object_mut() else {
                continue;
            };
            for operation in operations.values_mut() {
                let Some(operation) = operation.as_object_mut() else {
                    continue;
                };
                if state.config.token.is_some() {
                    operation.insert("security".into(), json!([{ "bearer": [] }]));
                }
                let responses = operation
                    .entry("responses")
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .expect("OpenAPI responses are objects");
                responses.insert(
                    "default".into(),
                    json!({
                        "description": "API error",
                        "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ApiError" } } }
                    }),
                );
                responses.insert(
                    "429".into(),
                    json!({
                        "description": "Request or SSE concurrency limit exceeded",
                        "headers": { "Retry-After": { "schema": { "type": "integer", "minimum": 0 } } },
                        "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ApiError" } } }
                    }),
                );
                if state.config.token.is_some() {
                    responses.insert(
                        "401".into(),
                        json!({
                            "description": "Bearer authentication required",
                            "headers": { "WWW-Authenticate": { "schema": { "type": "string" } } },
                            "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ApiError" } } }
                        }),
                    );
                }
            }
        }
    }
    Json(document)
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
    use crate::capture::{CaptureEndpoint, CaptureEvent, RoutingLossEvent};
    use crate::ets::{CsvEncoding, EtsCatalog, EtsFormat};
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request};
    use futures_util::StreamExt as _;
    use knx_rs_core::address::{DestinationAddress, IndividualAddress};
    use knx_rs_core::cemi::CemiFrame;
    use knx_rs_core::message::MessageCode;
    use knx_rs_core::types::Priority;
    use knx_rs_ip::RoutingLostMessage;
    use tower::ServiceExt as _;

    fn config(database: PathBuf) -> ApiConfig {
        ApiConfig {
            database: Some(database),
            endpoint: None,
            managed: false,
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

    async fn raw_request(
        app: Router,
        method: Method,
        path: &str,
        body: &str,
        content_type: Option<&str>,
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "127.0.0.1:8765");
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        app.oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap()
    }

    async fn assert_api_error(response: Response, status: StatusCode) -> Value {
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert!(error["error"].is_string(), "{error}");
        assert_eq!(error.as_object().unwrap().len(), 1, "{error}");
        error
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

    #[test]
    fn rest_rate_limits_bus_actions_separately_and_recovers_after_window() {
        let mut rate = ApiRate::default();
        let now = Instant::now();
        for _ in 0..BUS_REQUESTS_PER_MINUTE {
            assert!(rate.check(true, now).is_ok());
        }
        assert!(rate.check(true, now).is_err());
        assert!(rate.check(false, now).is_ok());
        assert!(rate.check(true, now + Duration::from_secs(60)).is_ok());
    }

    #[tokio::test]
    async fn sse_client_cap_rejects_excess_streams_with_retry_hint() {
        let app = router(config(PathBuf::from("unused")));
        let mut streams = Vec::new();
        for _ in 0..MAX_SSE_CLIENTS {
            let response = request(app.clone(), Method::GET, "/v1/events", None).await;
            assert_eq!(response.status(), StatusCode::OK);
            streams.push(response);
        }
        let rejected = request(app.clone(), Method::GET, "/v1/routing-loss-events", None).await;
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(rejected.headers()[header::RETRY_AFTER], "60");
        drop(streams.pop());
        let recovered = request(app, Method::GET, "/v1/routing-loss-events", None).await;
        assert_eq!(recovered.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn capture_enrichment_and_router_loss_history_have_distinct_cursors() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("api.sqlite");
        let mut store = CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap();
        let xml = br#"<GroupAddress-Export xmlns="http://knx.org/xml/ga-export/01"><GroupRange Name="Lighting"><GroupAddress Name="Desk" Address="1/2/3" DPTs="DPT-1-1" /></GroupRange></GroupAddress-Export>"#;
        let catalog = EtsCatalog::from_bytes(xml, EtsFormat::GaXml01, CsvEncoding::Utf8).unwrap();
        store.import_ets(&catalog).unwrap();
        let frame = CemiFrame::new_l_data(
            MessageCode::LDataInd,
            IndividualAddress::from_raw(0x1101),
            DestinationAddress::Group(GroupAddress::from_raw(0x0a03)),
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
        let app = router(config(database));
        let response = request(app.clone(), Method::GET, "/v1/captures?limit=1", None).await;
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let captures: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(captures["items"][0]["enrichment"]["group_name"], "Desk");
        assert_eq!(captures["items"][0]["enrichment"]["value"], "true");
        assert_eq!(captures["next_after"], 1);
        let response = request(app.clone(), Method::GET, "/v1/routing-losses?limit=1", None).await;
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let losses: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(losses["items"][0]["type"], "routing_lost_message");
        assert_eq!(losses["items"][0]["lost_messages"], 7);
        assert_eq!(losses["next_after"], 1);
        let response = request(app.clone(), Method::GET, "/v1/routing-losses?after=1", None).await;
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let empty: Value = serde_json::from_slice(&body).unwrap();
        assert!(empty["items"].as_array().unwrap().is_empty());

        let response = request(app, Method::GET, "/v1/routing-loss-events?after=0", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        let chunk = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = String::from_utf8(chunk.to_vec()).unwrap();
        assert!(text.contains("event: routing_loss"), "{text}");
        assert!(text.contains("id: 1"), "{text}");
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
        assert_eq!(
            schema["paths"]["/v1/sessions"]["post"]["operationId"],
            "startSession"
        );
        assert_eq!(
            schema["paths"]["/v1/sessions"]["delete"]["operationId"],
            "stopSession"
        );
        assert_eq!(
            schema["components"]["requestBodies"]["Read"]["content"]["application/json"]["schema"]
                ["properties"]["endpoint"]["type"],
            "string"
        );
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
    async fn openapi_declares_configured_bearer_security_and_api_errors() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("api.sqlite");
        CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap();

        let public_app = router(config(database.clone()));
        let response = request(public_app, Method::GET, "/v1/openapi.json", None).await;
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let public_schema: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            public_schema["components"]["schemas"]["ApiError"]["required"],
            json!(["error"])
        );
        for path in public_schema["paths"].as_object().unwrap().values() {
            for operation in path.as_object().unwrap().values() {
                assert!(operation.get("security").is_none());
                assert_eq!(
                    operation["responses"]["default"]["content"]["application/json"]["schema"]["$ref"],
                    "#/components/schemas/ApiError"
                );
            }
        }

        let mut secured_config = config(database);
        secured_config.token = Some("s".repeat(32));
        let secured_app = router(secured_config);
        let unauthorized =
            request(secured_app.clone(), Method::GET, "/v1/openapi.json", None).await;
        assert_eq!(
            unauthorized.headers()[header::WWW_AUTHENTICATE],
            "Bearer realm=\"devknx\""
        );
        assert_api_error(unauthorized, StatusCode::UNAUTHORIZED).await;

        let response = Request::builder()
            .method(Method::GET)
            .uri("/v1/openapi.json")
            .header(header::HOST, "127.0.0.1:8765")
            .header(header::AUTHORIZATION, format!("bEaReR  {}", "s".repeat(32)))
            .body(Body::empty())
            .unwrap();
        let response = secured_app.oneshot(response).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let secured_schema: Value = serde_json::from_slice(&body).unwrap();
        for path in secured_schema["paths"].as_object().unwrap().values() {
            for operation in path.as_object().unwrap().values() {
                assert_eq!(operation["security"], json!([{ "bearer": [] }]));
                assert_eq!(
                    operation["responses"]["401"]["headers"]["WWW-Authenticate"]["schema"]["type"],
                    "string"
                );
            }
        }
    }

    #[tokio::test]
    async fn path_query_and_json_extractor_rejections_use_api_error_shape() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("api.sqlite");
        CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap();
        let app = router(config(database));

        assert_api_error(
            request(app.clone(), Method::GET, "/v1/ets/%FF", None).await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_api_error(
            request(
                app.clone(),
                Method::GET,
                "/v1/captures?limit=not-a-number",
                None,
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_api_error(
            raw_request(
                app.clone(),
                Method::POST,
                "/v1/operations/preview",
                "{invalid",
                Some("application/json"),
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_api_error(
            raw_request(
                app.clone(),
                Method::POST,
                "/v1/operations/preview",
                "{}",
                Some("text/plain"),
            )
            .await,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        )
        .await;
        assert_api_error(
            raw_request(
                app,
                Method::POST,
                "/v1/operations/preview",
                &" ".repeat(16 * 1024 + 1),
                Some("application/json"),
            )
            .await,
            StatusCode::PAYLOAD_TOO_LARGE,
        )
        .await;
    }

    #[tokio::test]
    async fn unknown_routes_and_methods_use_api_error_shape() {
        let app = router(config(PathBuf::from("unused")));
        assert_api_error(
            request(app.clone(), Method::GET, "/v1/not-a-route", None).await,
            StatusCode::NOT_FOUND,
        )
        .await;
        assert_api_error(
            request(app, Method::POST, "/v1/health", Some(json!({}))).await,
            StatusCode::METHOD_NOT_ALLOWED,
        )
        .await;
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
        let unauthorized_session = request(app.clone(), Method::GET, "/v1/sessions", None).await;
        assert_api_error(unauthorized_session, StatusCode::UNAUTHORIZED).await;
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
        assert_eq!(
            response.headers()[header::WWW_AUTHENTICATE],
            "Bearer realm=\"devknx\""
        );
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
