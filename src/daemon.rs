// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Per-user daemon for explicitly connected KNX capture sessions.
//!
//! Starting the daemon only opens its control socket. A KNX connection, its
//! capture writer, and its database-scoped IPC listener are created only after
//! an explicit `Connect` request.

use std::collections::HashMap;
use std::error::Error;
use std::ffi::OsString;
use std::io;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use knx_rs_ip::parse_url;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::api::{ApiConfig, ApiServer};
use crate::control::{ControlRequest, ControlResponse, ControlServer, RestStatus, SessionInfo};
use crate::ipc::IpcServer;
use crate::lifecycle::{IdleSource, IdleTimeout, Lifecycle, SystemClock};
use crate::paths;
use crate::service::{
    CaptureService, MaintenanceEnvelope, MaintenanceReceipt, MaintenanceRequest, ReconnectPolicy,
};
use crate::storage::CaptureStore;

const LIVE_EVENT_CAPACITY: usize = 1_024;

/// The longest a stopping daemon answers requests that were already on their
/// way, each with [`ControlResponse::Stopping`].
/// How long a request to a session's store may take: a copy and a compaction of a
/// large capture are not instant.
const MAINTENANCE_TIMEOUT: Duration = Duration::from_secs(120);

const DRAIN: Duration = Duration::from_secs(5);

/// How long the stopping daemon waits for one more queued request. A request
/// already in the listener's queue arrives within this; after it, the
/// endpoint is gone and a late client's connect fails cleanly. Windows takes
/// queued pipe clients only while a listener is pending, so it does not wait.
#[cfg(unix)]
const QUEUE_QUIET: Duration = Duration::from_millis(50);
#[cfg(windows)]
const QUEUE_QUIET: Duration = Duration::ZERO;

/// How long a daemon started by a client waits for a stopping daemon to give
/// up the control endpoint: the drain plus stopping its sessions. At least as
/// long as a client waits for a daemon it started
/// ([`crate::control`] has that bound), or the client would give up while the
/// daemon it started is still waiting.
const OWNER_WAIT: Duration = Duration::from_secs(20);

/// How the daemon was started.
#[derive(Clone, Copy, Debug, Default)]
pub struct DaemonOptions {
    /// The idle period from `--idle-timeout`, over the settings file's.
    pub idle_timeout: Option<IdleTimeout>,
    /// Wait for a stopping daemon to give up the control endpoint, rather
    /// than fail at once. Set when a client starts the daemon.
    pub wait_for_endpoint: bool,
}

