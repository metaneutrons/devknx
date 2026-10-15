// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Per-user control IPC for the long-lived daemon.
//!
//! The control endpoint is stable across capture databases and never carries
//! live KNX frames. Each client exchanges one bounded JSON line per connection.

#[cfg(unix)]
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::net::SocketAddr;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use interprocess::local_socket::tokio::Stream;
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use interprocess::local_socket::{
    ListenerOptions,
    tokio::{Listener, prelude::*},
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc, oneshot};

use crate::paths;

const MAX_MESSAGE_SIZE: usize = 16 * 1024;
const MAX_CLIENTS: usize = 32;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a client waits for a daemon it started, or for one to stop.
///
/// It has to cover a stopping daemon's drain and the closing of its sessions,
/// which the daemon started in its place waits for (`daemon` has the same
/// bound), and a start that is slow on a loaded machine.
pub const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(20);
/// How many daemons a client starts while it waits: the first, and another when
/// the one it started has ended without answering, as one does that gave up
/// waiting for a stopping daemon's endpoint.
const MAX_DAEMON_SPAWNS: u32 = 4;
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// One request sent to the current user's daemon.
#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlRequest {
    Ping,
    /// Which build the daemon is. A request of its own, so a client can still
    /// reach a daemon too old to know it and warn about it.
    Identify,
    List,
    Connect {
        endpoint: String,
        database: Option<PathBuf>,
        max_events: u32,
    },
    Disconnect {
        endpoint: String,
    },
    DisconnectScoped {
        database: PathBuf,
    },
    RestEnable {
        endpoint: Option<String>,
        #[serde(default)]
        database: Option<PathBuf>,
        bind: SocketAddr,
        token: Option<String>,
        allow_remote_writes: bool,
    },
    RestDisable,
    RestStatus,
    /// Create or extend a long-lived client's lease, which holds the daemon
    /// while no KNX session does. It lapses after
    /// [`LEASE_TTL`](crate::lifecycle::LEASE_TTL) without renewal.
    RenewLease {
        client: String,
        kind: crate::lifecycle::LeaseKind,
    },
    /// End a lease before it lapses, when the client quits normally.
    ReleaseLease {
        client: String,
    },
    /// Change the running daemon's idle period. `0` means never stop by
    /// itself. Lasts until this daemon stops.
    SetIdleTimeout {
        seconds: u64,
    },
    /// The daemon's idle period, its source, and what holds it.
    GetLifecycle,
    /// How much the capture in `database` holds. The daemon answers only for a
    /// capture one of its sessions is recording.
    CaptureCounts {
        database: PathBuf,
    },
    /// Delete the capture in `database`, after a copy in the archive
    /// directory when `archive` is set. The session that records it does the
    /// deleting, between two telegrams.
    ClearCapture {
        database: PathBuf,
        archive: bool,
    },
    Stop,
}

impl ControlRequest {
    /// Whether sending the request twice does what sending it once does.
    ///
    /// Connecting an endpoint that is connected answers with its session, and
    /// the others only read or set an absolute value. Disconnecting is not on
    /// the list: its second answer would be an error for the session the first
    /// one removed.
    #[must_use]
    pub const fn is_repeatable(&self) -> bool {
        matches!(
            self,
            Self::Ping
                | Self::Identify
                | Self::List
                | Self::Connect { .. }
                | Self::RestStatus
                | Self::RenewLease { .. }
                | Self::ReleaseLease { .. }
                | Self::SetIdleTimeout { .. }
                | Self::GetLifecycle
                | Self::CaptureCounts { .. }
        )
    }
}

/// Result returned by the daemon for one control request.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlResponse {
    Pong,
    Sessions {
        sessions: Vec<SessionInfo>,
    },
    Session {
        session: SessionInfo,
    },
    Disconnected,
    Rest {
        status: RestStatus,
    },
    Stopped,
    Identity {
        identity: crate::identity::BuildIdentity,
    },
    Lifecycle {
        state: crate::lifecycle::LifecycleState,
    },
    /// What a capture holds.
    CaptureCounts {
        events: u64,
        losses: u64,
    },
    /// A capture was cleared: this is what it held, and where the copy went.
    CaptureCleared {
        events: u64,
        losses: u64,
        archive: Option<PathBuf>,
    },
    /// The daemon is stopping and did not act on the request. A client
    /// starts a new daemon and sends the request again.
    Stopping,
    Error {
        reason: String,
    },
}

