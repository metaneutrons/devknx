// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Bounded Windows named-pipe client connection.
//!
//! The interprocess Tokio local-socket client defaults to an unbounded
//! blocking connect worker when a pipe does not exist. Cancelling its async
//! wrapper leaves that worker alive and can prevent process shutdown.

use std::io;
use std::time::{Duration, Instant};

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

const PIPE_BUSY: i32 = 231;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// Open a local named pipe without creating an unbounded background waiter.
///
/// A missing pipe returns immediately so an on-demand daemon can be spawned.
/// A pipe with all instances busy is retried for at most five seconds.
///
/// # Errors
///
/// Returns a Windows pipe-open error when the listener is unavailable or busy.
pub async fn connect(name: &str) -> io::Result<NamedPipeClient> {
    let path = format!(r"\\.\pipe\{name}");
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match ClientOptions::new().open(std::ffi::OsStr::new(path.as_str())) {
            Ok(client) => return Ok(client),
            Err(error) if error.raw_os_error() == Some(PIPE_BUSY) && Instant::now() < deadline => {
                tokio::time::sleep(RETRY_INTERVAL).await;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn absent_pipe_returns_without_an_unbounded_waiter() {
        let name = format!(
            "devknx-absent-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("current clock")
                .as_nanos()
        );
        let result = tokio::time::timeout(Duration::from_secs(1), connect(&name))
            .await
            .expect("missing pipe connection must finish promptly");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}