/// Run the per-user daemon until its control API requests shutdown, it has
/// been idle for its idle period, or Ctrl-C is pressed.
///
/// No KNX connection is started automatically.
///
/// # Errors
///
/// Returns an error if the control listener cannot start or stops unexpectedly.
pub async fn run(options: DaemonOptions) -> Result<(), Box<dyn Error + Send + Sync>> {
    let control = bind_control(options.wait_for_endpoint)
        .await
        .map_err(|error| other_error(&error))?;
    let lifecycle = start_lifecycle(options.idle_timeout);
    let (calls_tx, mut calls_rx) = mpsc::channel(32);
    let mut control_task = tokio::spawn(control.run(calls_tx));
    let mut sessions = DaemonSessions::default();
    let mut reap_timer = tokio::time::interval(Duration::from_millis(500));
    let signal = tokio::signal::ctrl_c();
    tokio::pin!(signal);

    let terminal_error = loop {
        tokio::select! {
            biased;
            signal_result = &mut signal => {
                break signal_result.err().map(|error| other_error(&error));
            }
            control_result = &mut control_task => {
                let error = match control_result {
                    Ok(Ok(())) => io::Error::other("control listener stopped unexpectedly"),
                    Ok(Err(error)) => io::Error::other(error.to_string()),
                    Err(error) => io::Error::other(format!("control listener task failed: {error}")),
                };
                break Some(Box::new(error) as Box<dyn Error + Send + Sync>);
            }
            _ = reap_timer.tick() => {
                sessions.reap_finished(&lifecycle).await;
                if lifecycle.idle_expired(sessions.by_endpoint.len()) {
                    let state = lifecycle.state(0);
                    eprintln!(
                        "idle for {} ({}), stopping",
                        IdleTimeout(Duration::from_secs(state.idle_seconds)),
                        state.source
                    );
                    break None;
                }
            }
            call = calls_rx.recv() => {
                let Some(call) = call else {
                    break Some(Box::new(io::Error::other("control channel closed")) as Box<dyn Error + Send + Sync>);
                };
                sessions.reap_finished(&lifecycle).await;
                let (request, reply) = call.into_request_and_reply();
                if is_activity(&request) {
                    lifecycle.touch();
                }
                let response = sessions.handle(request, &lifecycle).await;
                let stop = matches!(response, ControlResponse::Stopped);
                reply.respond(response).await;
                if stop {
                    break None;
                }
            }
        }
    };

    // Nobody new gets in, and every request already on its way is answered
    // with `Stopping`, which tells its client the daemon did not act on it.
    // Only then do the sessions stop and the endpoint lock go, so a daemon a
    // client starts meanwhile takes over a clean endpoint.
    crate::control::release_endpoint();
    let deadline = Instant::now() + DRAIN;
    while let Ok(Some(call)) = tokio::time::timeout(
        QUEUE_QUIET.min(deadline.saturating_duration_since(Instant::now())),
        calls_rx.recv(),
    )
    .await
    {
        let (_, reply) = call.into_request_and_reply();
        reply.respond(ControlResponse::Stopping).await;
    }
    sessions.stop_all().await;
    control_task.abort();
    let _ = control_task.await;

    if let Some(error) = terminal_error {
        return Err(error);
    }
    Ok(())
}