/// Public endpoint and database identity for one configured session.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SessionInfo {
    pub endpoint: String,
    pub database: PathBuf,
    /// The session's retention limit, so a restart can reconnect it as it was.
    #[serde(default)]
    pub max_events: Option<u32>,
}

/// Daemon-owned REST listener state. Credentials are deliberately excluded.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RestStatus {
    pub enabled: bool,
    pub endpoint: Option<String>,
    pub bind: Option<SocketAddr>,
    pub allow_remote_writes: bool,
}

/// Request delivered to the daemon's control loop.
pub struct ControlCall {
    pub request: ControlRequest,
    pub response: oneshot::Sender<ControlResponse>,
    written: oneshot::Receiver<()>,
}

impl ControlCall {
    pub(crate) fn into_request_and_reply(self) -> (ControlRequest, ControlReply) {
        (
            self.request,
            ControlReply {
                response: self.response,
                written: self.written,
            },
        )
    }
}

pub(crate) struct ControlReply {
    response: oneshot::Sender<ControlResponse>,
    written: oneshot::Receiver<()>,
}

impl ControlReply {
    /// A stop response must reach the transport before the daemon's runtime
    /// exits. Other responses must not block the control loop on slow clients.
    pub(crate) async fn respond(self, response: ControlResponse) {
        let stopping = matches!(response, ControlResponse::Stopped);
        let _ = self.response.send(response);
        if stopping {
            // The transport signals completion after its bounded write attempt.
            // Dropping the sender also releases a wait if its task is cancelled.
            let _ = self.written.await;
        }
    }
}

/// One authenticated current-user listener for daemon control requests.
pub struct ControlServer {
    listener: Listener,
    #[cfg(unix)]
    _owner_lease: File,
}

impl ControlServer {
    /// Bind the stable per-user control endpoint.
    ///
    /// # Errors
    ///
    /// Fails if the data directory is unavailable, the endpoint is insecure,
    /// or another daemon already owns it.
    pub fn bind() -> io::Result<Self> {
        #[cfg(unix)]
        {
            let path = socket_path(true)?;
            let (listener, owner_lease) = bind_unix(&path)?;
            Ok(Self {
                listener,
                _owner_lease: owner_lease,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                listener: bind_windows(&pipe_name()?)?,
            })
        }
    }

    /// Accept bounded requests until the daemon drops the listener task.
    ///
    /// # Errors
    ///
    /// Returns a listener error. Malformed or stalled clients are isolated to
    /// their own connection.
    pub async fn run(self, calls: mpsc::Sender<ControlCall>) -> io::Result<()> {
        let slots = Arc::new(Semaphore::new(MAX_CLIENTS));
        loop {
            let stream = self.listener.accept().await?;
            let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
                drop(stream);
                continue;
            };
            let calls = calls.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let _ = handle_client(stream, calls).await;
            });
        }
    }
}

/// Client for the current user's daemon control socket.
pub struct ControlClient;

