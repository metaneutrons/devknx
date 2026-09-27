// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Current-user local IPC for an independent capture process.
//!
//! Unix sockets live below a private directory beside the capture database,
//! or below a short private directory in `/tmp` if the adjacent path exceeds
//! the portable Unix-domain socket path limit.
//! Windows named pipes use a protected DACL for the current process SID. The
//! first pipe instance must be created by this server; a pre-existing instance
//! makes startup fail rather than silently accepting an impostor endpoint.

#[cfg(unix)]
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use interprocess::local_socket::{
    ListenerOptions,
    tokio::{Listener, Stream, prelude::*},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Semaphore, broadcast, watch};

use crate::service::{ConnectionState, LiveCapture};

const MAX_MESSAGE_SIZE: usize = 16 * 1024;
const MAX_CLIENTS: usize = 32;

/// Failure to connect, parse, or exchange a local protocol message.
#[derive(Debug, Error)]
pub enum IpcError {
    /// The OS rejected a local socket operation.
    #[error("local IPC error: {0}")]
    Io(#[from] io::Error),
    /// A peer sent invalid JSON.
    #[error("invalid local IPC message: {0}")]
    Json(#[from] serde_json::Error),
    /// A peer exceeded the fixed message limit.
    #[error("local IPC message exceeds {MAX_MESSAGE_SIZE} bytes")]
    MessageTooLarge,
}

/// Version-one local protocol record. A lag count is application fan-out loss,
/// not a KNX bus packet-loss measurement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcMessage {
    /// Connection lifecycle state.
    State { value: WireState },
    /// One committed or ephemeral capture event.
    Capture {
        /// SQLite event ID when capture is persistent.
        id: Option<i64>,
        /// Wall-clock receive timestamp.
        observed_at_ms: u64,
        /// Tunnel or router endpoint URL.
        endpoint: String,
        /// Direction relative to this application.
        direction: String,
        /// KNX individual source address.
        source: String,
        /// KNX destination address.
        destination: String,
        /// Decoded group-value operation or `Other`.
        service: String,
        /// Exact raw cEMI frame, lowercase hexadecimal.
        raw_cemi: String,
    },
    /// Number of application events missed by this IPC subscriber.
    Lagged { count: u64 },
}

/// Serializable representation of [`ConnectionState`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WireState {
    /// No connection attempt has started.
    Idle,
    /// An attempt is in progress.
    Connecting { attempt: u64 },
    /// Receiving from the endpoint.
    Connected { endpoint: String },
    /// Waiting for another attempt.
    WaitingRetry { reason: String, delay_ms: u64 },
    /// Shutdown requested.
    Stopped,
    /// Durable capture failed.
    StorageFailed { reason: String },
}

impl From<&ConnectionState> for WireState {
    fn from(state: &ConnectionState) -> Self {
        match state {
            ConnectionState::Idle => Self::Idle,
            ConnectionState::Connecting { attempt } => Self::Connecting { attempt: *attempt },
            ConnectionState::Connected { endpoint } => Self::Connected {
                endpoint: endpoint.to_string(),
            },
            ConnectionState::WaitingRetry { reason, delay } => Self::WaitingRetry {
                reason: reason.clone(),
                delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            },
            ConnectionState::Stopped => Self::Stopped,
            ConnectionState::StorageFailed { reason } => Self::StorageFailed {
                reason: reason.clone(),
            },
        }
    }
}

impl From<&LiveCapture> for IpcMessage {
    fn from(capture: &LiveCapture) -> Self {
        let event = &capture.event;
        let frame = event.frame();
        let observed_at_ms = event
            .observed_at()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok())
            .unwrap_or(0);
        Self::Capture {
            id: capture.id,
            observed_at_ms,
            endpoint: event.endpoint().to_string(),
            direction: event.direction().to_string(),
            source: frame.source_address().to_string(),
            destination: frame.destination_address().to_string(),
            service: format!("{:?}", event.group_service()),
            raw_cemi: hex(frame.as_bytes()),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(result, "{byte:02x}").expect("writing to String cannot fail");
    }
    result
}

#[cfg(unix)]
fn socket_path(database: &Path, create: bool) -> io::Result<PathBuf> {
    use std::hash::{Hash as _, Hasher as _};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};

    let database = database.canonicalize()?;
    let mut directory_name = database
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing database filename"))?
        .to_os_string();
    directory_name.push(".ipc");
    let adjacent = database.with_file_name(directory_name);
    // Darwin has a shorter sun_path than Linux. Stay below both limits, including
    // the terminating NUL, instead of letting the bind fail on ordinary TMPDIRs.
    let directory = if adjacent.join("control.sock").as_os_str().as_bytes().len() < 100 {
        adjacent
    } else {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        database.hash(&mut hasher);
        Path::new("/tmp").join(format!("devknx-ipc-{:016x}", hasher.finish()))
    };
    if create {
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let metadata = std::fs::symlink_metadata(&directory)?;
    if !metadata.file_type().is_dir()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.uid() != database.metadata()?.uid()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "local IPC directory must be private, owned by the database owner, and not a symlink",
        ));
    }
    Ok(directory.join("control.sock"))
}