/// Bind the control endpoint, waiting for a stopping daemon to give it up
/// when asked to.
async fn bind_control(wait: bool) -> io::Result<ControlServer> {
    let deadline = Instant::now() + OWNER_WAIT;
    loop {
        match ControlServer::bind() {
            Ok(control) => return Ok(control),
            Err(error) if wait && error.kind() == io::ErrorKind::AddrInUse => {
                if Instant::now() >= deadline {
                    return Err(error);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// The idle timer, with its period from the start flag, the settings file,
/// or the default, in that order.
fn start_lifecycle(flag: Option<IdleTimeout>) -> Lifecycle {
    let (period, source) = flag.map_or_else(
        || match crate::settings::Settings::load() {
            Ok(crate::settings::Settings {
                idle_timeout: Some(timeout),
                ..
            }) => (timeout.0, IdleSource::Settings),
            Ok(_) => (crate::lifecycle::DEFAULT_IDLE, IdleSource::Default),
            Err(error) => {
                eprintln!("{error}; using the default idle period");
                (crate::lifecycle::DEFAULT_IDLE, IdleSource::Default)
            }
        },
        |timeout| (timeout.0, IdleSource::StartFlag),
    );
    Lifecycle::new(period, source, Arc::new(SystemClock))
}

/// Whether a request is something happening, which starts the idle period
/// again. Questions about the daemon itself are not: checking whether it
/// would stop must not keep it running. Leases hold the daemon on their own.
const fn is_activity(request: &ControlRequest) -> bool {
    !matches!(
        request,
        ControlRequest::Ping
            | ControlRequest::Identify
            | ControlRequest::GetLifecycle
            | ControlRequest::RenewLease { .. }
    )
}

#[derive(Default)]
struct DaemonSessions {
    by_endpoint: HashMap<String, ActiveSession>,
    by_database: HashMap<DatabaseIdentity, String>,
    rest: Option<ActiveRest>,
    /// Where a copy made before a clear goes; the data directory's `archive`
    /// when not set.
    archive_dir: Option<PathBuf>,
}

struct ActiveSession {
    info: SessionInfo,
    database_identity: DatabaseIdentity,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), String>>,
    /// The way to the service that owns this session's capture store.
    maintenance: mpsc::Sender<MaintenanceEnvelope>,
}

struct ActiveRest {
    status: RestStatus,
    database: Option<PathBuf>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), Box<dyn Error + Send + Sync>>>,
}

/// Identity of a database after resolving its parent and, on Unix, any existing
/// file's device/inode pair. The path variant also covers a not-yet-created DB.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum DatabaseIdentity {
    Path(PathBuf),
    #[cfg(unix)]
    File {
        device: u64,
        inode: u64,
    },
}

impl DaemonSessions {
    async fn handle(&mut self, request: ControlRequest, lifecycle: &Lifecycle) -> ControlResponse {
        self.reap_rest().await;
        match request {
            ControlRequest::RenewLease { client, kind } => {
                lifecycle.renew(&client, kind);
                self.lifecycle(lifecycle)
            }
            ControlRequest::ReleaseLease { client } => {
                lifecycle.release(&client);
                self.lifecycle(lifecycle)
            }
            ControlRequest::SetIdleTimeout { seconds } => {
                lifecycle.set_period(Duration::from_secs(seconds), IdleSource::Runtime);
                self.lifecycle(lifecycle)
            }
            ControlRequest::GetLifecycle => self.lifecycle(lifecycle),
            ControlRequest::Ping => ControlResponse::Pong,
            ControlRequest::Identify => ControlResponse::Identity {
                identity: crate::identity::BuildIdentity::current(),
            },
            ControlRequest::List => {
                let mut sessions = self
                    .by_endpoint
                    .values()
                    .map(|session| session.info.clone())
                    .collect::<Vec<_>>();
                sessions.sort_by(|left, right| left.endpoint.cmp(&right.endpoint));
                ControlResponse::Sessions { sessions }
            }
            ControlRequest::Connect {
                endpoint,
                database,
                max_events,
            } => self.connect(&endpoint, database, max_events),
            ControlRequest::Disconnect { endpoint } => {
                let endpoint = match paths::canonical_endpoint(&endpoint) {
                    Ok(endpoint) => endpoint,
                    Err(reason) => return ControlResponse::Error { reason },
                };
                self.disconnect(&endpoint).await;
                ControlResponse::Disconnected
            }
            ControlRequest::DisconnectScoped { database } => {
                let database = match resolve_database_path(&database) {
                    Ok(database) => database,
                    Err(error) => {
                        return ControlResponse::Error {
                            reason: format!("invalid capture database path: {error}"),
                        };
                    }
                };
                let identity = match database_identity(&database) {
                    Ok(identity) => identity,
                    Err(error) => {
                        return ControlResponse::Error {
                            reason: format!("invalid capture database path: {error}"),
                        };
                    }
                };
                let Some(endpoint) = self.by_database.get(&identity).cloned() else {
                    return ControlResponse::Error {
                        reason: "no active session for selected capture database".to_owned(),
                    };
                };
                self.disconnect(&endpoint).await;
                ControlResponse::Disconnected
            }
            request @ (ControlRequest::CaptureCounts { .. }
            | ControlRequest::ClearCapture { .. }) => self.capture_request(request).await,
            ControlRequest::Stop => {
                self.stop_all().await;
                ControlResponse::Stopped
            }
            ControlRequest::RestEnable {
                endpoint,
                database,
                bind,
                token,
                allow_remote_writes,
            } => {
                self.enable_rest(endpoint, database, bind, token, allow_remote_writes)
                    .await
            }
            ControlRequest::RestDisable => {
                self.disable_rest().await;
                ControlResponse::Rest {
                    status: self.rest_status(),
                }
            }
            ControlRequest::RestStatus => ControlResponse::Rest {
                status: self.rest_status(),
            },
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "session validation and atomic ownership setup"
    )]
    fn connect(
        &mut self,
        endpoint: &str,
        database: Option<PathBuf>,
        max_events: u32,
    ) -> ControlResponse {
        let endpoint = match paths::canonical_endpoint(endpoint) {
            Ok(endpoint) => endpoint,
            Err(reason) => return ControlResponse::Error { reason },
        };
        let spec = match parse_url(&endpoint) {
            Ok(spec) => spec,
            Err(error) => {
                return ControlResponse::Error {
                    reason: format!("invalid endpoint: {error}"),
                };
            }
        };
        let Some(retention) = NonZeroU32::new(max_events) else {
            return ControlResponse::Error {
                reason: "max_events must be greater than zero".to_owned(),
            };
        };

        let requested_database = match database {
            Some(database) => database,
            None => match self
                .rest
                .as_ref()
                .filter(|rest| rest.status.endpoint.as_deref() == Some(endpoint.as_str()))
            {
                Some(rest) => rest.database.clone().expect("selected REST has a database"),
                None => match paths::database_for_endpoint(&endpoint) {
                    Ok(database) => database,
                    Err(reason) => return ControlResponse::Error { reason },
                },
            },
        };
        let database = match resolve_database_path(&requested_database) {
            Ok(database) => database,
            Err(error) => {
                return ControlResponse::Error {
                    reason: format!("invalid capture database path: {error}"),
                };
            }
        };
        if self.rest.as_ref().is_some_and(|rest| {
            rest.status.endpoint.as_deref() == Some(endpoint.as_str())
                && rest.database.as_ref() != Some(&database)
        }) {
            return ControlResponse::Error {
                reason: "REST is configured for this endpoint with a different capture database"
                    .to_owned(),
            };
        }
        let before_open = match database_identity(&database) {
            Ok(identity) => identity,
            Err(error) => {
                return ControlResponse::Error {
                    reason: format!("invalid capture database path: {error}"),
                };
            }
        };

        if let Some(existing) = self.by_endpoint.get(&endpoint) {
            if existing.database_identity == before_open {
                return ControlResponse::Session {
                    session: existing.info.clone(),
                };
            }
            return ControlResponse::Error {
                reason: format!(
                    "endpoint {endpoint} is already connected with different settings; disconnect it first"
                ),
            };
        }
        if let Some(owner) = self.by_database.get(&before_open) {
            return ControlResponse::Error {
                reason: format!("capture database is already assigned to endpoint {owner}"),
            };
        }

        let store = match CaptureStore::open_bound(&database, retention, &endpoint) {
            Ok(store) => store,
            Err(error) => {
                return ControlResponse::Error {
                    reason: format!("cannot open capture database: {error}"),
                };
            }
        };
        let identity = match database_identity(&database) {
            Ok(identity) => identity,
            Err(error) => {
                return ControlResponse::Error {
                    reason: format!("cannot identify capture database: {error}"),
                };
            }
        };
        if let Some(owner) = self.by_database.get(&identity) {
            return ControlResponse::Error {
                reason: format!("capture database is already assigned to endpoint {owner}"),
            };
        }

        let ipc = match IpcServer::bind(&database) {
            Ok(ipc) => ipc.with_configured_endpoint(endpoint.clone()),
            Err(error) => {
                return ControlResponse::Error {
                    reason: format!("cannot bind capture IPC: {error}"),
                };
            }
        };
        let Some(event_capacity) = NonZeroUsize::new(LIVE_EVENT_CAPACITY) else {
            return ControlResponse::Error {
                reason: "invalid live event capacity".to_owned(),
            };
        };
        let service = CaptureService::new(
            spec,
            Some(store),
            ReconnectPolicy::default(),
            event_capacity,
        );
        let info = SessionInfo {
            endpoint: endpoint.clone(),
            database,
            max_events: Some(max_events),
        };
        let maintenance = service.maintenance_sender();
        let (shutdown, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(run_session(service, ipc, shutdown_rx));

        self.by_database.insert(identity.clone(), endpoint.clone());
        self.by_endpoint.insert(
            endpoint,
            ActiveSession {
                info: info.clone(),
                database_identity: identity,
                shutdown: Some(shutdown),
                task,
                maintenance,
            },
        );
        ControlResponse::Session { session: info }
    }

    /// Count or clear the capture a session records.
    async fn capture_request(&self, request: ControlRequest) -> ControlResponse {
        match request {
            ControlRequest::CaptureCounts { database } => {
                match self.maintain(&database, MaintenanceRequest::Counts).await {
                    Ok(receipt) => ControlResponse::CaptureCounts {
                        events: receipt.counts.events,
                        losses: receipt.counts.losses,
                    },
                    Err(reason) => ControlResponse::Error { reason },
                }
            }
            ControlRequest::ClearCapture { database, archive } => {
                let now = std::time::SystemTime::now();
                let archive = if archive {
                    let path = self.archive_dir.as_ref().map_or_else(
                        || paths::archive_path(&database, now),
                        |directory| Ok(paths::archive_path_in(directory, &database, now)),
                    );
                    match path {
                        Ok(path) => Some(path),
                        Err(reason) => return ControlResponse::Error { reason },
                    }
                } else {
                    None
                };
                match self
                    .maintain(&database, MaintenanceRequest::Clear { archive })
                    .await
                {
                    Ok(receipt) => ControlResponse::CaptureCleared {
                        events: receipt.counts.events,
                        losses: receipt.counts.losses,
                        archive: receipt.archive,
                    },
                    Err(reason) => ControlResponse::Error { reason },
                }
            }
            // Not formatted: the request type deliberately has no `Debug`, as
            // some of its variants carry a token.
            _ => ControlResponse::Error {
                reason: "not a capture request".to_owned(),
            },
        }
    }

    /// Ask the session that records `database` to do something to its capture.
    ///
    /// Only a capture a session of this daemon is recording is answered: its
    /// store has one writer, and the session is it.
    async fn maintain(
        &self,
        database: &Path,
        request: MaintenanceRequest,
    ) -> Result<MaintenanceReceipt, String> {
        let database = resolve_database_path(database)
            .map_err(|error| format!("invalid capture database path: {error}"))?;
        let identity = database_identity(&database)
            .map_err(|error| format!("invalid capture database path: {error}"))?;
        let session = self
            .by_database
            .get(&identity)
            .and_then(|endpoint| self.by_endpoint.get(endpoint))
            .ok_or_else(|| {
                "this capture is not being recorded by the daemon, so it cannot be changed here"
                    .to_owned()
            })?;
        let (response, receiver) = oneshot::channel();
        session
            .maintenance
            .send(MaintenanceEnvelope { request, response })
            .await
            .map_err(|_| "the session that records this capture has ended".to_owned())?;
        tokio::time::timeout(MAINTENANCE_TIMEOUT, receiver)
            .await
            .map_err(|_| "the session did not answer in time".to_owned())?
            .map_err(|_| "the session ended before it answered".to_owned())?
    }

    async fn disconnect(&mut self, endpoint: &str) {
        if let Some(mut session) = self.by_endpoint.remove(endpoint) {
            self.by_database.remove(&session.database_identity);
            if let Some(shutdown) = session.shutdown.take() {
                let _ = shutdown.send(());
            }
            report_session_exit(endpoint, session.task.await);
        }
    }

    async fn enable_rest(
        &mut self,
        endpoint: Option<String>,
        database: Option<PathBuf>,
        bind: std::net::SocketAddr,
        token: Option<String>,
        allow_remote_writes: bool,
    ) -> ControlResponse {
        if self.rest.is_some() {
            return ControlResponse::Error {
                reason: "a REST listener is already active".to_owned(),
            };
        }
        let endpoint = match endpoint {
            Some(endpoint) => match paths::canonical_endpoint(&endpoint) {
                Ok(endpoint) => Some(endpoint),
                Err(reason) => return ControlResponse::Error { reason },
            },
            None => None,
        };
        if endpoint.is_none() && database.is_some() {
            return ControlResponse::Error {
                reason: "REST database override requires an endpoint".to_owned(),
            };
        }
        let database = if let Some(endpoint) = endpoint.as_deref() {
            let requested = match database {
                Some(database) => database,
                None => match self.by_endpoint.get(endpoint) {
                    Some(session) => session.info.database.clone(),
                    None => match paths::database_for_endpoint(endpoint) {
                        Ok(database) => database,
                        Err(reason) => return ControlResponse::Error { reason },
                    },
                },
            };
            let resolved = match resolve_database_path(&requested) {
                Ok(database) => database,
                Err(error) => {
                    return ControlResponse::Error {
                        reason: format!("invalid capture database path: {error}"),
                    };
                }
            };
            if self
                .by_endpoint
                .get(endpoint)
                .is_some_and(|session| session.info.database != resolved)
            {
                return ControlResponse::Error {
                    reason: "endpoint is connected with a different capture database".to_owned(),
                };
            }
            Some(resolved)
        } else {
            None
        };
        let server = match ApiServer::bind(ApiConfig {
            database: database.clone(),
            endpoint: endpoint.clone(),
            managed: true,
            bind,
            token,
            allow_remote_writes,
        })
        .await
        {
            Ok(server) => server,
            Err(error) => {
                return ControlResponse::Error {
                    reason: format!("cannot enable REST: {error}"),
                };
            }
        };
        let address = server.address();
        let (shutdown, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(server.run(async move {
            let _ = shutdown_rx.await;
        }));
        let status = RestStatus {
            enabled: true,
            endpoint,
            bind: Some(address),
            allow_remote_writes,
        };
        self.rest = Some(ActiveRest {
            status: status.clone(),
            database,
            shutdown: Some(shutdown),
            task,
        });
        ControlResponse::Rest { status }
    }

    fn lifecycle(&self, lifecycle: &Lifecycle) -> ControlResponse {
        ControlResponse::Lifecycle {
            state: lifecycle.state(self.by_endpoint.len()),
        }
    }

    fn rest_status(&self) -> RestStatus {
        self.rest
            .as_ref()
            .map_or_else(disabled_rest_status, |rest| rest.status.clone())
    }

    /// Remove sessions that ended by themselves. The idle period starts when
    /// the last one ends, not when it was connected.
    async fn reap_finished(&mut self, lifecycle: &Lifecycle) {
        let before = self.by_endpoint.len();
        self.reap().await;
        if self.by_endpoint.len() < before {
            lifecycle.touch();
        }
    }

    async fn reap(&mut self) {
        self.reap_rest().await;
        let finished = self
            .by_endpoint
            .iter()
            .filter(|(_, session)| session.task.is_finished())
            .map(|(endpoint, _)| endpoint.clone())
            .collect::<Vec<_>>();
        for endpoint in finished {
            self.disconnect(&endpoint).await;
        }
    }

    async fn reap_rest(&mut self) {
        if self
            .rest
            .as_ref()
            .is_some_and(|rest| rest.task.is_finished())
        {
            self.disable_rest().await;
        }
    }

    async fn disable_rest(&mut self) {
        if let Some(mut rest) = self.rest.take() {
            if let Some(shutdown) = rest.shutdown.take() {
                let _ = shutdown.send(());
            }
            match tokio::time::timeout(Duration::from_secs(5), &mut rest.task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => eprintln!("REST listener stopped with an error: {error}"),
                Ok(Err(error)) => eprintln!("REST listener task failed: {error}"),
                Err(_) => {
                    rest.task.abort();
                    let _ = rest.task.await;
                    eprintln!("REST listener did not finish graceful shutdown within five seconds");
                }
            }
        }
    }

    async fn stop_all(&mut self) {
        self.disable_rest().await;
        let endpoints = self.by_endpoint.keys().cloned().collect::<Vec<_>>();
        for endpoint in endpoints {
            self.disconnect(&endpoint).await;
        }
    }
}

const fn disabled_rest_status() -> RestStatus {
    RestStatus {
        enabled: false,
        endpoint: None,
        bind: None,
        allow_remote_writes: false,
    }
}

async fn run_session(
    service: CaptureService,
    ipc: IpcServer,
    shutdown: oneshot::Receiver<()>,
) -> Result<(), String> {
    let state = service.subscribe_state();
    let frames = service.subscribe_frames();
    let routing_losses = service.subscribe_routing_losses();
    let operations = service.operation_sender();
    let ipc_task = tokio::spawn(ipc.run(state, frames, routing_losses, operations));
    let (service_shutdown, service_shutdown_rx) = oneshot::channel();
    let mut service_task = tokio::spawn(async move {
        let mut service = service;
        service
            .run_until(async move {
                let _ = service_shutdown_rx.await;
            })
            .await
    });
    let mut ipc_task = ipc_task;
    let mut shutdown = shutdown;

    let result = tokio::select! {
        biased;
        _ = &mut shutdown => {
            let _ = service_shutdown.send(());
            service_task
                .await
                .map_err(|error| format!("capture task failed: {error}"))?
                .map_err(|error| error.to_string())
        }
        result = &mut service_task => {
            result
                .map_err(|error| format!("capture task failed: {error}"))?
                .map_err(|error| error.to_string())
        }
        result = &mut ipc_task => {
            let _ = service_shutdown.send(());
            let _ = service_task.await;
            match result {
                Ok(Err(error)) => Err(format!("capture IPC stopped: {error}")),
                Ok(Ok(())) => Err("capture IPC stopped unexpectedly".to_owned()),
                Err(error) => Err(format!("capture IPC task failed: {error}")),
            }
        }
    };
    ipc_task.abort();
    let _ = ipc_task.await;
    result
}

fn report_session_exit(endpoint: &str, result: Result<Result<(), String>, tokio::task::JoinError>) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!("capture session {endpoint} stopped: {error}"),
        Err(error) => eprintln!("capture session {endpoint} task failed: {error}"),
    }
}

fn resolve_database_path(path: &Path) -> io::Result<PathBuf> {
    if path == Path::new(":memory:") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "daemon capture databases must be file-backed",
        ));
    }
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "database path has no filename")
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()?.join(parent)
    };
    Ok(resolve_parent_allow_missing(&parent)?.join(name))
}