impl ControlClient {
    /// Ensure the daemon is reachable, then issue one request.
    ///
    /// Stop is intentionally sent only to an already-running daemon.
    ///
    /// # Errors
    ///
    /// Returns a local IPC, timeout, serialization, or startup error.
    pub async fn request(request: ControlRequest) -> io::Result<ControlResponse> {
        if matches!(request, ControlRequest::Stop) {
            return Self::request_existing(request).await;
        }
        // A daemon can stop between the check and the request, through its
        // idle stop. It then refuses the connection or answers `Stopping`;
        // either way it did not act on the request, so starting a new daemon
        // and sending once more is safe whatever the request does. A daemon
        // that is already going away can also close a connection it accepted
        // without an answer, which Windows reports as an unexpected end of the
        // stream. The request may have been carried out then, so it is sent
        // again only when sending it twice does no harm. Any other failure is
        // returned as it is: the daemon may have acted on it.
        let deadline = Instant::now() + DAEMON_START_TIMEOUT;
        loop {
            Self::ensure_daemon().await?;
            match Self::request_existing(request.clone()).await {
                Ok(ControlResponse::Stopping) => {}
                Err(error) if daemon_unavailable(&error) => {}
                Err(error) if request.is_repeatable() && ended_without_answer(&error) => {}
                other => return other,
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "the daemon kept stopping for {} seconds",
                        DAEMON_START_TIMEOUT.as_secs()
                    ),
                ));
            }
            tokio::time::sleep(DAEMON_POLL_INTERVAL).await;
        }
    }

    /// Send one request without starting a daemon.
    ///
    /// # Errors
    ///
    /// Returns a local IPC, timeout, or serialization error.
    pub async fn request_existing(request: ControlRequest) -> io::Result<ControlResponse> {
        request_at_default_path(request).await
    }

    /// Start the current executable's daemon command if needed and wait for Ping.
    ///
    /// Concurrent callers may each attempt a spawn. The daemon's exclusive
    /// listener lease makes all but one process exit without owning the socket.
    ///
    /// # Errors
    ///
    /// Returns an IPC, process-startup, security, or timeout error.
    pub async fn ensure_daemon() -> io::Result<()> {
        match Self::request_existing(ControlRequest::Ping).await {
            Ok(ControlResponse::Pong) => return Ok(()),
            // A stopping daemon gives up the endpoint; the new one waits for it.
            Ok(ControlResponse::Stopping) => {}
            Ok(_) => return Err(invalid_data("daemon returned an unexpected Ping response")),
            // A ping can be sent again, so a connection that ended without an
            // answer, as one does when the daemon is going away, is no
            // different from no daemon.
            Err(error) if daemon_unavailable(&error) || ended_without_answer(&error) => {}
            Err(error) => return Err(error),
        }

        let mut child = spawn_daemon()?;
        let mut spawned = 1_u32;
        let deadline = Instant::now() + DAEMON_START_TIMEOUT;
        loop {
            match Self::request_existing(ControlRequest::Ping).await {
                Ok(ControlResponse::Pong) => return Ok(()),
                Ok(ControlResponse::Stopping) => {}
                Ok(_) => {
                    return Err(invalid_data("daemon returned an unexpected Ping response"));
                }
                Err(error) if daemon_unavailable(&error) || ended_without_answer(&error) => {}
                Err(error) => return Err(error),
            }
            // The daemon started here can be gone before it answered: it gave
            // up waiting for a stopping daemon's endpoint, or it lost the race
            // to another that is not answering yet. Nothing answers, so start
            // another, a few times at most. A daemon that lost the race to one
            // that answers is not seen here: the ping above would have
            // returned.
            if should_respawn(child.try_wait()?.is_some(), spawned) {
                child = spawn_daemon()?;
                spawned += 1;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "daemon did not become available within {} seconds",
                        DAEMON_START_TIMEOUT.as_secs()
                    ),
                ));
            }
            tokio::time::sleep(DAEMON_POLL_INTERVAL).await;
        }
    }

    /// The warning when the running daemon is another build than this
    /// process, or `None` when it is the same build, the user opted out, or
    /// no daemon answers.
    ///
    /// A daemon too old to know `Identify` refuses the request; that is
    /// reported as a build that cannot say which it is, which it certainly
    /// differs from this one.
    pub async fn build_warning() -> Option<String> {
        let ours = crate::identity::BuildIdentity::current();
        let theirs = match Self::request_existing(ControlRequest::Identify).await {
            Ok(ControlResponse::Identity { identity }) => identity,
            Ok(ControlResponse::Error { .. }) => crate::identity::BuildIdentity {
                version: "an older build".to_owned(),
                commit: None,
                dirty: None,
                stamp: String::new(),
                executable: None,
            },
            _ => return None,
        };
        crate::identity::warning(&ours, &theirs)
    }

    /// Replace the running daemon with one started from this executable.
    ///
    /// The daemon keeps its KNX sessions only in memory, so they are listed
    /// first and connected again afterwards, each with its database and
    /// retention limit. A REST listener is not restored: the daemon does not
    /// report its token, and enabling it again without one would weaken it.
    ///
    /// # Errors
    ///
    /// Returns an IPC error, or a timeout when the old daemon does not stop
    /// or a new one does not start.
    pub async fn restart() -> io::Result<Restart> {
        let (sessions, rest_was_enabled, was_running) =
            match Self::request_existing(ControlRequest::List).await {
                Ok(ControlResponse::Sessions { sessions }) => {
                    let rest = matches!(
                        Self::request_existing(ControlRequest::RestStatus).await,
                        Ok(ControlResponse::Rest { status }) if status.enabled
                    );
                    (sessions, rest, true)
                }
                Ok(_) => (Vec::new(), false, true),
                Err(error) if daemon_unavailable(&error) => (Vec::new(), false, false),
                Err(error) => return Err(error),
            };
        if was_running {
            match Self::request_existing(ControlRequest::Stop).await {
                Ok(_) => {}
                Err(error) if daemon_unavailable(&error) => {}
                Err(error) => return Err(error),
            }
            Self::wait_stopped().await?;
        }
        let ControlResponse::Identity { identity } =
            Self::request(ControlRequest::Identify).await?
        else {
            return Err(invalid_data("the new daemon did not identify itself"));
        };
        let mut reconnected = Vec::new();
        let mut failed = Vec::new();
        for session in sessions {
            let response = Self::request_existing(ControlRequest::Connect {
                endpoint: session.endpoint.clone(),
                database: Some(session.database.clone()),
                max_events: session.max_events.unwrap_or(DEFAULT_MAX_EVENTS),
            })
            .await;
            let reason = match response {
                Ok(ControlResponse::Session { session }) => {
                    reconnected.push(session);
                    continue;
                }
                Ok(ControlResponse::Error { reason }) => reason,
                Ok(_) => "unexpected response".to_owned(),
                Err(error) => error.to_string(),
            };
            failed.push(FailedSession { session, reason });
        }
        Ok(Restart {
            was_running,
            identity,
            reconnected,
            failed,
            rest_was_enabled,
        })
    }

    /// Wait until the daemon no longer accepts connections.
    async fn wait_stopped() -> io::Result<()> {
        let deadline = Instant::now() + DAEMON_START_TIMEOUT;
        loop {
            match Self::request_existing(ControlRequest::Ping).await {
                Err(error) if daemon_unavailable(&error) => return Ok(()),
                _ if Instant::now() >= deadline => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "the daemon did not stop within {} seconds",
                            DAEMON_START_TIMEOUT.as_secs()
                        ),
                    ));
                }
                _ => tokio::time::sleep(DAEMON_POLL_INTERVAL).await,
            }
        }
    }
}

