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
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use interprocess::local_socket::{
    ListenerOptions,
    tokio::{Listener, Stream, prelude::*},
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc, oneshot};

use crate::paths;

const MAX_MESSAGE_SIZE: usize = 16 * 1024;
const MAX_CLIENTS: usize = 32;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// One request sent to the current user's daemon.
#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlRequest {
    Ping,
    List,
    Connect {
        endpoint: String,
        database: Option<PathBuf>,
        max_events: u32,
    },
    Disconnect {
        endpoint: String,
    },
    RestEnable {
        endpoint: String,
        bind: SocketAddr,
        token: Option<String>,
        allow_remote_writes: bool,
    },
    RestDisable,
    RestStatus,
    Stop,
}

/// Result returned by the daemon for one control request.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlResponse {
    Pong,
    Sessions { sessions: Vec<SessionInfo> },
    Session { session: SessionInfo },
    Disconnected,
    Rest { status: RestStatus },
    Stopped,
    Error { reason: String },
}

/// Public endpoint and database identity for one configured session.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SessionInfo {
    pub endpoint: String,
    pub database: PathBuf,
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
        if !matches!(request, ControlRequest::Stop) {
            Self::ensure_daemon().await?;
        }
        Self::request_existing(request).await
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
            Ok(_) => return Err(invalid_data("daemon returned an unexpected Ping response")),
            Err(error) if daemon_unavailable(&error) => {}
            Err(error) => return Err(error),
        }

        spawn_daemon()?;
        let deadline = Instant::now() + DAEMON_START_TIMEOUT;
        loop {
            match Self::request_existing(ControlRequest::Ping).await {
                Ok(ControlResponse::Pong) => return Ok(()),
                Ok(_) => {
                    return Err(invalid_data("daemon returned an unexpected Ping response"));
                }
                Err(error) if daemon_unavailable(&error) => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "daemon did not become available within ten seconds",
                ));
            }
            tokio::time::sleep(DAEMON_POLL_INTERVAL).await;
        }
    }
}

async fn handle_client(mut stream: Stream, calls: mpsc::Sender<ControlCall>) -> io::Result<()> {
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
    match tokio::time::timeout(
        IO_TIMEOUT,
        calls.send(ControlCall {
            request,
            response: response_tx,
        }),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            return write_frame(
                &mut stream,
                &ControlResponse::Error {
                    reason: "daemon control loop stopped".to_owned(),
                },
            )
            .await;
        }
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
        Ok(Err(_)) => ControlResponse::Error {
            reason: "daemon did not return a control response".to_owned(),
        },
        Err(_) => ControlResponse::Error {
            reason: "daemon control response timed out".to_owned(),
        },
    };
    write_frame(&mut stream, &response).await
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
    tokio::time::timeout(IO_TIMEOUT, stream.write_all(&encoded))
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "control response write timed out")
        })??;
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

fn daemon_unavailable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::AddrNotAvailable
    )
}

fn spawn_daemon() -> io::Result<()> {
    use std::process::{Command, Stdio};

    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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
    drop(command.spawn()?);
    Ok(())
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
    fn control_protocol_is_tagged_and_rest_status_has_no_token_field() {
        let request = ControlRequest::RestEnable {
            endpoint: "tunnel://192.0.2.1:3671".into(),
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