fn resolve_parent_allow_missing(path: &Path) -> io::Result<PathBuf> {
    let mut resolved = PathBuf::new();
    let mut missing = Vec::<OsString>::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if missing.pop().is_none() {
                    resolved.pop();
                    if !resolved.as_os_str().is_empty() {
                        resolved = std::fs::canonicalize(&resolved)?;
                    }
                }
            }
            Component::Normal(name) if missing.is_empty() => {
                let candidate = resolved.join(name);
                match std::fs::canonicalize(&candidate) {
                    Ok(canonical) => resolved = canonical,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        missing.push(name.to_os_string());
                    }
                    Err(error) => return Err(error),
                }
            }
            Component::Normal(name) => missing.push(name.to_os_string()),
        }
    }
    for component in missing {
        resolved.push(component);
    }
    Ok(resolved)
}

fn database_identity(path: &Path) -> io::Result<DatabaseIdentity> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "database file must not be a symbolic link",
                ));
            }
            if !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "database path is not a regular file",
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                Ok(DatabaseIdentity::File {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                })
            }
            #[cfg(not(unix))]
            {
                Ok(DatabaseIdentity::Path(std::fs::canonicalize(path)?))
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(DatabaseIdentity::Path(path.to_path_buf()))
        }
        Err(error) => Err(error),
    }
}