/// The retention limit `devknx connect` uses when none is given, and the one
/// a restart uses for a session whose daemon did not report its limit.
pub const DEFAULT_MAX_EVENTS: u32 = 100_000;

/// What `devknx daemon --restart` did.
#[derive(Clone, Debug, Serialize)]
pub struct Restart {
    /// Whether a daemon was running before.
    pub was_running: bool,
    /// The build of the daemon now running.
    pub identity: crate::identity::BuildIdentity,
    /// Sessions connected again in the new daemon.
    pub reconnected: Vec<SessionInfo>,
    /// Sessions the new daemon could not connect again, with the reason.
    pub failed: Vec<FailedSession>,
    /// Whether the old daemon had a REST listener, which was not restored.
    pub rest_was_enabled: bool,
}

/// A session a restart could not connect again.
#[derive(Clone, Debug, Serialize)]
pub struct FailedSession {
    #[serde(flatten)]
    pub session: SessionInfo,
    pub reason: String,
}

async fn handle_client<S>(mut stream: S, calls: mpsc::Sender<ControlCall>) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request_bytes = match read_frame(&mut stream).await {
        Ok(bytes) => bytes,
        Err(error) => {
            if error.kind() == io::ErrorKind::InvalidData {
                let _ = write_frame(
                    &mut stream,
                    &ControlResponse::Error {
                        reason: "invalid control request".to_owned(),
                    },
                )
                .await;
            }
            return Err(error);
        }
    };
    let request: ControlRequest = if let Ok(request) = serde_json::from_slice(&request_bytes) {
        request
    } else {
        write_frame(
            &mut stream,
            &ControlResponse::Error {
                reason: "invalid control request".to_owned(),
            },
        )
        .await?;
        return Ok(());
    };
    let (response_tx, response_rx) = oneshot::channel();
    let (written_tx, written_rx) = oneshot::channel();
    match tokio::time::timeout(
        IO_TIMEOUT,
        calls.send(ControlCall {
            request,
            response: response_tx,
            written: written_rx,
        }),
    )
    .await
    {
        Ok(Ok(())) => {}
        // The control loop is gone: the daemon stopped, and did not act on
        // the request. Saying so lets the client send it to a new daemon.
        Ok(Err(_)) => return write_frame(&mut stream, &ControlResponse::Stopping).await,
        Err(_) => {
            return write_frame(
                &mut stream,
                &ControlResponse::Error {
                    reason: "daemon control queue timed out".to_owned(),
                },
            )
            .await;
        }
    }
    let response = match tokio::time::timeout(IO_TIMEOUT, response_rx).await {
        Ok(Ok(response)) => response,
        // The loop dropped the request without answering it, as it does when
        // it ends: the daemon is stopping and did not act on it.
        Ok(Err(_)) => ControlResponse::Stopping,
        Err(_) => ControlResponse::Error {
            reason: "daemon control response timed out".to_owned(),
        },
    };
    let result = write_frame(&mut stream, &response).await;
    let _ = written_tx.send(());
    result
}

