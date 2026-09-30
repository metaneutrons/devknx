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
use std::time::Duration;

use knx_rs_ip::parse_url;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::api::{ApiConfig, ApiServer};
use crate::control::{ControlRequest, ControlResponse, ControlServer, RestStatus, SessionInfo};
use crate::ipc::IpcServer;
use crate::paths;
use crate::service::{CaptureService, ReconnectPolicy};
use crate::storage::CaptureStore;

const LIVE_EVENT_CAPACITY: usize = 1_024;

/// Run the per-user daemon until its control API requests shutdown or Ctrl-C
/// is pressed.
///
/// No KNX connection is started automatically.
///
/// # Errors
///
/// Returns an error if the control listener cannot start or stops unexpectedly.
pub async fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let control = ControlServer::bind().map_err(|error| other_error(&error))?;
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
                sessions.reap_finished().await;
            }
            call = calls_rx.recv() => {
                let Some(call) = call else {
                    break Some(Box::new(io::Error::other("control channel closed")) as Box<dyn Error + Send + Sync>);
                };
                sessions.reap_finished().await;
                let response = sessions.handle(call.request).await;
                let stop = matches!(response, ControlResponse::Stopped);
                let _ = call.response.send(response);
                if stop {
                    break None;
                }
            }
        }
    };

    sessions.stop_all().await;
    control_task.abort();
    let _ = control_task.await;

    if let Some(error) = terminal_error {
        return Err(error);
    }
    Ok(())
}

#[derive(Default)]
struct DaemonSessions {
    by_endpoint: HashMap<String, ActiveSession>,
    by_database: HashMap<DatabaseIdentity, String>,
    rest: Option<ActiveRest>,
}

struct ActiveSession {
    info: SessionInfo,
    database_identity: DatabaseIdentity,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), String>>,
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
    async fn handle(&mut self, request: ControlRequest) -> ControlResponse {
        self.reap_rest().await;
        match request {
            ControlRequest::Ping => ControlResponse::Pong,
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
        };
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
            },
        );
        ControlResponse::Session { session: info }
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

    fn rest_status(&self) -> RestStatus {
        self.rest
            .as_ref()
            .map_or_else(disabled_rest_status, |rest| rest.status.clone())
    }

    async fn reap_finished(&mut self) {
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
}