fn other_error(error: &impl ToString) -> Box<dyn Error + Send + Sync> {
    Box::new(io::Error::other(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Different relative spellings resolve to one database path.
    #[test]
    fn relative_database_paths_are_normalized() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first = resolve_database_path(&directory.path().join("nested/../capture.sqlite"))
            .expect("resolve first spelling");
        let second = resolve_database_path(&directory.path().join("capture.sqlite"))
            .expect("resolve second spelling");
        assert_eq!(first, second);
    }

    /// Symbolic-link parent aliases resolve to the same database path.
    #[cfg(unix)]
    #[test]
    fn symlinked_database_parents_are_normalized() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let actual = directory.path().join("actual");
        std::fs::create_dir(&actual).expect("create actual directory");
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&actual, &alias).expect("create directory symlink");
        let first =
            resolve_database_path(&actual.join("capture.sqlite")).expect("resolve actual path");
        let second =
            resolve_database_path(&alias.join("capture.sqlite")).expect("resolve alias path");
        assert_eq!(first, second);
    }

    /// A lifecycle that nothing here waits on.
    fn lifecycle() -> Lifecycle {
        Lifecycle::new(Duration::ZERO, IdleSource::Default, Arc::new(SystemClock))
    }

    /// The daemon asks the session that records a capture to count and clear
    /// it, with a copy first, and refuses a capture no session records.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_daemon_counts_and_clears_a_capture_through_its_session() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        let archive_dir = directory.path().join("archive");
        let gateway = knx_rs_ip::DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let endpoint = format!("tunnel://{}", gateway.local_addr());
        let mut sessions = DaemonSessions {
            archive_dir: Some(archive_dir.clone()),
            ..DaemonSessions::default()
        };
        let lifecycle = lifecycle();
        let connected = sessions
            .handle(
                ControlRequest::Connect {
                    endpoint,
                    database: Some(database.clone()),
                    max_events: 100,
                },
                &lifecycle,
            )
            .await;
        assert!(
            matches!(connected, ControlResponse::Session { .. }),
            "{connected:?}"
        );

        let counted = sessions
            .handle(
                ControlRequest::CaptureCounts {
                    database: database.clone(),
                },
                &lifecycle,
            )
            .await;
        assert_eq!(
            counted,
            ControlResponse::CaptureCounts {
                events: 0,
                losses: 0
            }
        );

        let cleared = sessions
            .handle(
                ControlRequest::ClearCapture {
                    database: database.clone(),
                    archive: true,
                },
                &lifecycle,
            )
            .await;
        let ControlResponse::CaptureCleared { archive, .. } = cleared else {
            panic!("not cleared: {cleared:?}");
        };
        let archive = archive.expect("a copy was asked for");
        assert!(archive.starts_with(&archive_dir), "{archive:?}");
        assert!(archive.exists(), "the copy is not at {archive:?}");

        // Without a copy there is no file to look for, and nothing is made.
        let before = std::fs::read_dir(&archive_dir).unwrap().count();
        let cleared = sessions
            .handle(
                ControlRequest::ClearCapture {
                    database: database.clone(),
                    archive: false,
                },
                &lifecycle,
            )
            .await;
        assert!(
            matches!(
                cleared,
                ControlResponse::CaptureCleared { archive: None, .. }
            ),
            "{cleared:?}"
        );
        assert_eq!(std::fs::read_dir(&archive_dir).unwrap().count(), before);

        // Counter-probe: a capture no session records is refused.
        let other = directory.path().join("another.sqlite");
        let refused = sessions
            .handle(
                ControlRequest::ClearCapture {
                    database: other,
                    archive: false,
                },
                &lifecycle,
            )
            .await;
        assert!(
            matches!(&refused, ControlResponse::Error { reason } if reason.contains("not being recorded")),
            "{refused:?}"
        );
        sessions.stop_all().await;
    }

    /// A daemon a client starts waits for the endpoint at least as long as the
    /// client waits for that daemon, or the client gives up first.
    #[test]
    fn a_started_daemon_waits_as_long_as_its_client() {
        assert!(OWNER_WAIT >= crate::control::DAEMON_START_TIMEOUT);
    }
}