async fn request_at_default_path(request: ControlRequest) -> io::Result<ControlResponse> {
    let encoded = encode_frame(&request)?;
    let mut stream = tokio::time::timeout(IO_TIMEOUT, connect_default())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "control connection timed out"))??;
    tokio::time::timeout(IO_TIMEOUT, stream.write_all(&encoded))
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "control request write timed out")
        })??;
    let response = read_frame(&mut stream).await?;
    serde_json::from_slice(&response).map_err(|error| invalid_data(error.to_string()))
}

#[cfg(unix)]
async fn connect_default() -> io::Result<Stream> {
    let path = socket_path(false)?;
    Stream::connect(path.as_os_str().to_fs_name::<GenericFilePath>()?).await
}

#[cfg(windows)]
async fn connect_default() -> io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    crate::windows_pipe::connect(&pipe_name()?).await
}

async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<Vec<u8>> {
    tokio::time::timeout(IO_TIMEOUT, async {
        let mut bytes = Vec::new();
        loop {
            match stream.read_u8().await? {
                b'\n' => return Ok(bytes),
                byte if bytes.len() < MAX_MESSAGE_SIZE => bytes.push(byte),
                _ => return Err(invalid_data("control message exceeds 16 KiB")),
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "control request timed out"))?
}

async fn write_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    response: &ControlResponse,
) -> io::Result<()> {
    let encoded = encode_frame(response)?;
    tokio::time::timeout(IO_TIMEOUT, async {
        stream.write_all(&encoded).await?;
        stream.flush().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "control response write timed out"))??;
    Ok(())
}

fn encode_frame(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut encoded = serde_json::to_vec(value).map_err(|error| invalid_data(error.to_string()))?;
    if encoded.len() >= MAX_MESSAGE_SIZE {
        return Err(invalid_data("control message exceeds 16 KiB"));
    }
    encoded.push(b'\n');
    Ok(encoded)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Whether the connection ended before an answer came.
fn ended_without_answer(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
    )
}

fn daemon_unavailable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::AddrNotAvailable
    )
}

/// Remove the control endpoint so a late client's connect fails at once.
///
/// Only the daemon that owns the endpoint calls this, while it still holds
/// the owner lock, so no other daemon can have bound the path yet. On
/// Windows the pipe goes when its listener does.
#[cfg(unix)]
pub fn release_endpoint() {
    if let Ok(path) = socket_path(false) {
        let _ = std::fs::remove_file(path);
    }
}

/// Remove the control endpoint so a late client's connect fails at once.
///
/// Nothing to do on Windows: the pipe goes when its listener does.
#[cfg(windows)]
pub const fn release_endpoint() {}

/// Whether a client that waits for a daemon starts another: the one it started
/// has ended, and it has not started too many.
const fn should_respawn(ended: bool, spawned: u32) -> bool {
    ended && spawned < MAX_DAEMON_SPAWNS
}

/// Where the daemon started by a client writes what it prints, so a test can
/// show it when something goes wrong. Only debug builds read this; a release
/// build always discards the output.
#[cfg(debug_assertions)]
fn daemon_log() -> Option<std::fs::File> {
    let path = std::env::var_os("DEVKNX_DAEMON_LOG").filter(|path| !path.is_empty())?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

#[cfg(not(debug_assertions))]
const fn daemon_log() -> Option<std::fs::File> {
    None
}

fn spawn_daemon() -> io::Result<std::process::Child> {
    use std::process::{Command, Stdio};

    let (stdout, stderr) = match daemon_log().map(|log| log.try_clone().map(|copy| (log, copy))) {
        Some(Ok((out, err))) => (Stdio::from(out), Stdio::from(err)),
        _ => (Stdio::null(), Stdio::null()),
    };
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["daemon", "--wait-for-endpoint"])
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(DETACHED_PROCESS);
    }
    command.spawn()
}