#[cfg(windows)]
fn pipe_name(database: &Path) -> io::Result<String> {
    use std::hash::{Hash, Hasher};

    let sid = windows_permissions::utilities::current_process_sid()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    database.canonicalize()?.hash(&mut hasher);
    Ok(format!("devknx-{sid}-{:016x}", hasher.finish()))
}

/// One authenticated local listener owned by the capture process.
pub struct IpcServer {
    listener: Listener,
    #[cfg(unix)]
    _owner_lease: File,
}

#[cfg(unix)]
fn acquire_ipc_lease(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let lease_path = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing IPC directory"))?
        .join("owner.lock");
    match std::fs::symlink_metadata(&lease_path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPC owner lock is not a regular file",
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
            "local IPC listener is already active",
        )),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

impl IpcServer {
    /// Bind the endpoint for a committed capture database.
    ///
    /// # Errors
    ///
    /// Fails closed if the endpoint cannot be protected or is already active.
    pub fn bind(database: &Path) -> Result<Self, IpcError> {
        #[cfg(unix)]
        let (listener, owner_lease) = {
            use std::os::unix::fs::PermissionsExt as _;

            let path = socket_path(database, true)?;
            let owner_lease = acquire_ipc_lease(&path)?;
            let name = path.as_os_str().to_fs_name::<GenericFilePath>()?;
            let options = ListenerOptions::new().name(name).try_overwrite(true);
            #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd"))]
            let options = {
                use interprocess::os::unix::local_socket::ListenerOptionsExt as _;
                options.mode(0o600)
            };
            let listener = options.create_tokio()?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            (listener, owner_lease)
        };
        #[cfg(windows)]
        let listener = {
            use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
            use interprocess::os::windows::security_descriptor::SecurityDescriptor;
            use widestring::U16CString;

            let name = pipe_name(database)?;
            let sid = windows_permissions::utilities::current_process_sid()?;
            let sddl = format!("D:P(A;;GA;;;{sid})");
            let wide = U16CString::from_str(&sddl)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
            let descriptor = SecurityDescriptor::deserialize(&wide)?;
            ListenerOptions::new()
                .name(name.to_ns_name::<GenericNamespaced>()?)
                .security_descriptor(descriptor)
                .create_tokio()?
        };
        Ok(Self {
            listener,
            #[cfg(unix)]
            _owner_lease: owner_lease,
        })
    }

    /// Serve concurrent status and live-subscription clients.
    ///
    /// # Errors
    ///
    /// Returns a listener error; per-client protocol errors affect only that client.
    pub async fn run(
        self,
        state: watch::Receiver<ConnectionState>,
        frames: broadcast::Receiver<LiveCapture>,
    ) -> Result<(), IpcError> {
        let slots = Arc::new(Semaphore::new(MAX_CLIENTS));
        loop {
            let stream = self.listener.accept().await?;
            let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
                drop(stream);
                continue;
            };
            let state = state.clone();
            let frames = frames.resubscribe();
            tokio::spawn(async move {
                let _permit = permit;
                let _ = handle_client(stream, state, frames).await;
            });
        }
    }
}

async fn handle_client(
    mut stream: Stream,
    mut state: watch::Receiver<ConnectionState>,
    mut frames: broadcast::Receiver<LiveCapture>,
) -> Result<(), IpcError> {
    let mut command = [0_u8; 7];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_exact(&mut command),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "IPC command timed out"))??;
    if &command != b"STATUS\n" && &command != b"FOLLOW\n" {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "unknown IPC command").into());
    }
    let initial = IpcMessage::State {
        value: WireState::from(&*state.borrow_and_update()),
    };
    write_message(&mut stream, &initial).await?;
    if &command == b"STATUS\n" {
        return Ok(());
    }
    loop {
        tokio::select! {
            changed = state.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let message = IpcMessage::State {
                    value: WireState::from(&*state.borrow_and_update()),
                };
                write_message(&mut stream, &message).await?;
            }
            result = frames.recv() => {
                let message = match result {
                    Ok(capture) => IpcMessage::from(&capture),
                    Err(broadcast::error::RecvError::Lagged(count)) => IpcMessage::Lagged {count},
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                };
                write_message(&mut stream, &message).await?;
            }
        }
    }
}