#[cfg(unix)]
fn socket_path(create: bool) -> io::Result<PathBuf> {
    use std::hash::Hasher as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};

    let configured_data_dir =
        paths::data_dir().map_err(|error| io::Error::new(io::ErrorKind::NotFound, error))?;
    if create {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(&configured_data_dir)?;
    }
    let data_dir = configured_data_dir.canonicalize()?;
    let data_metadata = std::fs::metadata(&data_dir)?;
    if data_metadata.permissions().mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "application data directory is writable by other users",
        ));
    }

    let adjacent = data_dir.join("control");
    let directory = if adjacent.join("control.sock").as_os_str().as_bytes().len() < 100 {
        adjacent
    } else {
        let mut hasher = StableHasher::default();
        hasher.write(data_dir.as_os_str().as_bytes());
        std::fs::canonicalize("/tmp")?.join(format!("devknx-control-{:016x}", hasher.finish()))
    };
    if create {
        create_private_directory(&directory)?;
    }
    let metadata = std::fs::symlink_metadata(&directory)?;
    if !metadata.file_type().is_dir()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.uid() != data_metadata.uid()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control IPC directory must be private, owned by this user, and not a symlink",
        ));
    }
    Ok(directory.join("control.sock"))
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

#[derive(Default)]
struct StableHasher(u64);

impl std::hash::Hasher for StableHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        if self.0 == 0 {
            self.0 = 0xcbf2_9ce4_8422_2325;
        }
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

#[cfg(unix)]
fn bind_unix(path: &Path) -> io::Result<(Listener, File)> {
    use std::os::unix::fs::PermissionsExt as _;

    let owner_lease = acquire_owner_lease(path)?;
    let name = path.as_os_str().to_fs_name::<GenericFilePath>()?;
    let options = ListenerOptions::new().name(name).try_overwrite(true);
    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd"))]
    let options = {
        use interprocess::os::unix::local_socket::ListenerOptionsExt as _;
        options.mode(0o600)
    };
    let listener = options.create_tokio()?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok((listener, owner_lease))
}

#[cfg(unix)]
fn acquire_owner_lease(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let lease_path = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing control directory"))?
        .join("owner.lock");
    match std::fs::symlink_metadata(&lease_path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "control owner lock is not a regular file",
            ));
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lease_path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "a daemon already owns the control endpoint",
        )),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

#[cfg(windows)]
fn pipe_name() -> io::Result<String> {
    use std::hash::Hasher as _;
    use std::os::windows::ffi::OsStrExt as _;

    let sid = windows_permissions::utilities::current_process_sid()?;
    let data_dir = paths::data_dir().map_err(io::Error::other)?;
    let mut hasher = StableHasher::default();
    for unit in data_dir.as_os_str().encode_wide() {
        hasher.write(&unit.to_le_bytes());
    }
    Ok(format!("devknx-{sid}-{:016x}-control", hasher.finish()))
}

#[cfg(windows)]
fn bind_windows(name: &str) -> io::Result<Listener> {
    use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    use widestring::U16CString;

    let sid = windows_permissions::utilities::current_process_sid()?;
    let sddl = format!("D:P(A;;GA;;;{sid})");
    let wide = U16CString::from_str(&sddl)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let descriptor = SecurityDescriptor::deserialize(&wide)?;
    ListenerOptions::new()
        .name(name.to_ns_name::<GenericNamespaced>()?)
        .security_descriptor(descriptor)
        .create_tokio()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_daemon_is_started_only_when_the_last_has_ended_and_not_too_often() {
        assert!(
            !should_respawn(false, 1),
            "a daemon that still runs is waited for"
        );
        assert!(should_respawn(true, 1));
        assert!(should_respawn(true, MAX_DAEMON_SPAWNS - 1));
        // Counter-probe: after the last allowed start the client only waits.
        assert!(!should_respawn(true, MAX_DAEMON_SPAWNS));
    }

    #[test]
    fn only_requests_that_do_no_harm_twice_are_repeated() {
        assert!(
            ControlRequest::Connect {
                endpoint: "tunnel://127.0.0.1:3671".into(),
                database: None,
                max_events: 1,
            }
            .is_repeatable()
        );
        assert!(ControlRequest::List.is_repeatable());
        // Counter-probes: a second disconnect, REST change or stop is not the
        // same as the first.
        assert!(
            !ControlRequest::Disconnect {
                endpoint: "tunnel://127.0.0.1:3671".into(),
            }
            .is_repeatable()
        );
        assert!(!ControlRequest::RestDisable.is_repeatable());
        assert!(!ControlRequest::Stop.is_repeatable());
        assert!(ended_without_answer(&io::Error::from(
            io::ErrorKind::UnexpectedEof
        )));
        assert!(!ended_without_answer(&io::Error::from(
            io::ErrorKind::TimedOut
        )));
    }

    #[tokio::test]
    async fn stop_reply_waits_for_the_complete_transport_frame() {
        let (mut client, stream) = tokio::io::duplex(1);
        let (calls_tx, mut calls_rx) = mpsc::channel(1);
        let transport = tokio::spawn(handle_client(stream, calls_tx));
        client
            .write_all(&encode_frame(&ControlRequest::Stop).unwrap())
            .await
            .unwrap();
        let call = calls_rx.recv().await.unwrap();
        let reply = tokio::spawn(
            call.into_request_and_reply()
                .1
                .respond(ControlResponse::Stopped),
        );

        // One byte fits in the pipe. The rest cannot be written until this
        // test reads it, so the shutdown barrier is exercised deterministically.
        let first = client.read_u8().await.unwrap();
        assert!(
            !reply.is_finished(),
            "daemon exited before writing its reply"
        );
        let mut frame = vec![first];
        frame.extend(read_frame(&mut client).await.unwrap());
        assert_eq!(
            serde_json::from_slice::<ControlResponse>(&frame).unwrap(),
            ControlResponse::Stopped
        );
        tokio::time::timeout(IO_TIMEOUT, reply)
            .await
            .unwrap()
            .unwrap();
        transport.await.unwrap().unwrap();
    }

    /// A request that reaches a connection after the daemon's control loop has
    /// ended is answered `Stopping`, not with an error that looks like a
    /// failure of the request.
    #[tokio::test]
    async fn a_request_after_the_control_loop_ended_is_answered_stopping() {
        // The loop is gone before the request arrives.
        let (mut client, stream) = tokio::io::duplex(4096);
        let (calls_tx, calls_rx) = mpsc::channel(1);
        drop(calls_rx);
        let transport = tokio::spawn(handle_client(stream, calls_tx));
        client
            .write_all(&encode_frame(&ControlRequest::Ping).unwrap())
            .await
            .unwrap();
        let answer: ControlResponse =
            serde_json::from_slice(&read_frame(&mut client).await.unwrap()).unwrap();
        assert_eq!(answer, ControlResponse::Stopping);
        transport.await.unwrap().unwrap();

        // The loop took the request and ended without answering it.
        let (mut client, stream) = tokio::io::duplex(4096);
        let (calls_tx, mut calls_rx) = mpsc::channel(1);
        let transport = tokio::spawn(handle_client(stream, calls_tx));
        client
            .write_all(&encode_frame(&ControlRequest::Ping).unwrap())
            .await
            .unwrap();
        drop(calls_rx.recv().await.unwrap());
        let answer: ControlResponse =
            serde_json::from_slice(&read_frame(&mut client).await.unwrap()).unwrap();
        assert_eq!(answer, ControlResponse::Stopping);
        transport.await.unwrap().unwrap();

        // Counter-probe: a request the daemon answers is not turned into one.
        let (mut client, stream) = tokio::io::duplex(4096);
        let (calls_tx, mut calls_rx) = mpsc::channel(1);
        let transport = tokio::spawn(handle_client(stream, calls_tx));
        client
            .write_all(&encode_frame(&ControlRequest::Ping).unwrap())
            .await
            .unwrap();
        let call = calls_rx.recv().await.unwrap();
        call.into_request_and_reply()
            .1
            .respond(ControlResponse::Pong)
            .await;
        let answer: ControlResponse =
            serde_json::from_slice(&read_frame(&mut client).await.unwrap()).unwrap();
        assert_eq!(answer, ControlResponse::Pong);
        transport.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn ordinary_reply_does_not_wait_for_a_slow_transport() {
        let (response, received) = oneshot::channel();
        let (_written_tx, written) = oneshot::channel();
        let call = ControlCall {
            request: ControlRequest::Ping,
            response,
            written,
        };
        let reply = tokio::spawn(
            call.into_request_and_reply()
                .1
                .respond(ControlResponse::Pong),
        );
        assert_eq!(received.await.unwrap(), ControlResponse::Pong);
        assert!(reply.is_finished(), "ordinary control call blocked on I/O");
        reply.await.unwrap();
    }

    #[tokio::test]
    async fn stop_reply_finishes_when_transport_write_times_out() {
        let (mut client, stream) = tokio::io::duplex(1);
        let (calls_tx, mut calls_rx) = mpsc::channel(1);
        let transport = tokio::spawn(handle_client(stream, calls_tx));
        client
            .write_all(&encode_frame(&ControlRequest::Stop).unwrap())
            .await
            .unwrap();
        let call = calls_rx.recv().await.unwrap();
        let reply = tokio::spawn(
            call.into_request_and_reply()
                .1
                .respond(ControlResponse::Stopped),
        );
        // Keep the client open without reading: the one-byte pipe stays full.
        let error = tokio::time::timeout(IO_TIMEOUT + Duration::from_secs(1), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        reply.await.unwrap();
        drop(client);
    }

    #[tokio::test]
    async fn disconnected_stop_client_does_not_block_shutdown() {
        let (mut client, stream) = tokio::io::duplex(1);
        let (calls_tx, mut calls_rx) = mpsc::channel(1);
        let transport = tokio::spawn(handle_client(stream, calls_tx));
        client
            .write_all(&encode_frame(&ControlRequest::Stop).unwrap())
            .await
            .unwrap();
        let call = calls_rx.recv().await.unwrap();
        drop(client);
        tokio::time::timeout(
            IO_TIMEOUT,
            call.into_request_and_reply()
                .1
                .respond(ControlResponse::Stopped),
        )
        .await
        .unwrap();
        assert!(transport.await.unwrap().is_err());
    }

    #[test]
    fn control_protocol_is_tagged_and_rest_status_has_no_token_field() {
        let request = ControlRequest::RestEnable {
            endpoint: Some("tunnel://192.0.2.1:3671".into()),
            database: None,
            bind: "127.0.0.1:8765".parse().unwrap(),
            token: Some("test-secret-token".into()),
            allow_remote_writes: false,
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(encoded.starts_with("{\"action\":\"rest_enable\""));
        assert!(serde_json::from_str::<ControlRequest>(&encoded).unwrap() == request);

        let response = ControlResponse::Rest {
            status: RestStatus {
                enabled: true,
                endpoint: Some("tunnel://192.0.2.1:3671".into()),
                bind: Some("127.0.0.1:8765".parse().unwrap()),
                allow_remote_writes: false,
            },
        };
        let encoded = serde_json::to_string(&response).unwrap();
        assert!(!encoded.contains("token"));
        assert!(!encoded.contains("test-secret-token"));
        assert_eq!(
            serde_json::from_str::<ControlResponse>(&encoded).unwrap(),
            response
        );
    }

    #[test]
    fn frame_encoding_enforces_the_message_limit() {
        assert!(encode_frame(&ControlRequest::Ping).is_ok());
        let too_large = ControlResponse::Error {
            reason: "x".repeat(MAX_MESSAGE_SIZE),
        };
        assert_eq!(
            encode_frame(&too_large).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn one_daemon_owns_a_private_control_socket_at_a_time() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let (listener, lease) = bind_unix(&path).unwrap();
        drop(listener);
        assert!(bind_unix(&path).is_err());
        drop(lease);

        let (listener, lease) = bind_unix(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o077, 0);
        drop(listener);
        drop(lease);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn control_server_routes_one_bounded_call_and_returns_its_response() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("control.sock");
        let (listener, owner_lease) = bind_unix(&path).unwrap();
        let server = ControlServer {
            listener,
            _owner_lease: owner_lease,
        };
        let (calls_tx, mut calls_rx) = mpsc::channel(1);
        let server_task = tokio::spawn(server.run(calls_tx));

        let client_task = tokio::spawn(async move {
            let name = path.as_os_str().to_fs_name::<GenericFilePath>().unwrap();
            let mut stream = Stream::connect(name).await.unwrap();
            let encoded = encode_frame(&ControlRequest::Ping).unwrap();
            stream.write_all(&encoded).await.unwrap();
            let response = read_frame(&mut stream).await.unwrap();
            serde_json::from_slice::<ControlResponse>(&response).unwrap()
        });
        let call = tokio::time::timeout(IO_TIMEOUT, calls_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(call.request, ControlRequest::Ping));
        call.response.send(ControlResponse::Pong).unwrap();
        assert_eq!(client_task.await.unwrap(), ControlResponse::Pong);
        server_task.abort();
    }
}