async fn write_message(stream: &mut Stream, message: &IpcMessage) -> Result<(), IpcError> {
    let mut encoded = serde_json::to_vec(message)?;
    if encoded.len() >= MAX_MESSAGE_SIZE {
        return Err(IpcError::MessageTooLarge);
    }
    encoded.push(b'\n');
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.write_all(&encoded),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "IPC subscriber stalled"))??;
    Ok(())
}

/// One status request or live stream from the capture owner.
pub struct IpcClient {
    reader: BufReader<Stream>,
}

impl IpcClient {
    /// Request one current state or subscribe to future records.
    ///
    /// # Errors
    ///
    /// Returns an error if there is no owner or the endpoint rejects the client.
    pub async fn connect(database: &Path, follow: bool) -> Result<Self, IpcError> {
        #[cfg(unix)]
        let mut stream = {
            let path = socket_path(database, false)?;
            Stream::connect(path.as_os_str().to_fs_name::<GenericFilePath>()?).await?
        };
        #[cfg(windows)]
        let mut stream = {
            let name = pipe_name(database)?;
            Stream::connect(name.to_ns_name::<GenericNamespaced>()?).await?
        };
        stream
            .write_all(if follow { b"FOLLOW\n" } else { b"STATUS\n" })
            .await?;
        Ok(Self {
            reader: BufReader::new(stream),
        })
    }

    /// Read the next bounded JSON record, or `None` after graceful EOF.
    ///
    /// # Errors
    ///
    /// Returns a protocol, I/O, or malformed-JSON error.
    pub async fn next(&mut self) -> Result<Option<IpcMessage>, IpcError> {
        let mut line = Vec::new();
        loop {
            match self.reader.read_u8().await {
                Ok(b'\n') => return Ok(Some(serde_json::from_slice(&line)?)),
                Ok(byte) if line.len() < MAX_MESSAGE_SIZE => line.push(byte),
                Ok(_) => return Err(IpcError::MessageTooLarge),
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && line.is_empty() => {
                    return Ok(None);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::CaptureStore;
    use std::num::NonZeroU32;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    #[tokio::test]
    async fn local_ipc_has_only_one_active_listener() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("captures.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap());
        let first = IpcServer::bind(&database).unwrap();
        assert!(IpcServer::bind(&database).is_err());
        drop(first);
        assert!(IpcServer::bind(&database).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn local_ipc_rejects_shared_or_symlink_directories() {
        let directory = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let database = directory.path().join("captures.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap());
        let ipc_directory = directory.path().join("captures.sqlite.ipc");
        std::fs::create_dir(&ipc_directory).unwrap();
        std::fs::set_permissions(&ipc_directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            IpcServer::bind(&database),
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied
        ));
        std::fs::remove_dir(&ipc_directory).unwrap();
        symlink(directory.path(), &ipc_directory).unwrap();
        assert!(matches!(
            IpcServer::bind(&database),
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied
        ));
    }

    #[cfg(unix)]
    #[test]
    fn local_ipc_rejects_symlink_owner_lock() {
        let directory = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let database = directory.path().join("captures.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap());
        let ipc_directory = directory.path().join("captures.sqlite.ipc");
        std::fs::create_dir(&ipc_directory).unwrap();
        std::fs::set_permissions(&ipc_directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let victim = directory.path().join("victim");
        std::fs::write(&victim, b"unchanged").unwrap();
        symlink(&victim, ipc_directory.join("owner.lock")).unwrap();
        assert!(matches!(
            IpcServer::bind(&database),
            Err(IpcError::Io(error)) if error.kind() == io::ErrorKind::InvalidInput
        ));
        assert_eq!(std::fs::read(&victim).unwrap(), b"unchanged");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn long_database_path_uses_short_private_ipc_directory() {
        use std::os::unix::ffi::OsStrExt as _;

        let directory = tempfile::tempdir().unwrap();
        let long_directory = directory.path().join("long-path-component-".repeat(5));
        std::fs::create_dir(&long_directory).unwrap();
        let database = long_directory.join("captures.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap());

        let path = socket_path(&database, true).unwrap();
        assert_eq!(path.parent().unwrap().parent(), Some(Path::new("/tmp")));
        assert!(
            path.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .as_bytes()
                .starts_with(b"devknx-ipc-")
        );
        assert!(path.as_os_str().as_bytes().len() < 100);
        assert_eq!(path, socket_path(&database, false).unwrap());
        assert!(IpcServer::bind(&database).is_ok());

        let ipc_directory = path.parent().unwrap();
        std::fs::set_permissions(ipc_directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            socket_path(&database, false),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied
        ));
        std::fs::set_permissions(ipc_directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(ipc_directory).unwrap();
    }
}
